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

//go:build integration

package bigdb_test

import (
	"context"
	"errors"
	"math"
	"strings"
	"testing"

	bigdb "github.com/raftio/bigdb/clients/go"
)

func TestEscapingSurvivesTheRoundTrip(t *testing.T) {
	// This is the assertion in contrib/big-message/tests/producer.rs, and it is the one test
	// that proves the escaping rules rather than restating them: the value looks exactly like a
	// statement that would drop the table, and the table has to still be there afterwards.
	ctx := context.Background()
	c := client(t)
	name := table(t, c, map[string]string{"country": "mutex"})

	const nasty = `x'); DROP TABLE tx; --`
	if _, err := c.Import(ctx, name, []bigdb.Fact{
		{Field: "country", Record: 0, Value: nasty},
	}); err != nil {
		t.Fatal(err)
	}

	quoted := bigdb.QuoteText(nasty)
	tbl, err := bigdb.QuoteTable(name)
	if err != nil {
		t.Fatal(err)
	}
	res, err := c.SQL(ctx, "SELECT count(*) FROM "+tbl+" WHERE country = "+quoted)
	if err != nil {
		t.Fatal(err)
	}
	if len(res.Rows) != 1 || string(res.Rows[0][0]) != "1" {
		t.Fatalf("want one match, got %v", res.Rows)
	}

	// And the table is still there, which is the other half of the assertion.
	if _, err := c.Records(ctx, name); err != nil {
		t.Errorf("the table must still exist: %v", err)
	}
}

func TestBindProducesTheSameStatement(t *testing.T) {
	ctx := context.Background()
	c := client(t)
	name := table(t, c, map[string]string{"country": "mutex"})

	const nasty = `x'); DROP TABLE tx; --`
	if _, err := c.Import(ctx, name, []bigdb.Fact{
		{Field: "country", Record: 0, Value: nasty},
	}); err != nil {
		t.Fatal(err)
	}
	tbl, _ := bigdb.QuoteTable(name)
	stmt, err := bigdb.Bind("SELECT count(*) FROM "+tbl+" WHERE country = ?", nasty)
	if err != nil {
		t.Fatal(err)
	}
	res, err := c.SQL(ctx, stmt)
	if err != nil {
		t.Fatal(err)
	}
	if string(res.Rows[0][0]) != "1" {
		t.Errorf("want one match, got %v", res.Rows)
	}
}

func TestAMalformedLineNamesItsLineNumber(t *testing.T) {
	// The refusal has to name which line, because a producer that sent a million needs to know
	// which one. This asserts the server still does that and that the message reaches the
	// caller unmangled.
	ctx := context.Background()
	c := client(t)
	name := table(t, c, map[string]string{"amount": "int"})

	_, err := c.ImportRaw(ctx, name, []byte("amount 0 1\nthis is not a fact\namount 2 3\n"))
	if err == nil {
		t.Fatal("want a refusal")
	}
	var se *bigdb.ServerError
	if !errors.As(err, &se) {
		t.Fatalf("want a *ServerError, got %#v", err)
	}
	if !strings.Contains(se.Message, "2") {
		t.Errorf("the refusal must name line 2: %q", se.Message)
	}
}

func TestAFormatClauseChangesTheContentType(t *testing.T) {
	// The client decides on the header, so this is the test that the header is what it thinks
	// it is.
	ctx := context.Background()
	c := client(t)
	name := table(t, c, map[string]string{"amount": "int"})
	tbl, _ := bigdb.QuoteTable(name)

	json, err := c.SQL(ctx, "SELECT count(*) FROM "+tbl)
	if err != nil {
		t.Fatal(err)
	}
	if json.IsText() {
		t.Error("the default format is a result set")
	}

	csv, err := c.SQL(ctx, "SELECT count(*) FROM "+tbl+" FORMAT CSVWithNames")
	if err != nil {
		t.Fatal(err)
	}
	if !csv.IsText() {
		t.Fatalf("a FORMAT clause answers with text: %#v", csv)
	}
	if !strings.HasPrefix(csv.ContentType, "text/") {
		t.Errorf("content type = %q, want a text type", csv.ContentType)
	}
}

