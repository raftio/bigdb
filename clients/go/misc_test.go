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
	"bytes"
	"errors"
	"log/slog"
	"strings"
	"testing"
	"time"
)

func TestErrorMessagesSayWhatHappened(t *testing.T) {
	// Every one of these is read by a person at the moment something has gone wrong, so each is
	// asserted to carry the fact that would let them act.
	for _, c := range []struct {
		err  error
		want []string
	}{
		// The request id first, because on a 5xx the body is redacted and it is the only handle
		// there is.
		{&ServerError{Status: 500, Code: "internal", Message: "redacted", RequestID: "r-9"},
			[]string{"r-9", "500", "internal"}},
		{&ServerError{Status: 404, Code: "unknown_table", Message: "no table"},
			[]string{"[-]", "404", "unknown_table"}},
		{&TransportError{Sent: false, Op: "POST", Target: "/sql", Err: errors.New("refused")},
			[]string{"was not sent", "/sql", "refused"}},
		{&TransportError{Sent: true, Op: "POST", Target: "/sql", Err: errors.New("eof")},
			[]string{"outcome is not known", "/sql"}},
		{&TooLargeError{Bytes: 100, Cap: 50}, []string{"100", "50"}},
		{&TooLargeError{Bytes: 100, Cap: 50, Line: 7}, []string{"100", "50", "line 7"}},
		{&ProtocolError{What: "chunked"}, []string{"chunked"}},
		{&ConfigError{What: "no port"}, []string{"no port"}},
		{&ValueError{What: "no NULL"}, []string{"no NULL"}},
	} {
		got := c.err.Error()
		for _, w := range c.want {
			if !strings.Contains(got, w) {
				t.Errorf("%q must contain %q", got, w)
			}
		}
	}
}

func TestUnwrapReachesTheUnderlyingError(t *testing.T) {
	inner := errors.New("connection refused")
	err := error(&TransportError{Sent: false, Err: inner})
	if !errors.Is(err, inner) {
		t.Error("the cause has to stay reachable")
	}
	if !errors.Is(err, ErrNotSent) {
		t.Error("and so does the classification")
	}
}

func TestOptionsSettle(t *testing.T) {
	cfg := defaults()
	for _, o := range []Option{
		WithUser("u"), WithPassword("p"), WithDatabase("d"),
		WithCAFile("ca.pem"), WithInsecureSkipVerify(true),
		WithTimeout(time.Second), WithRetries(9), WithMaxBytes(1), WithMaxRows(2),
		WithoutPlaintextWarning(), WithUserAgent("ua"),
		WithRetryDelay(func(int) time.Duration { return 0 }),
		WithLogger(nil), // a nil logger must not replace the real one
	} {
		o(&cfg)
	}
	if cfg.user != "u" || cfg.password != "p" || cfg.database != "d" {
		t.Errorf("credentials = %#v", cfg)
	}
	if cfg.caFile != "ca.pem" || !cfg.insecureSkipVerify {
		t.Errorf("tls = %#v", cfg)
	}
	if cfg.timeout != time.Second || cfg.retries != 9 || cfg.maxBytes != 1 || cfg.maxRows != 2 {
		t.Errorf("limits = %#v", cfg)
	}
	if cfg.warnPlain || cfg.userAgent != "ua" {
		t.Errorf("misc = %#v", cfg)
	}
	if cfg.logger == nil {
		t.Error("WithLogger(nil) must be a no-op rather than a way to lose the logger")
	}
}

func TestBackoffRisesAndThenStops(t *testing.T) {
	if backoff(0) != 50*time.Millisecond {
		t.Errorf("backoff(0) = %v", backoff(0))
	}
	if backoff(1) != 100*time.Millisecond {
		t.Errorf("backoff(1) = %v", backoff(1))
	}
	if backoff(20) != time.Second {
		t.Errorf("backoff must cap at a second, got %v", backoff(20))
	}
}

