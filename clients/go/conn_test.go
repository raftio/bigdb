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
	"errors"
	"fmt"
	"io"
	"net"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

// A fake server that replays raw bytes.
//
// Deliberately not httptest.Server: that wraps net/http, which will not emit a Content-Length
// that lies, or a response cut off mid status line. Those are the whole of what needs testing
// here, so the listener is raw.

type fake struct {
	t  *testing.T
	ln net.Listener

	mu sync.Mutex
	// requests is every request byte-block the server read, in order.
	requests []string
	// conns is how many connections were accepted.
	conns int
	// maxConns, when positive, makes the server accept exactly that many and no more. This is
	// the port of `stocked(1)` in contrib/big-message/tests/producer.rs: it is what turns
	// keep-alive from a coincidence into a proof.
	maxConns int
}

// handler decides what to write back for request number n on this connection.
type handler func(n int, req string) (reply string, keepOpen bool)

func serve(t *testing.T, maxConns int, h handler) *fake {
	t.Helper()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	f := &fake{t: t, ln: ln, maxConns: maxConns}
	go f.loop(h)
	t.Cleanup(func() { _ = ln.Close() })
	return f
}

func (f *fake) addr() string { return f.ln.Addr().String() }

func (f *fake) loop(h handler) {
	for {
		c, err := f.ln.Accept()
		if err != nil {
			return
		}
		f.mu.Lock()
		f.conns++
		n := f.conns
		f.mu.Unlock()
		if f.maxConns > 0 && n > f.maxConns {
			// Refusing to serve a second connection is the assertion: if the client had
			// reconnected, the exchange would fail here rather than silently succeed.
			_ = c.Close()
			continue
		}
		go f.handle(c, h)
	}
}

func (f *fake) handle(c net.Conn, h handler) {
	defer c.Close()
	r := bufio.NewReader(c)
	for n := 0; ; n++ {
		req, err := readRequest(r)
		if err != nil {
			return
		}
		f.mu.Lock()
		f.requests = append(f.requests, req)
		f.mu.Unlock()

		reply, keepOpen := h(n, req)
		if _, err := c.Write([]byte(reply)); err != nil {
			return
		}
		if !keepOpen {
			return
		}
	}
}

// readRequest reads headers and exactly Content-Length body bytes, returning the whole block.
func readRequest(r *bufio.Reader) (string, error) {
	var b strings.Builder
	length := 0
	for {
		line, err := r.ReadString('\n')
		if err != nil {
			return "", err
		}
		b.WriteString(line)
		trimmed := strings.TrimRight(line, "\r\n")
		if trimmed == "" {
			break
		}
		if name, value, ok := strings.Cut(trimmed, ":"); ok &&
			strings.EqualFold(strings.TrimSpace(name), "content-length") {
			length, _ = strconv.Atoi(strings.TrimSpace(value))
		}
	}
	if length > 0 {
		body := make([]byte, length)
		if _, err := io.ReadFull(r, body); err != nil {
			return "", err
		}
		b.Write(body)
	}
	return b.String(), nil
}

// reply builds a well-formed response.
func reply(status int, body string, extra ...string) string {
	var b strings.Builder
	fmt.Fprintf(&b, "HTTP/1.1 %d %s\r\n", status, http1Reason(status))
	b.WriteString("Content-Type: application/json\r\n")
	fmt.Fprintf(&b, "Content-Length: %d\r\n", len(body))
	for _, h := range extra {
		b.WriteString(h + "\r\n")
	}
	b.WriteString("\r\n")
	b.WriteString(body)
	return b.String()
}

func http1Reason(status int) string {
	switch status {
	case 200:
		return "OK"
	case 404:
		return "Not Found"
	case 503:
		return "Service Unavailable"
	}
	return "Status"
}

func ok(body string) handler {
	return func(int, string) (string, bool) { return reply(200, body), true }
}

