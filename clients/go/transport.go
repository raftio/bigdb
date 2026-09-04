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
	"context"
	"net/http"
	"strconv"
	"strings"
	"time"
)

// This file is the seam. Everything above it - ops, decode, sql, escape, facts, retry - knows
// the protocol and touches no socket. Everything below it - conn.go - moves bytes and knows
// nothing about what they mean.

// A RawResponse is one answer, read but not interpreted.
type RawResponse struct {
	Status int
	// Header uses net/http's canonical keys. Get is first-wins, which is what the server does
	// too (big_http::Request::header: "a request that sends Authorization twice is not one
	// whose second value should be trusted over its first").
	Header http.Header
	Body   []byte
}

// ContentType is the media type with any parameters stripped.
//
// This is how the client decides whether a /sql answer is JSON or text. Never by scanning the
// statement for a FORMAT clause: that would be a second SQL parser to keep in step with
// big_sql, which is exactly what every client in this repository refuses to build.
func (r *RawResponse) ContentType() string {
	ct := r.Header.Get("Content-Type")
	if i := strings.IndexByte(ct, ';'); i >= 0 {
		ct = ct[:i]
	}
	return strings.ToLower(strings.TrimSpace(ct))
}

// RequestID is the server's handle for this exchange, and on a 5xx it is the only handle
// there is: the body is redacted (big_http::status), so correlating against the server's log
// is all that is left.
func (r *RawResponse) RequestID() string { return r.Header.Get("X-Request-Id") }

// RetryAfter is how long the server asked to be left alone. Only the delta-seconds form is
// read; the HTTP-date form is not one this server emits.
func (r *RawResponse) RetryAfter() (time.Duration, bool) {
	v := r.Header.Get("Retry-After")
	if v == "" {
		return 0, false
	}
	n, err := strconv.Atoi(strings.TrimSpace(v))
	if err != nil || n < 0 {
		return 0, false
	}
	return time.Duration(n) * time.Second, true
}

// Closing reports whether the server said it will not take another request on this connection.
func (r *RawResponse) Closing() bool {
	return strings.EqualFold(strings.TrimSpace(r.Header.Get("Connection")), "close")
}

// A Doer performs one request and reads the whole answer.
//
// # What an implementation owes the layer above it
//
// The error classification is the contract, not a detail. A failure before or during the write
// must come back as &TransportError{Sent: false}; a failure from the status line onward as
// &TransportError{Sent: true}. Everything above this seam decides whether to resend on that
// one bit, and a transport that guesses it wrong will duplicate an INSERT.
//
// # Why net/http is not the default implementation
//
// net/http knows the answer and discards it. In transport.go's roundTrip loop, when a request
// is not going to be retried, it unwraps nothingWrittenError and transportReadFromServerError
// down to the underlying error before returning - so what reaches the caller is a *url.Error
// around a *net.OpError or io.EOF, and "nothing was written" is no longer distinguishable from
// "written in full, then silence".
//
// Its own retrying is not the problem and is worth stating fairly: it only ever happens on a
// reused connection, nothingWrittenError is retried when GetBody is set (which is exactly the
// safe case), and both of the ambiguous errors are gated behind isReplayable(), which a POST
// is not. net/http will not duplicate an INSERT. It simply cannot tell you which failure you
// just had, and that is the one thing this client needs to know.
//
// A net/http-backed Doer is about thirty lines and works fine for a caller who needs a proxy
// and accepts coarser classification - report every failure as Sent: true, which is the
// conservative reading, and lose the ability to retry a write that never left the machine.
type Doer interface {
	Do(ctx context.Context, method, target string, body []byte) (*RawResponse, error)
}

// A Closer is a Doer that holds something worth releasing. Client.Close calls it when present.
type doerCloser interface {
	Close() error
}
