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
	"errors"
	"strings"
	"testing"
	"time"
)

func TestBindSubstitutesOnlyTopLevelPlaceholders(t *testing.T) {
	for _, c := range []struct {
		name string
		stmt string
		args []any
		want string
	}{
		{"plain", "SELECT * FROM t WHERE a = ?", []any{1}, "SELECT * FROM t WHERE a = 1"},
		{"text", "WHERE a = ?", []any{"O'Brien"}, "WHERE a = 'O''Brien'"},
		// A ? inside a literal is data. Substituting it would corrupt the caller's string and,
		// worse, shift every later placeholder by one.
		{"inside a literal", "WHERE a = 'is it? yes' AND b = ?", []any{2},
			"WHERE a = 'is it? yes' AND b = 2"},
		// A doubled quote is one quote and the literal continues, so the ? is still inside it.
		{"doubled quote", "WHERE a = 'it''s? no' AND b = ?", []any{2},
			"WHERE a = 'it''s? no' AND b = 2"},
		{"inside an identifier", `SELECT "why?" FROM t WHERE a = ?`, []any{3},
			`SELECT "why?" FROM t WHERE a = 3`},
		{"doubled dquote", `SELECT "a""?b" FROM t WHERE a = ?`, []any{3},
			`SELECT "a""?b" FROM t WHERE a = 3`},
		// -- is the only comment form this dialect has, so there is no /* */ state.
		{"inside a comment", "SELECT 1 -- what? why?\nWHERE a = ?", []any{4},
			"SELECT 1 -- what? why?\nWHERE a = 4"},
		{"several", "WHERE a = ? AND b = ? AND c = ?", []any{1, "x", true},
			"WHERE a = 1 AND b = 'x' AND c = TRUE"},
		{"none", "SELECT 1", nil, "SELECT 1"},
	} {
		t.Run(c.name, func(t *testing.T) {
			got, err := Bind(c.stmt, c.args...)
			if err != nil {
				t.Fatal(err)
			}
			if got != c.want {
				t.Errorf("Bind = %q, want %q", got, c.want)
			}
		})
	}
}

func TestBindNamesBothNumbersOnAMismatch(t *testing.T) {
	// "wrong number of arguments" without the numbers sends the caller back to count by hand.
	_, err := Bind("WHERE a = ? AND b = ?", 1)
	if err == nil {
		t.Fatal("want an error")
	}
	if !strings.Contains(err.Error(), "2") || !strings.Contains(err.Error(), "1") {
		t.Errorf("the message must name both counts: %v", err)
	}
	if _, err := Bind("SELECT 1", 1); err == nil {
		t.Error("too many arguments is a mismatch too")
	}
}

func TestBindNamesWhichArgumentWasRefused(t *testing.T) {
	// A statement with nine arguments and one refusal is a sentence the caller has to bisect
	// otherwise.
	_, err := Bind("WHERE a = ? AND b = ? AND c = ?", 1, nil, 3)
	if err == nil {
		t.Fatal("want an error")
	}
	if !strings.Contains(err.Error(), "argument 2") {
		t.Errorf("the message must name the argument: %v", err)
	}
}

func TestCountPlaceholders(t *testing.T) {
	for _, c := range []struct {
		stmt string
		want int
	}{
		{"SELECT 1", 0},
		{"WHERE a = ?", 1},
		{"WHERE a = ? AND b = ?", 2},
		{"WHERE a = 'x?y'", 0},
		{"-- ?\nWHERE a = ?", 1},
	} {
		if got := CountPlaceholders(c.stmt); got != c.want {
			t.Errorf("CountPlaceholders(%q) = %d, want %d", c.stmt, got, c.want)
		}
	}
}