func testClient(t *testing.T, f *fake, opts ...Option) *Client {
	t.Helper()
	opts = append([]Option{WithRetries(0), WithTimeout(2 * time.Second)}, opts...)
	c, err := New(f.addr(), opts...)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = c.Close() })
	return c
}

// --- the tests ---

func TestConnSendsKeepAliveExplicitly(t *testing.T) {
	// big_http::Request::wants_keep_alive is opt-in, not the HTTP/1.1 default. A client that
	// leaves the header out gets one request per connection and never finds out.
	f := serve(t, 0, ok(`{"count":1}`))
	c := testClient(t, f)

	if _, err := c.Query(context.Background(), "tx", "Count(All())"); err != nil {
		t.Fatal(err)
	}
	if got := f.requests[0]; !strings.Contains(got, "Connection: keep-alive\r\n") {
		t.Errorf("the request must ask for keep-alive explicitly:\n%s", got)
	}
	if !strings.Contains(f.requests[0], "Content-Length: 12\r\n") {
		t.Errorf("Content-Length must match the body:\n%s", f.requests[0])
	}
}

func TestConnReusesOneConnection(t *testing.T) {
	// The server accepts exactly one connection. If the client reconnected, the second request
	// would fail rather than quietly succeed - which is what makes this a proof and not a
	// coincidence. Ported from `stocked(1)` in contrib/big-message/tests/producer.rs.
	f := serve(t, 1, ok(`{"count":1}`))
	c := testClient(t, f)

	for i := 0; i < 5; i++ {
		if _, err := c.Query(context.Background(), "tx", "Count(All())"); err != nil {
			t.Fatalf("request %d: %v", i, err)
		}
	}
	f.mu.Lock()
	defer f.mu.Unlock()
	if f.conns != 1 {
		t.Errorf("five requests opened %d connections, want 1", f.conns)
	}
	if len(f.requests) != 5 {
		t.Errorf("the server saw %d requests, want 5", len(f.requests))
	}
}

func TestConnRetiresAtTheRequestCeiling(t *testing.T) {
	// The client must retire before the server's 1000, and it must do so between requests.
	f := serve(t, 0, ok(`{"count":1}`))
	c := testClient(t, f)
	conn := c.doer.(*conn)

	// Drive one exchange, then pretend the connection has carried its allowance.
	if _, err := c.Health(context.Background()); err != nil {
		t.Fatal(err)
	}
	conn.mu.Lock()
	conn.open.sent = MaxRequestsPerConn
	conn.mu.Unlock()

	if _, err := c.Health(context.Background()); err != nil {
		t.Fatal(err)
	}
	f.mu.Lock()
	defer f.mu.Unlock()
	if f.conns != 2 {
		t.Errorf("a connection at the ceiling must be replaced; got %d connections", f.conns)
	}
}

func TestConnRetiresAfterIdle(t *testing.T) {
	f := serve(t, 0, ok(`{"count":1}`))
	c := testClient(t, f)
	conn := c.doer.(*conn)

	if _, err := c.Health(context.Background()); err != nil {
		t.Fatal(err)
	}
	// Age the connection past MaxIdle without sleeping for it.
	conn.mu.Lock()
	conn.open.last = time.Now().Add(-MaxIdle - time.Second)
	conn.mu.Unlock()

	if _, err := c.Health(context.Background()); err != nil {
		t.Fatal(err)
	}
	f.mu.Lock()
	defer f.mu.Unlock()
	if f.conns != 2 {
		t.Errorf("an idle connection must be replaced; got %d connections", f.conns)
	}
}

func TestConnRefusesTransferEncoding(t *testing.T) {
	// The server always writes Content-Length, so a Transfer-Encoding means a proxy is on the
	// path. Saying so beats de-chunking and pretending the topology is what the caller thinks.
	f := serve(t, 0, func(int, string) (string, bool) {
		return "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n7\r\n{\"a\":1}\r\n0\r\n\r\n", false
	})
	c := testClient(t, f)

	_, err := c.Health(context.Background())
	var pe *ProtocolError
	if !errors.As(err, &pe) {
		t.Fatalf("want a *ProtocolError, got %#v", err)
	}
	if !errors.Is(err, ErrUnknown) {
		t.Error("a protocol failure leaves the outcome as unknown as a broken read does")
	}
}