func TestTheSchemaSpellsEveryKindTheWayTheServerDoes(t *testing.T) {
	// The fence against a stale vocabulary. /schema renders format!("{:?}").to_lowercase() over
	// FieldKind, and the create route's parse_kind reads a different set of words - so this
	// asserts both halves at once: each kind is created with the write spelling and read back
	// with the read spelling.
	ctx := context.Background()
	c := client(t)

	// write spelling -> read spelling. The two differ for exactly one kind, which is the whole
	// reason this client never translates.
	kinds := []struct{ write, read string }{
		{"set", "set"},
		{"mutex", "mutex"},
		{"bool", "bool"},
		{"int", "int"},
		{"decimal", "decimal"},
		{"timequantum", "timequantum"},
		{"signed", "signedint"},
		{"float32", "float32"},
		{"float64", "float64"},
		{"date", "date"},
		{"datetime", "datetime"},
	}

	// Every kind except decimal takes the route's default. A decimal without a scale is a 400
	// - "a decimal field needs a scale" - which is the server declining to pick a scale on the
	// caller's behalf, since `price > 5` means `> 500` on a field with two of them.
	name := table(t, c, nil)
	for i, k := range kinds {
		field := "f" + string(rune('a'+i))
		var opts []bigdb.CallOpt
		if k.write == "decimal" {
			opts = append(opts, bigdb.WithScale(2))
		}
		if _, err := c.CreateField(ctx, name, field, k.write, opts...); err != nil {
			t.Fatalf("create field %s %s: %v", field, k.write, err)
		}
	}

	s, err := c.Schema(ctx)
	if err != nil {
		t.Fatal(err)
	}
	tbl := s.Table(name)
	if tbl == nil {
		t.Fatalf("no table %s in the schema", name)
	}
	for i, k := range kinds {
		f := tbl.Field("f" + string(rune('a'+i)))
		if f == nil {
			t.Errorf("%s field missing", k.write)
			continue
		}
		if f.Kind != k.read {
			t.Errorf("created as %q, /schema says %q, want %q", k.write, f.Kind, k.read)
		}
	}

	// And the one that would be silently wrong if the client validated kinds against the read
	// vocabulary: posting the read spelling is a 400.
	if _, err := c.CreateField(ctx, name, "nope", "signedint"); err == nil {
		t.Error("the read vocabulary is not the write vocabulary; signedint must be refused")
	}
}

func TestARecordIDAboveTwoToTheFiftyThreeSurvivesEveryRoute(t *testing.T) {
	// The other end of the encoding/json guard. If anything on this path went through `any`,
	// this id would come back rounded.
	const big = uint64(9007199254740993) // 2^53 + 1
	ctx := context.Background()
	c := client(t)
	name := table(t, c, map[string]string{"amount": "int"})

	if _, err := c.Import(ctx, name, []bigdb.Fact{
		{Field: "amount", Record: big, Value: 42},
	}); err != nil {
		t.Fatal(err)
	}

	page, err := c.Records(ctx, name)
	if err != nil {
		t.Fatal(err)
	}
	found := false
	for _, id := range page.Records {
		if id == big {
			found = true
		}
	}
	if !found {
		t.Errorf("records = %v, want it to contain %d exactly", page.Records, big)
	}

	// And through the query route, which is a different decoder.
	a, err := c.Query(ctx, name, "All()")
	if err != nil {
		t.Fatal(err)
	}
	if ra, ok := a.(*bigdb.RecordsAnswer); ok {
		if len(ra.Records) == 0 || ra.Records[0] != big {
			t.Errorf("query records = %v, want %d", ra.Records, big)
		}
	}
}

func TestKeepAliveSurvivesTheClientsCeiling(t *testing.T) {
	// The client retires at 900 and the server allows 1000, so a run past the client's ceiling
	// has to keep working - and it is the retirement, not the server's, that is being exercised.
	if testing.Short() {
		t.Skip("this makes over nine hundred requests")
	}
	ctx := context.Background()
	c := client(t)

	for i := 0; i < bigdb.MaxRequestsPerConn+50; i++ {
		ok, err := c.Health(ctx)
		if err != nil || !ok {
			t.Fatalf("request %d: %v", i, err)
		}
	}
}

func TestABodyOverTheServersCeilingIs413(t *testing.T) {
	// The client's own ceiling is lower, so this reaches past it deliberately to check that the
	// server's answer is what the client's ceiling is standing in for.
	ctx := context.Background()
	c := client(t, bigdb.WithMaxBytes(16<<20))
	name := table(t, c, map[string]string{"amount": "int"})

	body := strings.Repeat("amount 1 1\n", (9<<20)/11)
	_, err := c.ImportRaw(ctx, name, []byte(body))
	if err == nil {
		t.Fatal("a body over the server's ceiling must be refused")
	}

	// Two shapes are correct here and which one arrives is a race the client does not control.
	// The server checks MAX_BODY against Content-Length before reading the body, so it can
	// answer 413 and close - and if it closes before the client has finished writing nine
	// megabytes, the client sees the write fail instead. That is a NotSent, which is the right
	// reading: the server never took the body, so resending is safe.
	var te *bigdb.TransportError
	switch {
	case errors.Is(err, bigdb.ErrPayloadTooLarge):
	case errors.As(err, &te) && !te.Sent:
	default:
		t.Fatalf("want a 413 or a not-sent write failure, got %#v", err)
	}
}

