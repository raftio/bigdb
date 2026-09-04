// Copyright 2026 Bany
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

package bigdb

import (
	"bufio"
	"context"
	"crypto/tls"
	"crypto/x509"
	"encoding/base64"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"strconv"
	"strings"
	"sync"
	"time"
)

// One connection to `big serve`, held open across requests.
//
// # Why this is not net/http
//
// See the doc on Doer. The short of it: net/http knows whether the request reached the wire and
// discards that fact before returning, and this client's whole retry story rests on it.
//
// # The idle-close race, and why it is avoided rather than handled
//
// A keep-alive connection can be closed by the server at the very moment a client writes its
// next request onto it. The write succeeds - it goes into a socket buffer - and the read then
// ends at once, having read nothing. A client cannot tell that apart from a server that read
// the request, ran it, and died before answering. For an idempotent request the difference does
// not matter; for an allocating INSERT it is the difference between recovering a batch and
// writing it twice.
//
// So this file does not try to tell them apart. It makes the race unreachable instead, by
// replacing the connection well before the server would: after MaxRequestsPerConn requests and
// after MaxIdle of quiet. A connection retired by the client is retired between requests, where
// a fresh connect is provably safe.
//
// This is contrib/big-message/src/http.rs, ported.

// The reader's own ceilings, taken from the server's (big_http::request) so that this client is
// no softer than the thing it talks to.
const (
	maxLine        = 8 << 10
	maxHeaders     = 64
	maxHeaderBytes = 16 << 10
	// maxBodyRead bounds a response body. The server's own MAX_BODY is 8 MiB on the way in; a
	// listing can be larger on the way out, so this is generous rather than tight. It exists so
	// that a Content-Length of 2^63 is refused rather than attempted.
	maxBodyRead = 256 << 20
)

// conn is the default Doer: one socket, reused, retired on this client's schedule.
type conn struct {
	addr       Address
	credential string // "user:password", already joined - the halves have no other use here
	timeout    time.Duration
	userAgent  string
	tlsConfig  *tls.Config

	mu   sync.Mutex
	open *openConn
}

type openConn struct {
	net    net.Conn
	reader *bufio.Reader
	// sent is how many requests have gone down this one.
	sent int
	// last is when the last response finished, which MaxIdle is measured from.
	last time.Time
}

func newConn(addr Address, cfg config) (*conn, error) {
	c := &conn{
		addr:      addr,
		timeout:   cfg.timeout,
		userAgent: cfg.userAgent,
	}
	if cfg.user != "" || cfg.password != "" {
		c.credential = cfg.user + ":" + cfg.password
	}

	if !addr.TLS {
		// A CA file on a plaintext address is a caller who thinks they are encrypted and is
		// not. Saying so is the whole value of the check.
		if cfg.caFile != "" {
			return nil, &ConfigError{What: "a CA file was given for " + addr.Dial() +
				", which is not an https address: this connection would not be encrypted"}
		}
		return c, nil
	}

	t := &tls.Config{
		// The host with its port removed. A certificate is issued to a name, and
		// "example:7654" is not a name.
		ServerName:         addr.Host,
		InsecureSkipVerify: cfg.insecureSkipVerify, //nolint:gosec // the option is named for what it is
		MinVersion:         tls.VersionTLS12,
	}
	if cfg.caFile != "" {
		pem, err := os.ReadFile(cfg.caFile)
		if err != nil {
			return nil, &ConfigError{What: "could not read the CA file: " + err.Error()}
		}
		pool := x509.NewCertPool()
		if !pool.AppendCertsFromPEM(pem) {
			return nil, &ConfigError{What: cfg.caFile + " holds no certificates this client can read"}
		}
		t.RootCAs = pool
	}
	c.tlsConfig = t
	return c, nil
}