func TestLastTopLevelValues(t *testing.T) {
	for _, c := range []struct {
		name string
		stmt string
		want bool
	}{
		{"plain", "INSERT INTO t (a) VALUES (1)", true},
		{"lower", "insert into t (a) values (1)", true},
		// A column called `values` is a Tok::Word, and it is not the keyword.
		{"quoted column", `INSERT INTO t ("values") VALUES (1)`, true},
		{"in a literal", "INSERT INTO t (a) VALUES ('values')", true},
		{"absent", "SELECT * FROM t", false},
		// Not a word boundary, so not the keyword.
		{"substring", "SELECT myvalues FROM t", false},
	} {
		t.Run(c.name, func(t *testing.T) {
			got := lastTopLevelValues(c.stmt)
			if (got >= 0) != c.want {
				t.Errorf("lastTopLevelValues(%q) = %d", c.stmt, got)
			}
		})
	}
	// The last one, not the first: a merged batch appends after it.
	stmt := "INSERT INTO t (a) VALUES (1) -- VALUES"
	if i := lastTopLevelValues(stmt); i < 0 || strings.TrimSpace(stmt[i:]) != "(1) -- VALUES" {
		t.Errorf("a comment must not move the keyword: %d", i)
	}
}

func TestOpsBuildTheExactTarget(t *testing.T) {
	for _, c := range []struct {
		name       string
		op         op
		wantMethod string
		wantTarget string
		idempotent bool
	}{
		{"sql", opSQL("SELECT 1", callOptions{database: "sales"}),
			"POST", "/sql?database=sales", false},
		// The database is folded into the path as well as sent as a parameter, because the DDL
		// routes read only the path and the data routes read both.
		{"query in a database", opQuery("tx", "All()", callOptions{database: "sales"}),
			"POST", "/table/sales.tx/query?database=sales", true},
		{"create table in a database", opCreateTable("tx", callOptions{database: "sales"}),
			"POST", "/table/sales.tx?database=sales", false},
		{"create field in a database", opCreateField("tx", "a", "int", callOptions{database: "sales"}),
			"POST", "/table/sales.tx/field/a?database=sales&kind=int", false},
		// A name the caller already qualified is more specific than the client's default, and
		// wins - the same rule the server applies.
		{"an explicit database wins", opRecords("other.tx", callOptions{database: "sales"}),
			"GET", "/table/other.tx/records?database=sales", true},
		{"query", opQuery("tx", "Count(All())", callOptions{}),
			"POST", "/table/tx/query", true},
		{"query paged", opQuery("tx", "All()", callOptions{limit: 10}),
			"POST", "/table/tx/query?limit=10", true},
		{"import", opImport("tx", nil, callOptions{}),
			"POST", "/table/tx/import", true},
		{"delete", opDelete("tx", nil, callOptions{}),
			"POST", "/table/tx/delete", true},
		{"records", opRecords("tx", callOptions{limit: 5}),
			"GET", "/table/tx/records?limit=5", true},
		{"schema", opSchema(callOptions{}), "GET", "/schema", true},
		{"health", opHealth(), "GET", "/health", true},
		{"ready", opReady(), "GET", "/ready", true},
		// The plus must survive: the server deliberately does not read `+` as a space, so an
		// engine called bitmap+columnar has to arrive with %2B.
		{"create table", opCreateTable("tx", callOptions{engine: "bitmap+columnar"}),
			"POST", "/table/tx?engine=bitmap%2Bcolumnar", false},
		{"drop table", opDropTable("tx", callOptions{}), "DELETE", "/table/tx", false},
		{"create field", opCreateField("tx", "amount", "signed", callOptions{}),
			"POST", "/table/tx/field/amount?kind=signed", false},
		{"drop field", opDropField("tx", "amount", callOptions{}),
			"DELETE", "/table/tx/field/amount", false},
		{"create database", opCreateDatabase("sales"), "POST", "/database/sales", false},
		{"drop database", opDropDatabase("sales", callOptions{cascade: true}),
			"DELETE", "/database/sales?cascade=true", false},
		// A qualified name is one path segment: the dot is unreserved, so it survives, and the
		// server splits on it itself.
		{"qualified table", opRecords("sales.orders", callOptions{}),
			"GET", "/table/sales.orders/records", true},
	} {
		t.Run(c.name, func(t *testing.T) {
			if c.op.Method != c.wantMethod {
				t.Errorf("method = %s, want %s", c.op.Method, c.wantMethod)
			}
			if c.op.Target != c.wantTarget {
				t.Errorf("target = %s, want %s", c.op.Target, c.wantTarget)
			}
			if c.op.Idempotent != c.idempotent {
				t.Errorf("idempotent = %v, want %v", c.op.Idempotent, c.idempotent)
			}
		})
	}
}

