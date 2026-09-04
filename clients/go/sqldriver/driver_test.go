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
	"encoding/json"
	"errors"
	"math"
	"strings"
	"sync"
	"testing"
	"time"

	bigdb "github.com/raftio/bigdb/clients/go"
)

// The driver is tested by driving it over a stub transport, so these tests assert the exact
// statement text that would go out without a server anywhere near them.

type stub struct {
	mu   sync.Mutex
	sent []string
	// reply returns the body and content type for request n.
	reply func(n int, body string) (status int, contentType, out string)
}

func (s *stub) Do(_ context.Context, _, _ string, body []byte) (*bigdb.RawResponse, error) {
	s.mu.Lock()
	n := len(s.sent)
	s.sent = append(s.sent, string(body))
	s.mu.Unlock()

	status, ct, out := 200, "application/json", `{"columns":[],"rows":[]}`
	if s.reply != nil {
		status, ct, out = s.reply(n, string(body))
	}
	h := make(map[string][]string)
	h["Content-Type"] = []string{ct}
	return &bigdb.RawResponse{Status: status, Header: h, Body: []byte(out)}, nil
}

func (s *stub) statements() []string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return append([]string(nil), s.sent...)
}

// openStub gives a *sql.DB whose one connection is driven by the stub.
func openStub(t *testing.T, s *stub) *sql.DB {
	t.Helper()
	// Retries off: these tests assert what one call sends, and a retried 503 would make the
	// count of statements on the wire a different question from the one being asked.
	db := sql.OpenDB(&connector{addr: "127.0.0.1:7654", opts: []bigdb.Option{
		bigdb.WithDoer(s), bigdb.WithRetries(0),
	}})
	t.Cleanup(func() { _ = db.Close() })
	return db
}

func TestArgumentsAreRenderedNotPassedThrough(t *testing.T) {
	// The server has no parameters, so the driver substitutes. The point of this test is the
	// exact text: this is where an injection would be visible.
	s := &stub{}
	db := openStub(t, s)

	_, err := db.ExecContext(context.Background(),
		"INSERT INTO tx (country) VALUES (?)", "x'); DROP TABLE tx; --")
	if err != nil {
		t.Fatal(err)
	}
	want := "INSERT INTO tx (country) VALUES ('x''); DROP TABLE tx; --')"
	if got := s.statements()[0]; got != want {
		t.Errorf("statement =\n%q\nwant\n%q", got, want)
	}
}

func TestAPlaceholderInsideALiteralIsNotAnArgument(t *testing.T) {
	s := &stub{}
	db := openStub(t, s)

	// One placeholder, not two: the ? inside the literal is data. If the scanner counted it,
	// NumInput would disagree with Bind and database/sql would refuse the call.
	if _, err := db.ExecContext(context.Background(),
		"SELECT * FROM t WHERE a = 'is it? yes' AND b = ?", 1); err != nil {
		t.Fatal(err)
	}
	want := "SELECT * FROM t WHERE a = 'is it? yes' AND b = 1"
	if got := s.statements()[0]; got != want {
		t.Errorf("statement = %q, want %q", got, want)
	}
}

func TestNumInputCatchesAMismatchBeforeTheSocketOpens(t *testing.T) {
	s := &stub{}
	db := openStub(t, s)

	st, err := db.PrepareContext(context.Background(), "SELECT * FROM t WHERE a = ? AND b = ?")
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()

	if _, err := st.Exec(1); err == nil {
		t.Fatal("want a refusal for the wrong argument count")
	}
	if len(s.statements()) != 0 {
		t.Error("nothing must reach the wire when the count is wrong")
	}
}

func TestNilIsRefusedBecauseTheDialectHasNoNull(t *testing.T) {
	s := &stub{}
	db := openStub(t, s)

	_, err := db.ExecContext(context.Background(), "INSERT INTO t (a) VALUES (?)", nil)
	if err == nil {
		t.Fatal("want a refusal")
	}
	if !strings.Contains(err.Error(), "NULL") {
		t.Errorf("the message must say why: %v", err)
	}
}

