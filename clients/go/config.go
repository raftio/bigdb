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
	"log/slog"
	"time"
)

// Every ceiling here is set below the server's, and each one says which server ceiling it is
// under. A client tuned to exactly the server's limit is a client that hits it, and hitting a
// limit from the wrong side is a failure the caller cannot act on.

const (
	// DefaultAddr is what `big serve` listens on and what `make start` gives you.
	DefaultAddr = "127.0.0.1:7654"

	// MaxRequestsPerConn retires a connection before the server does. The server's
	// max_keepalive_requests is 1000; stopping at 900 means the connection is always retired by
	// the client, between requests, rather than by the server underneath one.
	MaxRequestsPerConn = 900

	// MaxIdle retires a connection that has been quiet. The server's keepalive_idle is five
	// seconds. Three leaves two seconds of margin for the two clocks disagreeing about when the
	// last exchange ended - the margin is the point, and it costs one extra connect on a client
	// that has been quiet anyway.
	MaxIdle = 3 * time.Second

	// DefaultMaxBytes is the largest body this client sends. big_http::MAX_BODY is 8 MiB and is
	// checked against Content-Length before the body is read, so an over-large request is a 413
	// rather than a half-written one. Seven leaves the same 1 MiB margin `bigctl import` and
	// big-message leave, for the headers and for a body that grew by a line since it was
	// measured.
	DefaultMaxBytes = 7 << 20

	// DefaultMaxRows caps a batched INSERT. Same ceiling as contrib/big-message/src/batch.rs.
	DefaultMaxRows = 900_000

	// DefaultTimeout matches the server's read_timeout and write_timeout. Raising the server's
	// --query-timeout above this without raising the client's turns every long query into an
	// outcome nobody knows - the worst way for a slow query to fail. See the readme.
	DefaultTimeout = 30 * time.Second

	// DefaultRetries is how many extra attempts a retryable failure gets.
	DefaultRetries = 3

	// DefaultPage is the page size IterRecords asks for, matching the server's own default at
	// routes/mod.rs::DEFAULT_PAGE.
	DefaultPage = 1000

	// UserAgent identifies this client in the server's log.
	UserAgent = "bigdb-go"
)

// Config is the settled form of the options passed to New. It is not part of the public
// surface as a struct literal - use the With* options - so that a field added later does not
// break a caller who wrote one out.
type config struct {
	user     string
	password string
	database string

	caFile             string
	insecureSkipVerify bool

	timeout    time.Duration
	retries    int
	maxBytes   int
	maxRows    int
	logger     *slog.Logger
	doer       Doer
	warnPlain  bool
	userAgent  string
	retryDelay func(attempt int) time.Duration
}

func defaults() config {
	return config{
		timeout:    DefaultTimeout,
		retries:    DefaultRetries,
		maxBytes:   DefaultMaxBytes,
		maxRows:    DefaultMaxRows,
		logger:     slog.Default(),
		warnPlain:  true,
		userAgent:  UserAgent,
		retryDelay: backoff,
	}
}

// backoff is 50ms, 100ms, 200ms, ... capped at a second.
//
// Not jittered: this client holds one connection and makes one request at a time, so there is
// no herd of its own to disperse. A caller running many of them and worried about a herd should
// pass WithRetryDelay.
func backoff(attempt int) time.Duration {
	d := 50 * time.Millisecond << attempt
	if d > time.Second {
		return time.Second
	}
	return d
}

// An Option configures a Client. Options are used rather than an exported config struct so
// that a new one can be added without breaking a caller who wrote a struct literal.
type Option func(*config)

// WithUser sets the username sent in the Authorization header.
func WithUser(user string) Option { return func(c *config) { c.user = user } }

// WithPassword sets the password sent in the Authorization header.
func WithPassword(password string) Option { return func(c *config) { c.password = password } }

// WithDatabase sets the database sent as ?database= on every route.
//
// Read the package doc on what that does and does not do: the server threads it into query
// options for /sql and /query only, and takes a possibly-qualified table name everywhere else.
func WithDatabase(database string) Option { return func(c *config) { c.database = database } }

// WithCAFile verifies the server against the certificates in a PEM file rather than against
// the system roots. Only meaningful on an https address.
func WithCAFile(path string) Option { return func(c *config) { c.caFile = path } }

// WithInsecureSkipVerify turns off certificate verification.
//
// Named for what it is. There is no "development mode" flag here that turns this on as a side
// effect of something else, because that is how it ends up in production.
func WithInsecureSkipVerify(skip bool) Option {
	return func(c *config) { c.insecureSkipVerify = skip }
}

// WithTimeout sets the deadline used for a call whose context has none.
//
// A context deadline always wins when there is one. This is the floor for callers who pass
// context.Background(), which is most of them, most of the time.
func WithTimeout(d time.Duration) Option { return func(c *config) { c.timeout = d } }

// WithRetries sets how many extra attempts a retryable failure gets. Zero means one attempt.
func WithRetries(n int) Option { return func(c *config) { c.retries = n } }

// WithRetryDelay replaces the backoff schedule. attempt counts from zero.
func WithRetryDelay(f func(attempt int) time.Duration) Option {
	return func(c *config) { c.retryDelay = f }
}

// WithMaxBytes sets the largest body this client will send. It cannot usefully be raised above
// the server's 8 MiB; lowering it is how a caller with a slow link gets smaller chunks.
func WithMaxBytes(n int) Option { return func(c *config) { c.maxBytes = n } }

// WithMaxRows caps how many rows a batched INSERT carries.
func WithMaxRows(n int) Option { return func(c *config) { c.maxRows = n } }

// WithLogger sets where the client writes its one warning. Pass a discarding logger to silence
// it: WithLogger(slog.New(slog.DiscardHandler)).
func WithLogger(l *slog.Logger) Option {
	return func(c *config) {
		if l != nil {
			c.logger = l
		}
	}
}

// WithoutPlaintextWarning silences the warning about sending a password over a plaintext
// connection to something that is not loopback.
func WithoutPlaintextWarning() Option { return func(c *config) { c.warnPlain = false } }

// WithUserAgent sets the User-Agent header.
func WithUserAgent(ua string) Option { return func(c *config) { c.userAgent = ua } }

// WithDoer replaces the transport.
//
// The seam exists for tests and for a caller who needs something this client does not do -
// a proxy, HTTP/2, a company's own dialer. Read the doc on Doer first: a transport that cannot
// tell "not sent" from "outcome unknown" costs the client its retry safety, and net/http is
// one of those.
func WithDoer(d Doer) Option { return func(c *config) { c.doer = d } }