func TestConnShortBodyIsUnknownNotNotSent(t *testing.T) {
	// The request was written in full, so what happened at the server is not knowable. This is
	// the case that must never be retried for a non-idempotent operation.
	f := serve(t, 0, func(int, string) (string, bool) {
		return "HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{\"count\":1}", false
	})
	c := testClient(t, f)

	_, err := c.SQL(context.Background(), "SELECT 1")
	if !errors.Is(err, ErrUnknown) {
		t.Fatalf("a body shorter than Content-Length is an unknown outcome, got %#v", err)
	}
	if errors.Is(err, ErrNotSent) {
		t.Error("it must not be reported as never sent")
	}
}

func TestConnCloseBeforeStatusLineIsUnknown(t *testing.T) {
	f := serve(t, 0, func(int, string) (string, bool) { return "", false })
	c := testClient(t, f)

	_, err := c.SQL(context.Background(), "SELECT 1")
	if !errors.Is(err, ErrUnknown) {
		t.Fatalf("a connection closed before the status line is an unknown outcome, got %#v", err)
	}
}

func TestConnRefusedIsNotSent(t *testing.T) {
	// Nothing was opened, so nothing was written. Provably safe to try again.
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	addr := ln.Addr().String()
	_ = ln.Close()

	c, err := New(addr, WithRetries(0), WithTimeout(time.Second))
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()

	_, err = c.SQL(context.Background(), "SELECT 1")
	if !errors.Is(err, ErrNotSent) {
		t.Fatalf("a refused connection was never sent, got %#v", err)
	}
	var te *TransportError
	if errors.As(err, &te) && te.Sent {
		t.Error("Sent must be false")
	}
}

func TestConnRetriesA503AndThenSucceeds(t *testing.T) {
	f := serve(t, 0, func(n int, _ string) (string, bool) {
		if n == 0 {
			return reply(503, `{"error":"not now","code":"server_busy"}`, "Retry-After: 0"), true
		}
		return reply(200, `{"count":7}`), true
	})
	c := testClient(t, f, WithRetries(2), WithRetryDelay(func(int) time.Duration { return 0 }))

	a, err := c.Query(context.Background(), "tx", "Count(All())")
	if err != nil {
		t.Fatal(err)
	}
	got, isCount := a.(*CountAnswer)
	if !isCount || got.Count != 7 {
		t.Fatalf("want a count of 7, got %#v", a)
	}
	if len(f.requests) != 2 {
		t.Errorf("the server saw %d requests, want 2", len(f.requests))
	}
}

func TestConnDoesNotRetryANonIdempotentUnknown(t *testing.T) {
	// The one that matters: /sql is not idempotent, so a request whose outcome is unknown must
	// be reported rather than repeated. urllib3 retries this case; doing so here would silently
	// double an INSERT.
	f := serve(t, 0, func(int, string) (string, bool) { return "", false })
	c := testClient(t, f, WithRetries(5), WithRetryDelay(func(int) time.Duration { return 0 }))

	if _, err := c.SQL(context.Background(), "INSERT INTO tx (amount) VALUES (1)"); err == nil {
		t.Fatal("want an error")
	}
	f.mu.Lock()
	defer f.mu.Unlock()
	if len(f.requests) != 1 {
		t.Errorf("the server saw %d requests; a non-idempotent unknown must be sent once", len(f.requests))
	}
}