// Do sends one request and reads the whole answer.
//
// body is sent verbatim. This never inspects it: a statement is bytes on their way to the only
// thing that understands them, and a client that checked SQL would be a second parser to keep
// in step with the first.
func (c *conn) Do(ctx context.Context, method, target string, body []byte) (*RawResponse, error) {
	c.mu.Lock()
	defer c.mu.Unlock()

	fail := func(sent bool, err error) (*RawResponse, error) {
		c.closeOpen()
		return nil, &TransportError{Sent: sent, Op: method, Target: target, Err: err}
	}

	// Whose deadline this is matters, and not only for the message. The socket deadline and the
	// context's timer are two independent clocks armed for the same instant, so either can fire
	// first - and when the socket wins, ctx.Err() is still nil and the failure looks like an
	// ordinary I/O timeout. On an idempotent operation that would be retried, past a deadline
	// the caller had already set. So the origin is carried and the error is reported as the
	// caller's cancellation whichever clock happened to win.
	deadline, deadlineFromCtx := ctx.Deadline()
	hasDeadline := deadlineFromCtx
	if !hasDeadline && c.timeout > 0 {
		deadline = time.Now().Add(c.timeout)
		hasDeadline = true
	}

	c.retireIfStale()

	if c.open == nil {
		o, err := c.dial(ctx, deadline, hasDeadline)
		if err != nil {
			// Nothing was written because nothing was opened.
			return fail(false, err)
		}
		c.open = o
	}
	o := c.open

	// A cancelled context has to actually interrupt a blocked read, and a deadline alone will
	// not do that. Closing the socket underneath the read is what does; the read then returns
	// an error and the ctx.Err() below turns it into the right one.
	stop := watch(ctx, o.net)
	defer stop()

	if hasDeadline {
		if err := o.net.SetDeadline(deadline); err != nil {
			return fail(false, err)
		}
	}

	req := c.request(method, target, body)

	// Everything up to here is not-sent. A write that fails part way leaves the server holding
	// fewer bytes than Content-Length promised, which it cannot parse as a statement and will
	// never run.
	if _, err := o.net.Write(req); err != nil {
		return fail(false, ctxErr(ctx, err, deadlineFromCtx))
	}

	// Everything from here is unknown. The request is on the wire in full.
	resp, err := readResponse(o.reader)
	if err != nil {
		c.closeOpen()
		if pe, ok := err.(*ProtocolError); ok {
			return nil, pe
		}
		return nil, &TransportError{Sent: true, Op: method, Target: target, Err: ctxErr(ctx, err, deadlineFromCtx)}
	}

	o.sent++
	o.last = time.Now()
	if resp.Closing() || o.sent >= MaxRequestsPerConn {
		c.closeOpen()
	}
	return resp, nil
}

// Close releases the connection, if one is open.
func (c *conn) Close() error {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.closeOpen()
	return nil
}

func (c *conn) closeOpen() {
	if c.open != nil {
		_ = c.open.net.Close()
		c.open = nil
	}
}

// retireIfStale drops a connection the server may be about to close, before a request is
// written onto it. This is the whole of the race avoidance: the check happens where a fresh
// connect is provably safe, which is between requests and never during one.
func (c *conn) retireIfStale() {
	if c.open == nil {
		return
	}
	if c.open.sent >= MaxRequestsPerConn || time.Since(c.open.last) >= MaxIdle {
		c.closeOpen()
	}
}

func (c *conn) dial(ctx context.Context, deadline time.Time, hasDeadline bool) (*openConn, error) {
	d := net.Dialer{}
	if hasDeadline {
		d.Deadline = deadline
	}
	raw, err := d.DialContext(ctx, "tcp", c.addr.Dial())
	if err != nil {
		return nil, err
	}
	// Nagle off: a request is written in one Write and then the client waits for an answer, so
	// there is never a second small write to coalesce with - only a delay to add.
	if tcp, ok := raw.(*net.TCPConn); ok {
		_ = tcp.SetNoDelay(true)
	}

	out := net.Conn(raw)
	if c.tlsConfig != nil {
		tc := tls.Client(raw, c.tlsConfig)
		if hasDeadline {
			_ = tc.SetDeadline(deadline)
		}
		if err := tc.HandshakeContext(ctx); err != nil {
			_ = raw.Close()
			return nil, err
		}
		out = tc
	}
	// The reader is per connection, not per request: bytes of a later response are already in
	// this buffer, and building a fresh reader each time would drop them - silently, and only
	// under load.
	return &openConn{net: out, reader: bufio.NewReader(out), last: time.Now()}, nil
}

func (c *conn) request(method, target string, body []byte) []byte {
	var b strings.Builder
	b.Grow(len(body) + 256)
	b.WriteString(method)
	b.WriteByte(' ')
	b.WriteString(target)
	b.WriteString(" HTTP/1.1\r\nHost: ")
	b.WriteString(c.addr.HostHeader())
	b.WriteString("\r\nUser-Agent: ")
	b.WriteString(c.userAgent)
	b.WriteString("\r\n")
	if c.credential != "" {
		b.WriteString("Authorization: Basic ")
		b.WriteString(base64.StdEncoding.EncodeToString([]byte(c.credential)))
		b.WriteString("\r\n")
	}
	b.WriteString("Content-Length: ")
	b.WriteString(strconv.Itoa(len(body)))
	// Sent explicitly, because big_http::Request::wants_keep_alive is opt-in and not the
	// HTTP/1.1 default here: a client that says nothing gets one request and a close. Leaving
	// this header out costs a TCP handshake per request, invisibly.
	b.WriteString("\r\nConnection: keep-alive\r\n\r\n")
	b.Write(body)
	return []byte(b.String())
}

