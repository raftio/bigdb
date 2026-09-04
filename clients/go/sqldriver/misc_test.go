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
	"strings"
	"testing"

	bigdb "github.com/raftio/bigdb/clients/go"
)

func TestSQLOpenGoesThroughTheRegisteredDriver(t *testing.T) {
	// The registration itself, which nothing else here exercises: a caller writes sql.Open and
	// expects the DSN to be parsed then, not at first use.
	db, err := sql.Open("bigdb", "bigdb://127.0.0.1:1/sales?timeout=1s")
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()

	// Nothing is listening on port 1, so this is a dial failure and not a DSN failure - which
	// is the distinction being asserted.
	if err := db.Ping(); err == nil {
		t.Error("want a dial failure")
	}

	if _, err := sql.Open("bigdb", "mysql://x:1"); err == nil {
		t.Error("a scheme this driver does not dial must be refused at Open")
	}
}

func TestOpenConnectorAndOpenAgree(t *testing.T) {
	var d Driver
	if _, err := d.OpenConnector("bigdb://127.0.0.1:7654"); err != nil {
		t.Fatal(err)
	}
	c, err := d.Open("bigdb://127.0.0.1:7654")
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()

	if _, ok := c.(driver.Conn); !ok {
		t.Error("Open must give a driver.Conn")
	}
	if got := (&connector{driver: d}).Driver(); got != d {
		t.Errorf("Driver = %#v", got)
	}
}

func TestPingCountsAsARequest(t *testing.T) {
	s := &stub{}
	db := openStub(t, s)

	if err := db.Ping(); err != nil {
		t.Fatal(err)
	}
	if len(s.statements()) != 1 {
		t.Errorf("Ping must reach the server, %d requests", len(s.statements()))
	}
}

func TestExecResultRefusesToInventNumbers(t *testing.T) {
	// Both questions have real answers this route does not give. Returning zero would be a
	// number a caller could act on wrongly.
	s := &stub{}
	db := openStub(t, s)

	res, err := db.Exec("INSERT INTO t (a) VALUES (1)")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := res.LastInsertId(); err == nil {
		t.Error("this server does not report the record id it allocated")
	}
	if _, err := res.RowsAffected(); err == nil {
		t.Error("this server does not report how many rows a statement touched")
	}
}

func TestPreparedStatementsCarryTheirText(t *testing.T) {
	s := &stub{}
	db := openStub(t, s)

	st, err := db.Prepare("SELECT * FROM t WHERE a = ?")
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()

	rows, err := st.Query("x")
	if err != nil {
		t.Fatal(err)
	}
	defer rows.Close()

	if got, want := s.statements()[0], "SELECT * FROM t WHERE a = 'x'"; got != want {
		t.Errorf("statement = %q, want %q", got, want)
	}
}

func TestNamedParametersAreRefusedRatherThanInvented(t *testing.T) {
	s := &stub{}
	db := openStub(t, s)

	_, err := db.Exec("SELECT * FROM t WHERE a = ?", sql.Named("a", 1))
	if err == nil {
		t.Fatal("want a refusal")
	}
	if !strings.Contains(err.Error(), "named parameters") {
		t.Errorf("the message must say why: %v", err)
	}
}

func TestAValuerIsHonoured(t *testing.T) {
	s := &stub{}
	db := openStub(t, s)

	// sql.NullString is the canonical driver.Valuer, and a valid one has to render as its text.
	if _, err := db.Exec("SELECT ?", sql.NullString{String: "gb", Valid: true}); err != nil {
		t.Fatal(err)
	}
	if got, want := s.statements()[0], "SELECT 'gb'"; got != want {
		t.Errorf("statement = %q, want %q", got, want)
	}

	// And an invalid one is NULL, which this dialect does not have - so it is refused rather
	// than rendered as something else.
	if _, err := db.Exec("SELECT ?", sql.NullString{}); err == nil {
		t.Error("a null Valuer must be refused")
	}
}

func TestRowsReportsAShapeItCannotRead(t *testing.T) {
	// A result set whose rows do not match its declared columns is a protocol problem, and
	// scanning it into the wrong number of destinations would hide that.
	s := &stub{reply: func(int, string) (int, string, string) {
		return 200, "application/json", `{"columns":["a","b"],"rows":[[1]]}`
	}}
	db := openStub(t, s)

	rows, err := db.Query("SELECT a, b FROM t")
	if err != nil {
		t.Fatal(err)
	}
	defer rows.Close()

	if rows.Next() {
		t.Error("a short row must not scan")
	}
	if rows.Err() == nil {
		t.Error("and the mismatch must be reported")
	}
}

func TestColumnsComeFromTheResultSet(t *testing.T) {
	s := &stub{reply: func(int, string) (int, string, string) {
		return 200, "application/json", `{"columns":["n","s"],"rows":[]}`
	}}
	db := openStub(t, s)

	rows, err := db.Query("SELECT n, s FROM t")
	if err != nil {
		t.Fatal(err)
	}
	defer rows.Close()

	cols, err := rows.Columns()
	if err != nil {
		t.Fatal(err)
	}
	if len(cols) != 2 || cols[0] != "n" || cols[1] != "s" {
		t.Errorf("columns = %v", cols)
	}
}

func TestBatchOptions(t *testing.T) {
	s := &stub{}
	db := openStub(t, s)

	// Two rows per statement, so three rows are two requests.
	n, err := ExecBatch(context.Background(), db, "INSERT INTO t (n) VALUES (?)",
		[][]any{{1}, {2}, {3}}, WithBatchMaxRows(2), WithBatchMaxBytes(bigdb.DefaultMaxBytes))
	if err != nil {
		t.Fatal(err)
	}
	if n != 2 {
		t.Errorf("statements = %d, want 2", n)
	}
}

func TestExecBatchReportsHowFarItGot(t *testing.T) {
	// The server commits per request, so a batch that fails part way has applied what came
	// before. The count is the only offset there is, and it has to be honest.
	s := &stub{reply: func(n int, _ string) (int, string, string) {
		if n == 1 {
			return 503, "application/json", `{"error":"not now","code":"not_serving"}`
		}
		return 200, "application/json", `{"columns":[],"rows":[]}`
	}}
	db := openStub(t, s)

	done, err := ExecBatch(context.Background(), db, "INSERT INTO t (n) VALUES (?)",
		[][]any{{1}, {2}}, WithBatchMaxRows(1))
	if err == nil {
		t.Fatal("want the failure")
	}
	if done != 1 {
		t.Errorf("statements applied = %d, want 1", done)
	}
}