func TestAPagedCountIsRefusedRatherThanPretendedAt(t *testing.T) {
	ctx := context.Background()
	c := client(t)
	name := table(t, c, map[string]string{"amount": "int"})

	_, err := c.Query(ctx, name, "Count(All())", bigdb.Limit(10))
	if err == nil {
		t.Fatal("a count has no pages")
	}
	var se *bigdb.ServerError
	if errors.As(err, &se) && se.Status != 422 {
		t.Errorf("status = %d, want 422", se.Status)
	}
}

func TestAnUnknownTableAndAnUnknownRouteAreBoth404(t *testing.T) {
	// There is no 405 here, so the code is the only thing that tells a typo in the target from
	// a table that genuinely is not there.
	ctx := context.Background()
	c := client(t)

	_, err := c.Records(ctx, "no_such_table_at_all")
	var se *bigdb.ServerError
	if !errors.As(err, &se) {
		t.Fatalf("want a *ServerError, got %#v", err)
	}
	if se.Status != 404 {
		t.Errorf("status = %d, want 404", se.Status)
	}
	if se.Code == "" {
		t.Error("the code is the only thing that distinguishes the two 404s, so it must be set")
	}
	if !errors.Is(err, bigdb.ErrNotFound) {
		t.Error("and it must match the sentinel")
	}
}

func TestIterRecordsWalksARealTable(t *testing.T) {
	ctx := context.Background()
	c := client(t)
	name := table(t, c, map[string]string{"amount": "int"})

	const n = 250
	facts := make([]bigdb.Fact, 0, n)
	for i := 0; i < n; i++ {
		facts = append(facts, bigdb.Fact{Field: "amount", Record: uint64(i), Value: i})
	}
	if _, err := c.Import(ctx, name, facts); err != nil {
		t.Fatal(err)
	}

	// A page size well under the count, so the cursor is actually exercised.
	seen := 0
	for _, err := range c.IterRecords(ctx, name, bigdb.Limit(30)) {
		if err != nil {
			t.Fatal(err)
		}
		seen++
	}
	if seen != n {
		t.Errorf("walked %d records, want %d", seen, n)
	}
}

func TestImportIsIdempotent(t *testing.T) {
	// The claim the whole retry policy rests on: a fact is a bit set at an address the caller
	// chose, so sending the same batch twice leaves the table as it was after the first.
	ctx := context.Background()
	c := client(t)
	name := table(t, c, map[string]string{"amount": "int"})

	facts := []bigdb.Fact{
		{Field: "amount", Record: 1, Value: 10},
		{Field: "amount", Record: 2, Value: 20},
	}
	for i := 0; i < 3; i++ {
		if _, err := c.Import(ctx, name, facts); err != nil {
			t.Fatal(err)
		}
	}
	page, err := c.Records(ctx, name)
	if err != nil {
		t.Fatal(err)
	}
	if len(page.Records) != 2 {
		t.Errorf("three identical imports left %d records, want 2", len(page.Records))
	}
}

func TestDeleteRemovesRecords(t *testing.T) {
	ctx := context.Background()
	c := client(t)
	name := table(t, c, map[string]string{"amount": "int"})

	if _, err := c.Import(ctx, name, []bigdb.Fact{
		{Field: "amount", Record: 1, Value: 1},
		{Field: "amount", Record: 2, Value: 2},
	}); err != nil {
		t.Fatal(err)
	}
	if _, err := c.DeleteRecords(ctx, name, []uint64{1}); err != nil {
		t.Fatal(err)
	}
	page, err := c.Records(ctx, name)
	if err != nil {
		t.Fatal(err)
	}
	if len(page.Records) != 1 || page.Records[0] != 2 {
		t.Errorf("records = %v, want [2]", page.Records)
	}
}

