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

package sqldriver

import (
	"context"
	"database/sql"
	"database/sql/driver"
	"errors"
	"io"
	"net/url"
	"strconv"
	"strings"
	"sync"
	"time"

	bigdb "github.com/raftio/bigdb/clients/go"
)

func init() { sql.Register("bigdb", Driver{}) }

// Driver is the registered driver. Use sql.Open("bigdb", dsn).
type Driver struct{}

// Open is the legacy entry point; OpenConnector is what database/sql prefers.
func (d Driver) Open(dsn string) (driver.Conn, error) {
	c, err := d.OpenConnector(dsn)
	if err != nil {
		return nil, err
	}
	return c.Connect(context.Background())
}

// OpenConnector parses the DSN once, at sql.Open, rather than once per connection.
func (d Driver) OpenConnector(dsn string) (driver.Connector, error) {
	addr, opts, err := parseDSN(dsn)
	if err != nil {
		return nil, err
	}
	return &connector{addr: addr, opts: opts, driver: d}, nil
}

type connector struct {
	addr   string
	opts   []bigdb.Option
	driver Driver
}

func (c *connector) Driver() driver.Driver { return c.driver }

func (c *connector) Connect(context.Context) (driver.Conn, error) {
	client, err := bigdb.New(c.addr, c.opts...)
	if err != nil {
		return nil, err
	}
	return &conn{client: client, last: time.Now()}, nil
}

// parseDSN reads bigdb://user:password@host:port/database?timeout=30s&retries=3
//
// The scheme is also allowed to be http or https, in which case it decides TLS the way the rest
// of this client does - by the scheme and only by the scheme.
func parseDSN(dsn string) (string, []bigdb.Option, error) {
	u, err := url.Parse(dsn)
	if err != nil {
		return "", nil, &bigdb.ConfigError{What: "the DSN is not a URL: " + err.Error()}
	}

	scheme := "http"
	switch u.Scheme {
	case "bigdb", "http":
	case "bigdbs", "https":
		scheme = "https"
	case "":
		return "", nil, &bigdb.ConfigError{What: "the DSN needs a scheme, e.g. bigdb://host:7654"}
	default:
		return "", nil, &bigdb.ConfigError{What: u.Scheme + " is not a scheme this driver dials"}
	}
	if u.Host == "" {
		return "", nil, &bigdb.ConfigError{What: "the DSN needs a host and a port"}
	}

	var opts []bigdb.Option
	if u.User != nil {
		opts = append(opts, bigdb.WithUser(u.User.Username()))
		if p, ok := u.User.Password(); ok {
			opts = append(opts, bigdb.WithPassword(p))
		}
	}
	if db := strings.TrimPrefix(u.Path, "/"); db != "" {
		opts = append(opts, bigdb.WithDatabase(db))
	}

	q := u.Query()
	for key, values := range q {
		v := values[0]
		switch key {
		case "timeout":
			d, err := time.ParseDuration(v)
			if err != nil {
				return "", nil, &bigdb.ConfigError{What: "timeout is not a duration: " + v}
			}
			opts = append(opts, bigdb.WithTimeout(d))
		case "retries":
			n, err := strconv.Atoi(v)
			if err != nil || n < 0 {
				return "", nil, &bigdb.ConfigError{What: "retries is not a count: " + v}
			}
			opts = append(opts, bigdb.WithRetries(n))
		case "ca_file":
			opts = append(opts, bigdb.WithCAFile(v))
		case "insecure_skip_verify":
			b, err := strconv.ParseBool(v)
			if err != nil {
				return "", nil, &bigdb.ConfigError{What: "insecure_skip_verify is not a bool: " + v}
			}
			opts = append(opts, bigdb.WithInsecureSkipVerify(b))
		case "max_bytes":
			n, err := strconv.Atoi(v)
			if err != nil || n <= 0 {
				return "", nil, &bigdb.ConfigError{What: "max_bytes is not a size: " + v}
			}
			opts = append(opts, bigdb.WithMaxBytes(n))
		case "max_rows":
			n, err := strconv.Atoi(v)
			if err != nil || n <= 0 {
				return "", nil, &bigdb.ConfigError{What: "max_rows is not a count: " + v}
			}
			opts = append(opts, bigdb.WithMaxRows(n))
		default:
			// Named, because a typo in a DSN parameter that is silently ignored is a setting
			// the caller believes is in force and is not.
			return "", nil, &bigdb.ConfigError{What: key + " is not a DSN parameter this driver reads"}
		}
	}
	return scheme + "://" + u.Host, opts, nil
}

// conn is one bigdb.Client, wearing the driver interfaces.
//
// # Retirement, and why database/sql already has the right two hooks
//
// The connection lifetime rules are the same as the Client's: replace a connection after
// MaxRequestsPerConn requests and after MaxIdle of quiet, so that the server never closes one
// underneath a request. database/sql happens to provide exactly the two callbacks those need:
//
//   - Validator.IsValid is called before a connection goes back into the pool, which is where
//     the request count is checked.
//   - SessionResetter.ResetSession is called before a query runs on a connection that has been
//     used before, which is where the idle time is checked - and that is precisely the moment
//     contrib/big-message/src/http.rs argues the check has to happen, because it is the only
//     point at which a fresh connect is provably safe.
type conn struct {
	client *bigdb.Client

	mu   sync.Mutex
	sent int
	last time.Time
}

var (
	_ driver.Conn               = (*conn)(nil)
	_ driver.QueryerContext     = (*conn)(nil)
	_ driver.ExecerContext      = (*conn)(nil)
	_ driver.ConnPrepareContext = (*conn)(nil)
	_ driver.Pinger             = (*conn)(nil)
	_ driver.Validator          = (*conn)(nil)
	_ driver.SessionResetter    = (*conn)(nil)
)

