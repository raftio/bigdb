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
	"strconv"
	"strings"
	"sync"
	"time"
)

// Client is one connection to one server.
//
// It is safe to use from several goroutines - calls are serialised on the connection - but it
// will not make them faster. This is a deliberate difference from the Python client, which
// declares threadsafety 1 and takes no lock: in Go a client that cannot be shared is a client
// that will be shared anyway, by accident, at three in the morning. For real parallelism open
// several Clients, or go through database/sql, which pools them.
type Client struct {
	addr Address
	cfg  config
	doer Doer

	mu   sync.Mutex
	once sync.Once
}

// New opens a client. It does not dial: the first call does that, so that a Client can be
// constructed at start-up without the server having to be up yet.
func New(addr string, opts ...Option) (*Client, error) {
	if addr == "" {
		addr = DefaultAddr
	}
	a, err := ParseAddress(addr)
	if err != nil {
		return nil, err
	}

	cfg := defaults()
	for _, o := range opts {
		o(&cfg)
	}
	if cfg.retries < 0 {
		cfg.retries = 0
	}

	c := &Client{addr: a, cfg: cfg, doer: cfg.doer}
	if c.doer == nil {
		d, err := newConn(a, cfg)
		if err != nil {
			return nil, err
		}
		c.doer = d
	}
	c.warnIfPlaintextCredential()
	return c, nil
}

// warnIfPlaintextCredential says something once, and only where it is worth saying.
//
// Base64 is not encryption, and the default address is plaintext. Refusing outright would break
// 127.0.0.1:7654 with a users file, which is an entirely ordinary way to run this - and a client
// that complains about the normal case teaches people to ignore it. So: silent on loopback, one
// warning otherwise, and an option to turn it off.
func (c *Client) warnIfPlaintextCredential() {
	if !c.cfg.warnPlain || c.addr.TLS || c.cfg.password == "" || c.addr.IsLoopback() {
		return
	}
	c.once.Do(func() {
		c.cfg.logger.Warn(
			"bigdb: sending a password over a plaintext connection; base64 is not encryption",
			"addr", c.addr.Dial())
	})
}

// Addr is the address this client dials.
func (c *Client) Addr() Address { return c.addr }

// Close releases the connection.
func (c *Client) Close() error {
	if cl, ok := c.doer.(doerCloser); ok {
		return cl.Close()
	}
	return nil
}

// run sends one op, retrying according to the policy in retry.go, and returns the raw answer.
//
// This is the only loop in the package that sleeps, and the decision it acts on is made by a
// pure function that does not.
func (c *Client) run(ctx context.Context, o op) (*RawResponse, error) {
	if err := o.check(c.cfg.maxBytes); err != nil {
		return nil, err
	}

	var last error
	for attempt := 0; ; attempt++ {
		resp, err := c.doer.Do(ctx, o.Method, o.Target, o.Body)
		if err == nil {
			if resp.Status >= 200 && resp.Status < 300 {
				return resp, nil
			}
			err = decodeError(resp)
		}
		last = err

		// Never retry past the caller's own deadline, whatever the failure looked like. decide()
		// refuses a cancellation it can see, but a socket that timed out on a deadline derived
		// from this context can reach here as an ordinary I/O error - and spending another
		// attempt after the caller has already given up is the one thing a deadline is for.
		if ctx.Err() != nil {
			return nil, last
		}

		d := decide(err, o.Idempotent, attempt, c.cfg.retries, c.cfg.retryDelay)
		if !d.Retry {
			return nil, last
		}
		select {
		case <-ctx.Done():
			return nil, ctx.Err()
		case <-time.After(d.Wait):
		}
	}
}

// SQL runs one statement.
//
// The answer's shape follows the response's Content-Type: a statement with a FORMAT clause
// comes back as text, and SQLResult.IsText says so. This client never scans the statement to
// find out - see the note on SQLResult.
//
// This call is not retried on an unknown outcome, because an INSERT that lets the server
// allocate a record id writes a second row when it runs twice. A caller who needs a retry that
// is safe picks the record ids and uses Import.
func (c *Client) SQL(ctx context.Context, statement string, opts ...CallOpt) (*SQLResult, error) {
	resp, err := c.run(ctx, opSQL(statement, apply(opts, c.cfg.database)))
	if err != nil {
		return nil, err
	}
	return decodeSQL(resp)
}

// Query runs one PQL query against one table.
func (c *Client) Query(ctx context.Context, table, pql string, opts ...CallOpt) (Answer, error) {
	resp, err := c.run(ctx, opQuery(table, pql, apply(opts, c.cfg.database)))
	if err != nil {
		return nil, err
	}
	return decodeAnswer(resp.Body)
}

// Import writes a batch of facts.
//
// Idempotent, and that is what makes it the safe way to write: a fact is one bit set at an
// address the caller chose, so sending the same batch twice leaves the table as it was after
// the first. A batch larger than the client's ceiling is refused here rather than at the
// server, and the refusal names the line that crossed it. See ImportStream for a batch that
// does not fit.
func (c *Client) Import(ctx context.Context, table string, facts []Fact, opts ...CallOpt) (*WriteResult, error) {
	body, err := RenderFacts(facts, c.cfg.maxBytes)
	if err != nil {
		return nil, err
	}
	return c.importBody(ctx, table, body, apply(opts, c.cfg.database))
}