func TestReadyReportsAVersion(t *testing.T) {
	r, err := client(t).Ready(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if r.Status != "ready" || r.Version == "" {
		t.Errorf("ready = %#v", r)
	}
}

func TestNumbersWithNoSpellingAreRefusedByTheServerToo(t *testing.T) {
	// The client refuses these before sending. This checks the client is refusing the same set
	// the server would, rather than a set it invented - by sending one past the edge by hand.
	ctx := context.Background()
	c := client(t)
	name := table(t, c, map[string]string{"amount": "int"})
	tbl, _ := bigdb.QuoteTable(name)

	// One past u64::MAX, which the client refuses at Literal and the server refuses at the lexer.
	if _, err := bigdb.Literal(bigdb.Decimal("18446744073709551616")); err == nil {
		t.Fatal("the client must refuse this")
	}
	_, err := c.SQL(ctx, "SELECT * FROM "+tbl+" WHERE amount = 18446744073709551616")
	if err == nil {
		t.Error("and so must the server, which is what makes the client's rule the right one")
	}

	// And just inside it is fine at both ends.
	if _, err := bigdb.Literal(uint64(math.MaxUint64)); err != nil {
		t.Errorf("u64::MAX is spellable: %v", err)
	}
}

func TestATableCreatedAfterADropIsStillThere(t *testing.T) {
	// A regression test for a bug this suite found. `schema::snapshot` walked table ids from
	// zero and stopped at the first id the catalog did not answer for, on the stated grounds
	// that tables are "interned from zero upwards and never removed" - which `drop_table` is
	// not. A drop punched a hole, and every table above the hole vanished from /schema and
	// therefore from /import, which resolves a name through that snapshot.
	//
	// What made it hard to see is that nothing errored at the time: POST /table answered with
	// an id, the field route answered with an id, and only the write said the table did not
	// exist. Dropping the very first table emptied the schema outright.
	ctx := context.Background()
	c := client(t)

	first := table(t, c, map[string]string{"amount": "int"})
	if err := c.DropTable(ctx, first); err != nil {
		t.Fatal(err)
	}

	// Everything after the hole. `table` already asserts the schema admits it exists, which is
	// the assertion that used to fail; the write is the consequence that made it matter.
	second := table(t, c, map[string]string{"amount": "int"})
	if _, err := c.Import(ctx, second, []bigdb.Fact{
		{Field: "amount", Record: 1, Value: 42},
	}); err != nil {
		t.Fatalf("a table created after a drop must be writable: %v", err)
	}

	// And a third, so the fix is not "one hole is tolerated".
	third := table(t, c, map[string]string{"amount": "int"})
	s, err := c.Schema(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if s.Table(second) == nil || s.Table(third) == nil {
		t.Errorf("the schema lists %d tables and is missing one of %s / %s",
			len(s.Tables), second, third)
	}
	if s.Table(first) != nil {
		t.Errorf("%s was dropped and must not still be listed", first)
	}
}

func TestDatabaseScopingMeansTheSameThingOnEveryRoute(t *testing.T) {
	// The question the plan left open, now that the server has settled it: ?database= is folded
	// into the table name on every route that takes one, a qualified path wins over a
	// disagreeing parameter, and a bare name means the default database.
	//
	// This client sends ?database= everywhere and also accepts "db.table" in the table
	// argument, so both spellings are exercised here - if the two ever stop meaning the same
	// thing, this is where it shows up rather than in somebody's ingest job.
	ctx := context.Background()
	root := client(t)

	const db = "goscoping"
	if err := root.CreateDatabase(ctx, db); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = root.DropDatabase(context.Background(), db, bigdb.Cascade()) })

	// Spelling one: the database on the client, a bare table name everywhere.
	scoped := client(t, bigdb.WithDatabase(db))
	const name = "orders"
	if _, err := scoped.CreateTable(ctx, name); err != nil {
		t.Fatal(err)
	}
	if _, err := scoped.CreateField(ctx, name, "amount", "int"); err != nil {
		t.Fatal(err)
	}
	if _, err := scoped.Import(ctx, name, []bigdb.Fact{
		{Field: "amount", Record: 1, Value: 10},
	}); err != nil {
		t.Fatalf("import must reach the database the client named: %v", err)
	}
	page, err := scoped.Records(ctx, name)
	if err != nil {
		t.Fatal(err)
	}
	if len(page.Records) != 1 || page.Records[0] != 1 {
		t.Errorf("records = %v, want [1]", page.Records)
	}

	// Spelling two: no database on the client, a qualified name in the argument. Same table.
	qualified := client(t)
	page, err = qualified.Records(ctx, db+"."+name)
	if err != nil {
		t.Fatalf("a qualified name must reach the same table: %v", err)
	}
	if len(page.Records) != 1 {
		t.Errorf("the two spellings disagree: %v", page.Records)
	}
	if _, err := qualified.Import(ctx, db+"."+name, []bigdb.Fact{
		{Field: "amount", Record: 2, Value: 20},
	}); err != nil {
		t.Fatalf("import by qualified name must work too: %v", err)
	}

	// The more specific of the two wins: a qualified path beats a parameter that disagrees.
	page, err = qualified.Records(ctx, db+"."+name, bigdb.InDatabase("nosuchdatabase"))
	if err != nil {
		t.Fatalf("the path must win over a disagreeing parameter: %v", err)
	}
	if len(page.Records) != 2 {
		t.Errorf("records = %v, want both", page.Records)
	}

	// And a bare name is the default database, which is a different table entirely - so a
	// write scoped to one database must not be visible from the other.
	if _, err := qualified.Records(ctx, name); err == nil {
		t.Error("a bare `orders` is the default database's, and there is no table there")
	}
}