func (c *conn) Close() error { return c.client.Close() }

// IsValid reports whether this connection may go back into the pool.
func (c *conn) IsValid() bool {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.sent < bigdb.MaxRequestsPerConn
}

// ResetSession runs before a query on a connection that has been used before.
func (c *conn) ResetSession(context.Context) error {
	c.mu.Lock()
	defer c.mu.Unlock()
	if time.Since(c.last) >= bigdb.MaxIdle {
		// The pool discards the connection and dials a new one. Retiring here rather than
		// discovering it mid-request is the whole point.
		return driver.ErrBadConn
	}
	return nil
}

func (c *conn) mark() {
	c.mu.Lock()
	c.sent++
	c.last = time.Now()
	c.mu.Unlock()
}

func (c *conn) Ping(ctx context.Context) error {
	c.mark()
	_, err := c.client.Health(ctx)
	return err
}

// Begin returns a transaction that is honest about not being one.
//
// See the package doc: an error from Begin would break libraries that call it defensively, and
// a Rollback that pretended to succeed would be a lie at the one moment it matters.
func (c *conn) Begin() (driver.Tx, error) { return tx{}, nil }

func (c *conn) BeginTx(context.Context, driver.TxOptions) (driver.Tx, error) { return tx{}, nil }

type tx struct{}

func (tx) Commit() error { return nil }

func (tx) Rollback() error {
	return errors.New("bigdb: this server has no transactions to roll back; each statement " +
		"is committed on its own")
}

func (c *conn) QueryContext(ctx context.Context, query string, args []driver.NamedValue) (driver.Rows, error) {
	stmt, err := bind(query, args)
	if err != nil {
		return nil, err
	}
	c.mark()
	res, err := c.client.SQL(ctx, stmt)
	if err != nil {
		return nil, err
	}
	if res.IsText() {
		// A FORMAT clause changed the body into something that is not a result set. There is no
		// honest way to hand that to database/sql, which is written to read columns and rows.
		return nil, notATable(res.ContentType)
	}
	return newRows(res), nil
}

func (c *conn) ExecContext(ctx context.Context, query string, args []driver.NamedValue) (driver.Result, error) {
	stmt, err := bind(query, args)
	if err != nil {
		return nil, err
	}
	c.mark()
	res, err := c.client.SQL(ctx, stmt)
	if err != nil {
		return nil, err
	}
	return result{res: res}, nil
}

func (c *conn) Prepare(query string) (driver.Stmt, error) {
	return c.PrepareContext(context.Background(), query)
}

func (c *conn) PrepareContext(_ context.Context, query string) (driver.Stmt, error) {
	return &stmt{conn: c, query: query}, nil
}

func notATable(contentType string) error {
	return errors.New("bigdb: this statement answered with " + contentType +
		" rather than a result set, because its FORMAT clause asked for that. database/sql " +
		"reads columns and rows; use bigdb.Client.SQL to get the bytes")
}

// bind renders the arguments into the statement.
//
// Ordinal arguments only. database/sql's named parameters have no spelling in this dialect, and
// accepting them here would mean inventing one.
func bind(query string, args []driver.NamedValue) (string, error) {
	if len(args) == 0 {
		return query, nil
	}
	out := make([]any, len(args))
	for _, a := range args {
		if a.Name != "" {
			return "", errors.New("bigdb: this dialect has no named parameters; use ? and " +
				"positional arguments")
		}
		if a.Ordinal < 1 || a.Ordinal > len(args) {
			return "", errors.New("bigdb: an argument arrived out of range")
		}
		out[a.Ordinal-1] = a.Value
	}
	return bigdb.Bind(query, out...)
}

// result is what an Exec answers with.
//
// Both methods return an error rather than a zero, because both questions have real answers
// this route does not give: LastInsertId would be the record id the server allocated, which it
// does not report, and RowsAffected is not in the result-set envelope either. Returning 0 would
// be a number a caller could act on wrongly.
type result struct{ res *bigdb.SQLResult }

func (r result) LastInsertId() (int64, error) {
	return 0, errors.New("bigdb: this server does not report the record id it allocated")
}

func (r result) RowsAffected() (int64, error) {
	return 0, errors.New("bigdb: this server does not report how many rows a statement touched")
}

// stmt carries the statement text. There is nothing prepared at the server; see the package doc.
type stmt struct {
	conn  *conn
	query string
}

func (s *stmt) Close() error { return nil }

// NumInput counts the placeholders, using the same scanner Bind uses.
//
// Returning the real count rather than -1 means database/sql catches a mismatched argument
// count before a socket opens, in the caller's own stack.
func (s *stmt) NumInput() int { return bigdb.CountPlaceholders(s.query) }

func (s *stmt) Query(args []driver.Value) (driver.Rows, error) {
	return s.QueryContext(context.Background(), ordinal(args))
}

func (s *stmt) Exec(args []driver.Value) (driver.Result, error) {
	return s.ExecContext(context.Background(), ordinal(args))
}

func (s *stmt) QueryContext(ctx context.Context, args []driver.NamedValue) (driver.Rows, error) {
	return s.conn.QueryContext(ctx, s.query, args)
}

func (s *stmt) ExecContext(ctx context.Context, args []driver.NamedValue) (driver.Result, error) {
	return s.conn.ExecContext(ctx, s.query, args)
}

func ordinal(args []driver.Value) []driver.NamedValue {
	out := make([]driver.NamedValue, len(args))
	for i, v := range args {
		out[i] = driver.NamedValue{Ordinal: i + 1, Value: v}
	}
	return out
}

var _ io.Closer = (*conn)(nil)