func TestConnDoesRetryAnIdempotentUnknown(t *testing.T) {
	// /import is idempotent - a fact is a bit at an address the caller chose - so the same
	// failure is worth another try.
	// Counted across connections, not within one: the retry opens a fresh connection, so a
	// per-connection counter would hand it the same failure forever.
	var seen atomic.Int32
	f := serve(t, 0, func(int, string) (string, bool) {
		if seen.Add(1) == 1 {
			return "", false
		}
		return reply(200, `{"imported":1}`), true
	})
	c := testClient(t, f, WithRetries(3), WithRetryDelay(func(int) time.Duration { return 0 }))

	r, err := c.Import(context.Background(), "tx", []Fact{{Field: "amount", Record: 0, Value: 1250}})
	if err != nil {
		t.Fatal(err)
	}
	if r.Count != 1 {
		t.Errorf("imported = %d, want 1", r.Count)
	}
}

func TestConnCancellationIsNotAWriteFailure(t *testing.T) {
	// A cancelled context must surface as a cancellation, not as "never sent" - which would
	// invite a retry the caller explicitly asked not to happen.
	f := serve(t, 0, func(int, string) (string, bool) {
		time.Sleep(2 * time.Second)
		return reply(200, `{}`), false
	})
	c := testClient(t, f, WithTimeout(10*time.Second))

	ctx, cancel := context.WithTimeout(context.Background(), 80*time.Millisecond)
	defer cancel()

	_, err := c.SQL(ctx, "SELECT 1")
	if !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("want context.DeadlineExceeded, got %#v", err)
	}
}

func TestConnSendsBasicAuthTheWayTheServerReadsIt(t *testing.T) {
	// Same vector as contrib/big-message/src/http.rs::base64_tests.
	f := serve(t, 0, ok(`{}`))
	c := testClient(t, f, WithUser("alice"), WithPassword("s3cret"))

	if _, err := c.Health(context.Background()); err != nil {
		t.Fatal(err)
	}
	if want := "Authorization: Basic YWxpY2U6czNjcmV0\r\n"; !strings.Contains(f.requests[0], want) {
		t.Errorf("want %q in:\n%s", want, f.requests[0])
	}
}

func TestConnEscapesTheTableInThePath(t *testing.T) {
	// The server's own decode tests use these two. `+` in particular must arrive as %2B: the
	// server deliberately does not read `+` as a space.
	f := serve(t, 0, ok(`{"records":[],"next":null}`))
	c := testClient(t, f)

	if _, err := c.Records(context.Background(), "báo cáo"); err != nil {
		t.Fatal(err)
	}
	if want := "GET /table/b%C3%A1o%20c%C3%A1o/records"; !strings.HasPrefix(f.requests[0], want) {
		t.Errorf("want a request starting %q, got:\n%s", want, f.requests[0])
	}
}

func TestATimedOutIdempotentCallIsNotRetriedPastTheDeadline(t *testing.T) {
	// The half of the deadline race that is not about the message. /records is idempotent, so a
	// failure whose outcome is unknown would ordinarily be retried - but the caller's deadline
	// has passed, and spending another attempt after they have given up is the one thing a
	// deadline exists to prevent. This used to happen whenever the socket's clock beat the
	// context's, which was often.
	var seen atomic.Int32
	f := serve(t, 0, func(int, string) (string, bool) {
		seen.Add(1)
		time.Sleep(2 * time.Second)
		return reply(200, `{"records":[],"next":null}`), false
	})
	c := testClient(t, f,
		WithRetries(5), WithRetryDelay(func(int) time.Duration { return 0 }),
		WithTimeout(10*time.Second))

	ctx, cancel := context.WithTimeout(context.Background(), 80*time.Millisecond)
	defer cancel()

	start := time.Now()
	if _, err := c.Records(ctx, "tx"); err == nil {
		t.Fatal("want a failure")
	}
	if elapsed := time.Since(start); elapsed > time.Second {
		t.Errorf("the call took %v; it must give up when the deadline passes", elapsed)
	}
	if n := seen.Load(); n != 1 {
		t.Errorf("the server saw %d requests; a passed deadline must not buy another attempt", n)
	}
}