func TestTheIdempotencyTableIsWhatTheRetryPolicyReads(t *testing.T) {
	// Stated once here so the table can be read at a glance, because it is the single input
	// that decides whether an unknown outcome is repeated.
	idempotent := []op{
		opQuery("t", "", callOptions{}), opImport("t", nil, callOptions{}),
		opDelete("t", nil, callOptions{}), opRecords("t", callOptions{}),
		opSchema(callOptions{}), opHealth(), opReady(),
	}
	notIdempotent := []op{
		opSQL("", callOptions{}),
		opCreateTable("t", callOptions{}), opDropTable("t", callOptions{}),
		opCreateField("t", "f", "int", callOptions{}), opDropField("t", "f", callOptions{}),
		opCreateDatabase("d"), opDropDatabase("d", callOptions{}),
	}
	for _, o := range idempotent {
		if !o.Idempotent {
			t.Errorf("%s must be idempotent", o.Name)
		}
	}
	for _, o := range notIdempotent {
		if o.Idempotent {
			t.Errorf("%s must not be idempotent", o.Name)
		}
	}
}

func TestOpRefusesABodyOverTheCeilingBeforeTheSocketOpens(t *testing.T) {
	o := opImport("tx", make([]byte, 100), callOptions{})
	err := o.check(50)
	var tl *TooLargeError
	if !errors.As(err, &tl) {
		t.Fatalf("want a *TooLargeError, got %#v", err)
	}
	if tl.Bytes != 100 || tl.Cap != 50 {
		t.Errorf("the refusal must name both sizes: %#v", tl)
	}
	if !errors.Is(err, ErrPayloadTooLarge) {
		t.Error("and it must read as the 413 it is standing in for")
	}
}

func TestDecideFollowsTheTable(t *testing.T) {
	never := func(int) time.Duration { return 0 }

	for _, c := range []struct {
		name       string
		err        error
		idempotent bool
		want       bool
	}{
		{"not sent, non-idempotent", &TransportError{Sent: false}, false, true},
		{"not sent, idempotent", &TransportError{Sent: false}, true, true},
		// The one that matters most: an INSERT whose outcome is unknown must never be repeated.
		{"unknown, non-idempotent", &TransportError{Sent: true}, false, false},
		{"unknown, idempotent", &TransportError{Sent: true}, true, true},
		{"protocol, non-idempotent", &ProtocolError{}, false, false},
		{"protocol, idempotent", &ProtocolError{}, true, true},
		{"503", &ServerError{Status: 503, Code: "server_busy"}, false, true},
		{"504", &ServerError{Status: 504, Code: "query_timeout"}, true, false},
		{"partially applied", &ServerError{Status: 500, Code: "partially_applied"}, true, false},
		{"404", &ServerError{Status: 404, Code: "unknown_table"}, true, false},
		// The caller said stop, and nothing overrides that.
		{"cancelled", context.Canceled, true, false},
		{"deadline", context.DeadlineExceeded, true, false},
		// This client's own refusals do not get better on a second try.
		{"value refused", &ValueError{}, true, false},
		{"too large", &TooLargeError{}, true, false},
	} {
		t.Run(c.name, func(t *testing.T) {
			d := decide(c.err, c.idempotent, 0, 3, never)
			if d.Retry != c.want {
				t.Errorf("retry = %v, want %v", d.Retry, c.want)
			}
		})
	}
}