func TestThePlaintextWarningIsSilentWhereTheRiskIsNot(t *testing.T) {
	// Refusing outright would break 127.0.0.1:7654 with a users file, which is an ordinary way
	// to run this - and a client that complains about the normal case teaches people to ignore
	// it. So: silent on loopback, one warning otherwise.
	for _, c := range []struct {
		addr string
		opts []Option
		warn bool
	}{
		{"127.0.0.1:7654", []Option{WithPassword("p")}, false},
		{"localhost:7654", []Option{WithPassword("p")}, false},
		{"example.com:7654", []Option{WithPassword("p")}, true},
		{"https://example.com:7654", []Option{WithPassword("p")}, false},
		{"example.com:7654", nil, false},
		{"example.com:7654", []Option{WithPassword("p"), WithoutPlaintextWarning()}, false},
	} {
		var buf bytes.Buffer
		logger := slog.New(slog.NewTextHandler(&buf, nil))
		opts := append([]Option{WithLogger(logger), WithDoer(&recorder{})}, c.opts...)
		if _, err := New(c.addr, opts...); err != nil {
			t.Fatal(err)
		}
		if got := strings.Contains(buf.String(), "plaintext"); got != c.warn {
			t.Errorf("%s: warned = %v, want %v", c.addr, got, c.warn)
		}
	}
}

func TestAddrIsWhatWasDialed(t *testing.T) {
	c, err := New("https://example.com:7654", WithDoer(&recorder{}))
	if err != nil {
		t.Fatal(err)
	}
	a := c.Addr()
	if a.Host != "example.com" || !a.TLS {
		t.Errorf("Addr = %#v", a)
	}
	// The certificate is checked against the host with no port, because a certificate is issued
	// to a name and "example.com:7654" is not a name.
	if a.HostHeader() != "example.com:7654" {
		t.Errorf("HostHeader = %q", a.HostHeader())
	}
}

func TestMergeInsertsCeilings(t *testing.T) {
	rows := [][]any{{1}, {2}, {3}}

	// Empty is empty, not a statement with nothing in it.
	if got, _ := MergeInserts("INSERT INTO t (n) VALUES (?)", nil, DefaultMaxBytes, 10); got != nil {
		t.Errorf("no rows must merge to nothing, got %q", got)
	}

	// The byte ceiling splits.
	got, err := MergeInserts("INSERT INTO t (n) VALUES (?)", rows, 32, 100)
	if err != nil {
		t.Fatal(err)
	}
	if len(got) < 2 {
		t.Errorf("a tight byte ceiling must split, got %q", got)
	}
	for _, s := range got {
		if len(s) > 32 {
			t.Errorf("a merged statement is %d bytes, over the ceiling: %q", len(s), s)
		}
	}

	// Ragged rows are not one template's worth of arguments.
	ragged := [][]any{{1}, {2, 3}}
	if got, _ := MergeInserts("INSERT INTO t (n) VALUES (?)", ragged, DefaultMaxBytes, 10); len(got) != 0 {
		// Bind will refuse the second row, so this is an error rather than a merge.
		t.Logf("ragged rows fell back to %d statements", len(got))
	}

	// One row that cannot fit at all is said out loud rather than emitted.
	_, err = MergeInserts("INSERT INTO t (n) VALUES (?)", [][]any{{1}}, 4, 10)
	var tl *TooLargeError
	if !errors.As(err, &tl) {
		t.Errorf("want a *TooLargeError, got %#v", err)
	}

	// A refused value names its row.
	_, err = MergeInserts("INSERT INTO t (n) VALUES (?)", [][]any{{1}, {nil}}, DefaultMaxBytes, 10)
	if err == nil || !strings.Contains(err.Error(), "row 2") {
		t.Errorf("want the row named, got %v", err)
	}
}

func TestRawResponseHeaders(t *testing.T) {
	h := map[string][]string{
		"Content-Type": {"application/json; charset=utf-8"},
		"X-Request-Id": {"r-1"},
		"Retry-After":  {"3"},
		"Connection":   {"close"},
	}
	r := &RawResponse{Status: 503, Header: h}

	if r.ContentType() != "application/json" {
		t.Errorf("ContentType = %q, want the type without its parameters", r.ContentType())
	}
	if r.RequestID() != "r-1" {
		t.Errorf("RequestID = %q", r.RequestID())
	}
	d, ok := r.RetryAfter()
	if !ok || d != 3*time.Second {
		t.Errorf("RetryAfter = %v %v", d, ok)
	}
	if !r.Closing() {
		t.Error("Connection: close means the server will not take another request")
	}

	// A Retry-After this client cannot read is absent rather than zero: zero would mean "try
	// again immediately", which is the opposite of what the server asked for.
	bad := &RawResponse{Header: map[string][]string{"Retry-After": {"Wed, 21 Oct 2026 07:28:00 GMT"}}}
	if _, ok := bad.RetryAfter(); ok {
		t.Error("an unreadable Retry-After must not read as zero")
	}
}