// readResponse reads the status line, the headers, and exactly as many body bytes as were
// promised.
func readResponse(r *bufio.Reader) (*RawResponse, error) {
	line, err := readLine(r)
	if err != nil {
		return nil, err
	}
	if line == "" {
		return nil, io.EOF
	}

	// "HTTP/1.1 200 OK"
	parts := strings.SplitN(line, " ", 3)
	if len(parts) < 2 || !strings.HasPrefix(parts[0], "HTTP/") {
		return nil, &ProtocolError{What: "not an HTTP status line: " + strconv.Quote(line)}
	}
	status, err := strconv.Atoi(parts[1])
	if err != nil || status < 100 || status > 599 {
		return nil, &ProtocolError{What: "not an HTTP status line: " + strconv.Quote(line)}
	}

	header := make(http.Header)
	length := -1
	total := 0
	for n := 0; ; n++ {
		if n >= maxHeaders {
			return nil, &ProtocolError{What: "the response has more headers than this client reads"}
		}
		h, err := readLine(r)
		if err != nil {
			return nil, err
		}
		if h == "" {
			break
		}
		total += len(h)
		if total > maxHeaderBytes {
			return nil, &ProtocolError{What: "the response headers are larger than this client reads"}
		}
		name, value, ok := strings.Cut(h, ":")
		if !ok {
			return nil, &ProtocolError{What: "not a header: " + strconv.Quote(h)}
		}
		name, value = strings.TrimSpace(name), strings.TrimSpace(value)

		if strings.EqualFold(name, "transfer-encoding") {
			// Refused rather than decoded. The server always writes Content-Length
			// (big_http::response::encode), so a Transfer-Encoding means something is on the
			// path that this client did not dial - a proxy, most likely - and saying so is
			// better than quietly de-chunking and carrying on as if the topology were what the
			// caller believed.
			return nil, &ProtocolError{What: "the answer is chunked, which this server does not " +
				"send: something is proxying this connection"}
		}
		if strings.EqualFold(name, "content-length") && length < 0 {
			length, err = strconv.Atoi(value)
			if err != nil || length < 0 {
				return nil, &ProtocolError{What: "Content-Length is not a length: " + strconv.Quote(value)}
			}
		}
		// Add rather than Set, so a repeated header keeps its order and Get stays first-wins -
		// which is what the server does with the ones it reads.
		header.Add(name, value)
	}

	if length < 0 {
		return nil, &ProtocolError{What: "the answer has no Content-Length, which this server always sends"}
	}
	if length > maxBodyRead {
		return nil, &ProtocolError{What: "the answer declares " + strconv.Itoa(length) +
			" bytes, more than this client reads"}
	}

	body := make([]byte, length)
	// ReadFull is the Go spelling of read_exact: a connection that dies mid-body is an error,
	// not a shorter answer. It also leaves the socket at a request boundary when it succeeds,
	// which is what makes reusing it safe.
	if _, err := io.ReadFull(r, body); err != nil {
		if err == io.ErrUnexpectedEOF || err == io.EOF {
			return nil, io.ErrUnexpectedEOF
		}
		return nil, err
	}
	return &RawResponse{Status: status, Header: header, Body: body}, nil
}

// readLine reads one CRLF-terminated line, without the terminator.
func readLine(r *bufio.Reader) (string, error) {
	line, err := r.ReadString('\n')
	if err != nil {
		if err == io.EOF && line == "" {
			return "", io.EOF
		}
		if err == io.EOF {
			return "", io.ErrUnexpectedEOF
		}
		return "", err
	}
	if len(line) > maxLine {
		return "", &ProtocolError{What: "a response line is longer than this client reads"}
	}
	return strings.TrimRight(line, "\r\n"), nil
}

// watch closes the connection when the context is done, so a cancellation interrupts a read
// that is already blocked. The returned function stops the watcher.
func watch(ctx context.Context, c net.Conn) func() {
	if ctx.Done() == nil {
		return func() {}
	}
	done := make(chan struct{})
	go func() {
		select {
		case <-ctx.Done():
			_ = c.Close()
		case <-done:
		}
	}()
	return func() { close(done) }
}

// ctxErr prefers the context's reason over the socket's.
//
// A read that failed because the deadline passed reports "i/o timeout", which is true and
// unhelpful; context.DeadlineExceeded is what the caller set and what they can act on. It also
// matters for retry: decide() never retries a cancellation, and it can only see one if it
// survives to here.
//
// The second clause is the race. When the deadline on the socket came from the context, a
// socket timeout *is* the context's deadline - the two are the same instant armed on two
// clocks, and which one fires first is not something the caller should be able to observe.
// Without this the same event is a cancellation sometimes and a retryable I/O failure other
// times.
func ctxErr(ctx context.Context, err error, deadlineFromCtx bool) error {
	if e := ctx.Err(); e != nil {
		return fmt.Errorf("%w (%v)", e, err)
	}
	if deadlineFromCtx && errors.Is(err, os.ErrDeadlineExceeded) {
		return fmt.Errorf("%w (%v)", context.DeadlineExceeded, err)
	}
	return err
}