func TestDecideStopsAtTheAttemptCeiling(t *testing.T) {
	err := &TransportError{Sent: false}
	if d := decide(err, false, 2, 3, backoff); !d.Retry {
		t.Error("attempt 2 of 3 is still within the allowance")
	}
	if d := decide(err, false, 3, 3, backoff); d.Retry {
		t.Error("attempt 3 of 3 has used the allowance")
	}
}

func TestRetryAfterIsAFloorNotAReplacement(t *testing.T) {
	// A server that asked for two seconds gets at least two; the backoff still applies when it
	// asked for less.
	long := &ServerError{Status: 503, RetryAfter: 2 * time.Second}
	if d := decide(long, false, 0, 3, func(int) time.Duration { return time.Millisecond }); d.Wait != 2*time.Second {
		t.Errorf("wait = %v, want the server's 2s", d.Wait)
	}
	short := &ServerError{Status: 503, RetryAfter: time.Millisecond}
	if d := decide(short, false, 0, 3, func(int) time.Duration { return time.Second }); d.Wait != time.Second {
		t.Errorf("wait = %v, want the backoff's 1s", d.Wait)
	}
}

func TestParseAddress(t *testing.T) {
	for _, c := range []struct {
		in   string
		host string
		port string
		tls  bool
	}{
		{"127.0.0.1:7654", "127.0.0.1", "7654", false},
		{"http://example.com:7654", "example.com", "7654", false},
		{"https://example.com:7654", "example.com", "7654", true},
		{"https://example.com:7654/", "example.com", "7654", true},
		{"[::1]:7654", "::1", "7654", false},
		{"https://[2001:db8::1]:7654", "2001:db8::1", "7654", true},
	} {
		a, err := ParseAddress(c.in)
		if err != nil {
			t.Fatalf("%s: %v", c.in, err)
		}
		if a.Host != c.host || a.Port != c.port || a.TLS != c.tls {
			t.Errorf("%s = %#v", c.in, a)
		}
	}

	// An IPv6 dial string comes back bracketed, which is what net.Dial wants.
	a, _ := ParseAddress("[::1]:7654")
	if a.Dial() != "[::1]:7654" {
		t.Errorf("Dial = %q", a.Dial())
	}

	for _, bad := range []string{
		"", "example.com", "ftp://example.com:1", "http://example.com:7654/table/tx",
		"example.com:", ":7654",
	} {
		if _, err := ParseAddress(bad); err == nil {
			t.Errorf("%q must not parse", bad)
		}
	}
}

func TestLoopbackIsWhereAPlaintextPasswordIsNormal(t *testing.T) {
	for _, c := range []struct {
		addr string
		want bool
	}{
		{"127.0.0.1:7654", true},
		{"[::1]:7654", true},
		{"localhost:7654", true},
		{"example.com:7654", false},
		{"10.0.0.1:7654", false},
	} {
		a, err := ParseAddress(c.addr)
		if err != nil {
			t.Fatal(err)
		}
		if got := a.IsLoopback(); got != c.want {
			t.Errorf("%s IsLoopback = %v, want %v", c.addr, got, c.want)
		}
	}
}

func TestEscapeSegmentMatchesTheServersDecoder(t *testing.T) {
	// Both of these are the server's own test cases in big_http::request::decode_tests.
	for _, c := range []struct{ in, want string }{
		{"báo cáo", "b%C3%A1o%20c%C3%A1o"},
		// `+` must be escaped: the server deliberately does not read it as a space, so the
		// default engine name was unreachable over HTTP until it was.
		{"bitmap+columnar", "bitmap%2Bcolumnar"},
		{"tx", "tx"},
		{"sales.orders", "sales.orders"},
		{"a-b_c~d", "a-b_c~d"},
		{"a/b", "a%2Fb"},
		{"a?b=c&d", "a%3Fb%3Dc%26d"},
	} {
		if got := EscapeSegment(c.in); got != c.want {
			t.Errorf("EscapeSegment(%q) = %q, want %q", c.in, got, c.want)
		}
	}
}