// ImportRaw writes a body that is already in the line format: `field record value` per line.
func (c *Client) ImportRaw(ctx context.Context, table string, body []byte, opts ...CallOpt) (*WriteResult, error) {
	return c.importBody(ctx, table, body, apply(opts, c.cfg.database))
}

func (c *Client) importBody(ctx context.Context, table string, body []byte, o callOptions) (*WriteResult, error) {
	resp, err := c.run(ctx, opImport(table, body, o))
	if err != nil {
		return nil, err
	}
	return decodeWrite(resp.Body, "imported")
}

// DeleteRecords removes records by id.
func (c *Client) DeleteRecords(ctx context.Context, table string, records []uint64, opts ...CallOpt) (*WriteResult, error) {
	body, err := RenderRecords(records, c.cfg.maxBytes)
	if err != nil {
		return nil, err
	}
	resp, err := c.run(ctx, opDelete(table, body, apply(opts, c.cfg.database)))
	if err != nil {
		return nil, err
	}
	return decodeWrite(resp.Body, "deleted")
}

// Records lists one page of a table's record ids.
//
// Read the note on RecordPage about the cursor: a full page always reports one, so a scan ends
// with an empty page rather than with a nil Next on the last full one. IterRecords handles that.
func (c *Client) Records(ctx context.Context, table string, opts ...CallOpt) (*RecordPage, error) {
	resp, err := c.run(ctx, opRecords(table, apply(opts, c.cfg.database)))
	if err != nil {
		return nil, err
	}
	return decodeRecordPage(resp.Body)
}

// Schema is every table the server will describe.
func (c *Client) Schema(ctx context.Context, opts ...CallOpt) (*Schema, error) {
	resp, err := c.run(ctx, opSchema(apply(opts, c.cfg.database)))
	if err != nil {
		return nil, err
	}
	return decodeSchema(resp.Body)
}

// Health is whether the server is up. It is never authenticated, so it answers while the
// database is still opening.
func (c *Client) Health(ctx context.Context) (bool, error) {
	resp, err := c.run(ctx, opHealth())
	if err != nil {
		return false, err
	}
	return resp.Status == 200, nil
}

// Ready is what the server says about itself. Also never authenticated.
func (c *Client) Ready(ctx context.Context) (*Ready, error) {
	resp, err := c.run(ctx, opReady())
	if err != nil {
		return nil, err
	}
	return decodeReady(resp.Body)
}

// CreateTable makes a table and returns the number the server assigned it.
//
// No DDL is retried. In a cluster a schema change that got far enough may already have come
// back as partially_applied, and sending it again turns one thing to check into two.
func (c *Client) CreateTable(ctx context.Context, table string, opts ...CallOpt) (int64, error) {
	resp, err := c.run(ctx, opCreateTable(table, apply(opts, c.cfg.database)))
	if err != nil {
		return 0, err
	}
	return scalarInt(resp.Body)
}

// DropTable removes a table.
func (c *Client) DropTable(ctx context.Context, table string, opts ...CallOpt) error {
	_, err := c.run(ctx, opDropTable(table, apply(opts, c.cfg.database)))
	return err
}

// CreateField adds a field.
//
// kind is passed through as written and is not validated here, because the server has two
// vocabularies and this client is not the place to reconcile them: /schema renders "signedint"
// but this route reads "signed". A kind this client has never heard of is between the caller
// and the server.
func (c *Client) CreateField(ctx context.Context, table, field, kind string, opts ...CallOpt) (int64, error) {
	resp, err := c.run(ctx, opCreateField(table, field, kind, apply(opts, c.cfg.database)))
	if err != nil {
		return 0, err
	}
	return scalarInt(resp.Body)
}

// DropField removes a field.
func (c *Client) DropField(ctx context.Context, table, field string, opts ...CallOpt) error {
	_, err := c.run(ctx, opDropField(table, field, apply(opts, c.cfg.database)))
	return err
}

// CreateDatabase makes a database.
func (c *Client) CreateDatabase(ctx context.Context, database string) error {
	_, err := c.run(ctx, opCreateDatabase(database))
	return err
}

// DropDatabase removes a database. Pass Cascade() to remove what is in it too.
func (c *Client) DropDatabase(ctx context.Context, database string, opts ...CallOpt) error {
	_, err := c.run(ctx, opDropDatabase(database, apply(opts, nil2empty(c.cfg.database))))
	return err
}

func nil2empty(s string) string { return s }

// scalarInt reads an answer that is one number under one key, which is what the DDL routes
// answer with. The key is not fixed across routes, so the value is taken from the single
// numeric field rather than from a name this client would have to keep in step.
func scalarInt(body []byte) (int64, error) {
	m, err := unmarshalObject(body)
	if err != nil {
		// A DDL route that answered with something else still succeeded - the status said so -
		// so this is not an error worth failing the call over.
		return 0, nil
	}
	for _, v := range m {
		s := strings.TrimSpace(string(v))
		if n, err := strconv.ParseInt(s, 10, 64); err == nil {
			return n, nil
		}
	}
	return 0, nil
}