func TestARecordIDLargerThanAFloat64HoldsSurvivesTheDriver(t *testing.T) {
	// The same guard as the parent package's, at the other end of the pipe: a cell must not be
	// rounded on its way into a driver.Value.
	s := &stub{reply: func(int, string) (int, string, string) {
		return 200, "application/json",
			`{"columns":["id","n"],"rows":[[9007199254740993,18446744073709551615]]}`
	}}
	db := openStub(t, s)

	var id int64
	var big string
	if err := db.QueryRow("SELECT id, n FROM t").Scan(&id, &big); err != nil {
		t.Fatal(err)
	}
	if id != 9007199254740993 {
		t.Errorf("id = %d, want 9007199254740993", id)
	}
	// Above MaxInt64 there is no signed column type to put it in, so it comes over as its
	// digits rather than as a float that has lost the last four of them.
	if big != "18446744073709551615" {
		t.Errorf("n = %s, want 18446744073709551615", big)
	}
}

func TestCellTypes(t *testing.T) {
	s := &stub{reply: func(int, string) (int, string, string) {
		return 200, "application/json",
			`{"columns":["a","b","c","d","e"],"rows":[["x",true,3,3.5,null]]}`
	}}
	db := openStub(t, s)

	var a string
	var b bool
	var c int64
	var d float64
	var e any
	if err := db.QueryRow("SELECT a,b,c,d,e FROM t").Scan(&a, &b, &c, &d, &e); err != nil {
		t.Fatal(err)
	}
	if a != "x" || !b || c != 3 || d != 3.5 || e != nil {
		t.Errorf("row = %q %v %d %v %v", a, b, c, d, e)
	}
}

func TestAListCellArrivesAsItsBytes(t *testing.T) {
	s := &stub{reply: func(int, string) (int, string, string) {
		return 200, "application/json", `{"columns":["keys"],"rows":[[["gb","fr"]]]}`
	}}
	db := openStub(t, s)

	var raw []byte
	if err := db.QueryRow("SELECT keys FROM t").Scan(&raw); err != nil {
		t.Fatal(err)
	}
	var keys []string
	if err := json.Unmarshal(raw, &keys); err != nil {
		t.Fatal(err)
	}
	if len(keys) != 2 || keys[0] != "gb" {
		t.Errorf("keys = %v", keys)
	}
}

func TestAFormatClauseIsNotAResultSet(t *testing.T) {
	// The decision is made on the response's Content-Type. The driver does not scan the
	// statement, and the refusal points at the call that can actually return the bytes.
	s := &stub{reply: func(int, string) (int, string, string) {
		return 200, "text/csv", "n\r\n41\r\n"
	}}
	db := openStub(t, s)

	_, err := db.Query("SELECT count(*) FROM t FORMAT CSVWithNames")
	if err == nil {
		t.Fatal("want a refusal")
	}
	if !strings.Contains(err.Error(), "Client.SQL") {
		t.Errorf("the refusal must point somewhere useful: %v", err)
	}
}

func TestRollbackDoesNotPretend(t *testing.T) {
	// Begin succeeds so that a library calling it defensively still works; Rollback does not,
	// because this server committed each statement as it ran.
	s := &stub{}
	db := openStub(t, s)

	tx, err := db.Begin()
	if err != nil {
		t.Fatal(err)
	}
	if _, err := tx.Exec("INSERT INTO t (a) VALUES (1)"); err != nil {
		t.Fatal(err)
	}
	if err := tx.Rollback(); err == nil {
		t.Fatal("a rollback that silently succeeded would be a lie")
	}

	tx2, err := db.Begin()
	if err != nil {
		t.Fatal(err)
	}
	if err := tx2.Commit(); err != nil {
		t.Errorf("commit is a no-op, not a failure: %v", err)
	}
}

func TestIdleRetirementGoesThroughResetSession(t *testing.T) {
	c := &conn{last: time.Now()}
	if err := c.ResetSession(context.Background()); err != nil {
		t.Errorf("a fresh connection is usable: %v", err)
	}
	c.last = time.Now().Add(-bigdb.MaxIdle - time.Second)
	if err := c.ResetSession(context.Background()); !errors.Is(err, driver.ErrBadConn) {
		t.Errorf("an idle connection must be retired before the request, got %v", err)
	}
}