func TestRenderFactsNamesTheLineThatCrossedTheCeiling(t *testing.T) {
	// A producer that sent a million needs to know which one, not that there was one.
	facts := []Fact{
		{Field: "a", Record: 1, Value: 1},
		{Field: "a", Record: 2, Value: 2},
		{Field: "a", Record: 3, Value: 3},
	}
	_, err := RenderFacts(facts, 14) // two lines of "a 1 1\n" fit, the third does not
	var tl *TooLargeError
	if !errors.As(err, &tl) {
		t.Fatalf("want a *TooLargeError, got %#v", err)
	}
	if tl.Line != 3 {
		t.Errorf("line = %d, want 3", tl.Line)
	}
}

func TestRenderFacts(t *testing.T) {
	got, err := RenderFacts([]Fact{
		{Field: "amount", Record: 0, Value: 1250},
		{Field: "country", Record: 0, Value: "gb"},
		{Field: "active", Record: 1, Value: true},
		{Field: "seen", Record: 2, Value: Keyed{Key: "gb", At: 1750000000}},
	}, DefaultMaxBytes)
	if err != nil {
		t.Fatal(err)
	}
	want := "amount 0 1250\ncountry 0 gb\nactive 1 true\nseen 2 gb@1750000000\n"
	if string(got) != want {
		t.Errorf("RenderFacts =\n%q\nwant\n%q", got, want)
	}
}

func TestAFactValueCannotCarryTheFraming(t *testing.T) {
	// A newline ends the line and there is no escape in this format, so it cannot be carried.
	// Whitespace at either end is trimmed by the server before the split, so a value carrying
	// it would arrive different from the one that was sent - which is worse than a refusal.
	for _, bad := range []any{"a\nb", "a\rb", " a", "a ", "a\t", ""} {
		if _, err := FactValue(bad); err == nil {
			t.Errorf("%q must not reach a fact line", bad)
		}
	}
	if _, err := FactValue(nil); err == nil {
		t.Error("a fact with no value must be refused")
	}

	// But an interior space is data, not framing. big_http::routes::query::three cuts the line
	// at its first two spaces and takes the whole rest as the value, deliberately - "the value
	// keeps whatever spaces it contains, which is what a keyed value needs". Refusing these
	// would make an ordinary value unwritable, and it is exactly what this client used to do.
	for _, fine := range []string{"a b", "x'); DROP TABLE tx; --", "a b c", "a\tb"} {
		got, err := FactValue(fine)
		if err != nil {
			t.Errorf("%q is a value this format carries: %v", fine, err)
		}
		if got != fine {
			t.Errorf("FactValue(%q) = %q, want it unchanged", fine, got)
		}
	}
}

func TestRenderRecords(t *testing.T) {
	got, err := RenderRecords([]uint64{1, 2, 18446744073709551615}, DefaultMaxBytes)
	if err != nil {
		t.Fatal(err)
	}
	if want := "1\n2\n18446744073709551615\n"; string(got) != want {
		t.Errorf("RenderRecords = %q, want %q", got, want)
	}
}

func TestQualifyFoldsTheDatabaseIntoTheName(t *testing.T) {
	// One meaning for "which database" across every route. The parameter alone would not do:
	// the DDL routes ignore it, so a client that set a database would create a table in the
	// default one and then write to another, with a 200 at every step.
	for _, c := range []struct {
		table, database, want string
	}{
		{"tx", "sales", "sales.tx"},
		{"tx", "", "tx"},
		// Already qualified: the caller was more specific than the client's default.
		{"other.tx", "sales", "other.tx"},
		{"other.tx", "", "other.tx"},
	} {
		if got := qualify(c.table, c.database); got != c.want {
			t.Errorf("qualify(%q, %q) = %q, want %q", c.table, c.database, got, c.want)
		}
	}
}