func TestRequestCeilingGoesThroughIsValid(t *testing.T) {
	c := &conn{last: time.Now()}
	if !c.IsValid() {
		t.Error("a fresh connection may go back into the pool")
	}
	c.sent = bigdb.MaxRequestsPerConn
	if c.IsValid() {
		t.Error("a connection at the ceiling must not go back into the pool")
	}
}

func TestMergeInsertsFoldsRowsAndFallsBackWhenItCannot(t *testing.T) {
	rows := [][]any{{1, "a"}, {2, "b"}, {3, "c"}}

	got, err := bigdb.MergeInserts("INSERT INTO t (n, s) VALUES (?, ?)", rows, bigdb.DefaultMaxBytes, 100)
	if err != nil {
		t.Fatal(err)
	}
	if len(got) != 1 {
		t.Fatalf("want one merged statement, got %d: %q", len(got), got)
	}
	want := "INSERT INTO t (n, s) VALUES (1, 'a'), (2, 'b'), (3, 'c')"
	if got[0] != want {
		t.Errorf("merged =\n%q\nwant\n%q", got[0], want)
	}

	// The row ceiling splits rather than overflows.
	got, err = bigdb.MergeInserts("INSERT INTO t (n, s) VALUES (?, ?)", rows, bigdb.DefaultMaxBytes, 2)
	if err != nil {
		t.Fatal(err)
	}
	if len(got) != 2 {
		t.Errorf("want two statements at maxRows 2, got %d: %q", len(got), got)
	}

	// A shape this does not recognise is sent one row at a time, which is always correct.
	got, err = bigdb.MergeInserts("UPDATE t SET n = ? WHERE id = ?", rows, bigdb.DefaultMaxBytes, 100)
	if err != nil {
		t.Fatal(err)
	}
	if len(got) != 3 {
		t.Errorf("an unfamiliar shape falls back to one statement per row, got %d", len(got))
	}
}

func TestExecBatchSendsMergedStatements(t *testing.T) {
	s := &stub{}
	db := openStub(t, s)

	n, err := ExecBatch(context.Background(), db, "INSERT INTO t (n) VALUES (?)",
		[][]any{{1}, {2}, {3}})
	if err != nil {
		t.Fatal(err)
	}
	if n != 1 {
		t.Errorf("statements = %d, want 1", n)
	}
	if got, want := s.statements()[0], "INSERT INTO t (n) VALUES (1), (2), (3)"; got != want {
		t.Errorf("statement = %q, want %q", got, want)
	}
}

func TestParseDSN(t *testing.T) {
	for _, c := range []struct{ dsn, addr string }{
		{"bigdb://127.0.0.1:7654", "http://127.0.0.1:7654"},
		{"bigdb://alice:s3cret@127.0.0.1:7654/sales", "http://127.0.0.1:7654"},
		{"bigdbs://example.com:7654", "https://example.com:7654"},
		{"https://example.com:7654", "https://example.com:7654"},
		{"bigdb://127.0.0.1:7654?timeout=5s&retries=1", "http://127.0.0.1:7654"},
	} {
		addr, _, err := parseDSN(c.dsn)
		if err != nil {
			t.Fatalf("%s: %v", c.dsn, err)
		}
		if addr != c.addr {
			t.Errorf("%s -> %s, want %s", c.dsn, addr, c.addr)
		}
	}

	for _, bad := range []string{
		"127.0.0.1:7654",           // no scheme
		"bigdb://",                 // no host
		"mysql://127.0.0.1:3306",   // not a scheme this dials
		"bigdb://h:1?timeout=soon", // not a duration
		"bigdb://h:1?retries=lots", // not a count
		"bigdb://h:1?tiemout=5s",   // a typo must not be silently ignored
	} {
		if _, _, err := parseDSN(bad); err == nil {
			t.Errorf("%q must not parse", bad)
		}
	}
}

func TestUint64ArgumentsSurviveTheDefaultConverter(t *testing.T) {
	// database/sql's default converter rejects a uint64 with the high bit set. CheckNamedValue
	// takes these before it can, so a record id is still a record id by the time Literal sees it.
	s := &stub{}
	db := openStub(t, s)

	if _, err := db.Exec("SELECT ?", uint64(math.MaxUint64)); err != nil {
		t.Fatal(err)
	}
	if got, want := s.statements()[0], "SELECT 18446744073709551615"; got != want {
		t.Errorf("statement = %q, want %q", got, want)
	}
}
