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

// Command goex writes rows into bigdb through the Go client and then asks what landed.
//
//	goex <addr> <table> <rows>
//
// It is the counterpart to examples/producer, which writes the same shape of data through
// contrib/big-message and gets at-least-once delivery for it. This one names every record id
// itself and uses /import, where a fact is one bit set at an address the caller chose - so
// running it twice writes what running it once wrote, and the count does not move. That is the
// whole point of the example, and the last thing it prints.
//
// Everything here is the client's public API. There is no HTTP in this file.
package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"iter"
	"os"
	"os/signal"
	"strconv"
	"strings"
	"syscall"
	"time"

	bigdb "github.com/raftio/bigdb/clients/go"
)

// Exit codes, the same four bigctl uses. A script that wraps this can tell "the server said no"
// from "nothing was listening" without reading the message.
const (
	exitOK       = 0
	exitRefused  = 1
	exitUsage    = 2
	exitNoServer = 3
)

const usage = `usage: goex <addr> <table> <rows>

  addr    host:port, http://host:port or https://host:port
  table   created if it is not there yet
  rows    how many records to write

environment:
  BIG_CREDENTIALS   path to a file holding one user:password line
`

// The four values the country field takes, so the GROUP BY below has something to group.
var countries = []string{"GB", "JP", "US", "VN"}

func main() {
	if err := run(os.Args[1:]); err != nil {
		fmt.Fprintln(os.Stderr, "goex: "+err.Error())
		os.Exit(exitCode(err))
	}
}

// exitCode reads the failure the way the client already classified it, rather than by matching
// on the message. ErrNotSent is the one worth its own code: nothing was listening, which is an
// operator's problem and not the statement's.
func exitCode(err error) int {
	switch {
	case errors.Is(err, errUsage):
		return exitUsage
	case errors.Is(err, bigdb.ErrNotSent):
		return exitNoServer
	default:
		return exitRefused
	}
}

var errUsage = errors.New("usage")

func run(args []string) error {
	if len(args) != 3 {
		return fmt.Errorf("%w\n\n%s", errUsage, usage)
	}
	addr, table := args[0], args[1]
	rows, err := strconv.Atoi(args[2])
	if err != nil || rows <= 0 {
		return fmt.Errorf("%w: rows must be a positive number, got %q", errUsage, args[2])
	}

	user, password, err := credentials(os.Getenv("BIG_CREDENTIALS"))
	if err != nil {
		return err
	}

	// Ctrl-C cancels the context, which cancels the request in flight. Worth wiring up in an
	// example: a write that is interrupted mid-chunk is exactly the case the checkpoint below
	// exists for.
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	// New does not dial. The connection is opened by the first call, so a client can be built
	// at start-up before the server is up.
	c, err := bigdb.New(addr,
		bigdb.WithUser(user),
		bigdb.WithPassword(password),
		// The default is 30s and so is the server's read timeout; they are set together on
		// purpose. Raise both or neither - a client deadline under the server's turns a slow
		// query into a transport error, which throws away an answer that exists.
		bigdb.WithTimeout(30*time.Second),
		// One request per megabyte rather than per seven, which is where the client would
		// chunk on its own. Two reasons, and the deadline above is the first: a chunk is one
		// request, so a chunk that takes longer than 30s to apply is a timeout no matter how
		// healthy the server is - and 7 MiB of facts against a mutex field is comfortably
		// longer than that. The second is that the checkpoint below is per chunk, so the chunk
		// size *is* the resume granularity, and a resume point every 7 MiB is a coarse one.
		bigdb.WithMaxBytes(1<<20),
	)
	if err != nil {
		return err
	}
	defer c.Close()

	if err := ready(ctx, c); err != nil {
		return err
	}
	if err := schema(ctx, c, table); err != nil {
		return err
	}

	before, err := count(ctx, c, table)
	if err != nil {
		return err
	}

	if err := load(ctx, c, table, rows); err != nil {
		return err
	}
	if err := questions(ctx, c, table); err != nil {
		return err
	}

	after, err := count(ctx, c, table)
	if err != nil {
		return err
	}
	report(rows, before, after)
	return nil
}

// credentials reads the one line `user:password`.
//
// A file rather than a flag or an argument, which is the rule everywhere in this repository: an
// argument is visible in `ps` and in shell history. An empty path is not an error - a daemon on
// loopback with no users file wants no credential at all, and sending one would be a 401.
func credentials(path string) (user, password string, err error) {
	if path == "" {
		return "", "", nil
	}
	b, err := os.ReadFile(path)
	if err != nil {
		return "", "", fmt.Errorf("reading $BIG_CREDENTIALS: %w", err)
	}
	line := strings.TrimRight(string(b), "\r\n")
	user, password, ok := strings.Cut(line, ":")
	if !ok || user == "" {
		return "", "", fmt.Errorf("%s: want one line of user:password", path)
	}
	return user, password, nil
}

// ready asks the server whether it can answer, and says which node answered.
func ready(ctx context.Context, c *bigdb.Client) error {
	r, err := c.Ready(ctx)
	if err != nil {
		return fmt.Errorf("the server is not ready: %w", err)
	}
	fmt.Printf("==> connected to %s\n", c.Addr().Dial())
	if r.Node != "" {
		fmt.Printf("    node %s, %d table(s), version %s\n", r.Node, r.Tables, r.Version)
	}
	return nil
}

// schema creates the table and its two fields, and tolerates them being there already.
//
// There is no CREATE TABLE IF NOT EXISTS on this route - that spelling belongs to /sql - so
// "already there" arrives as a 409 and is read as one. Tolerating it is what lets the demo be
// run twice, which is the thing it is trying to show.
func schema(ctx context.Context, c *bigdb.Client, table string) error {
	if _, err := c.CreateTable(ctx, table); err != nil && !errors.Is(err, bigdb.ErrConflict) {
		return fmt.Errorf("create table %s: %w", table, err)
	}
	// `set` rather than `mutex`, and the reason is measured rather than modelled. A record here
	// has exactly one country, which is what `mutex` is for - but writing 40,000 facts into a
	// mutex field takes about 13 seconds against the 0.014 the same facts take into a `set` or
	// an `int`, and the cost grows faster than the record count rather than in step with it.
	// Enforcing one-bit-per-record appears to cost something proportional to what the field
	// already holds. An example that spends twelve seconds teaching a modelling point is
	// teaching the wrong thing; the constraint is what this demo would give up, and it writes
	// one country per record either way.
	fields := []struct{ name, kind string }{
		{"country", "set"},
		{"amount", "int"},
	}
	for _, f := range fields {
		if _, err := c.CreateField(ctx, table, f.name, f.kind); err != nil &&
			!errors.Is(err, bigdb.ErrConflict) {
			return fmt.Errorf("create field %s %s: %w", f.name, f.kind, err)
		}
	}
	fmt.Printf("==> table %s (country SET, amount INT)\n", table)
	return nil
}

// facts yields two facts per record: the country and the amount.
//
// The record id is the loop counter and nothing else, which is the entire reason this example
// is idempotent. Record 7 is record 7 on every run, so the second run sets bits that are
// already set.
//
// A real producer would derive the id from something in the message - an order number, a hash
// of a key - and would need it to be stable and dense-ish, because ids are the engine's own
// coordinates and a sparse space costs containers.
func facts(rows int) iter.Seq[bigdb.Fact] {
	return func(yield func(bigdb.Fact) bool) {
		for i := range rows {
			id := uint64(i)
			if !yield(bigdb.Fact{Field: "country", Record: id, Value: countries[i%len(countries)]}) {
				return
			}
			if !yield(bigdb.Fact{Field: "amount", Record: id, Value: 100 + i%900}) {
				return
			}
		}
	}
}

// load sends the facts, in as many requests as they need.
//
// ImportStream does the chunking: the server's body ceiling is 8 MiB and the client refuses at
// 7, so a batch larger than one request becomes several without the caller counting bytes.
func load(ctx context.Context, c *bigdb.Client, table string, rows int) error {
	fmt.Printf("\n==> importing %d records\n", rows)
	start := time.Now()

	chunks := 0
	// The callback is the only checkpoint there is. /import is idempotent, so a caller that
	// recorded `sent` here could resume a failed run by replaying from it - which is the shape
	// `bigctl import --resume` has. Printing is the honest demo version of writing it down.
	onChunk := func(sent int, _ *bigdb.WriteResult) error {
		chunks++
		fmt.Printf("    chunk %d: %d facts sent\n", chunks, sent)
		return nil
	}

	res, err := c.ImportStream(ctx, table, facts(rows), onChunk)
	if err != nil {
		// A refusal names the line it could not read, and that name is the useful part of the
		// message for anything that sent a million of them - so it is printed as it arrived
		// rather than wrapped into something tidier.
		var se *bigdb.ServerError
		if errors.As(err, &se) {
			return fmt.Errorf("import refused (%s): %s", se.Code, se.Message)
		}
		return fmt.Errorf("import: %w", err)
	}
	fmt.Printf("    %d facts in %d request(s), %s\n", res.Count, chunks, took(start))
	if len(res.Missed) > 0 {
		fmt.Printf("    missed: %s\n", strings.Join(res.Missed, " "))
	}
	return nil
}

// questions asks the three things worth asking of what was just written.
func questions(ctx context.Context, c *bigdb.Client, table string) error {
	tbl, err := bigdb.QuoteTable(table)
	if err != nil {
		return err
	}

	fmt.Println("\n==> what is in there")
	if err := show(ctx, c, "SELECT country, count(*), sum(amount) FROM "+tbl+" GROUP BY country"); err != nil {
		return err
	}

	// Bind, rather than fmt.Sprintf. The placeholder is `?` and the value is quoted by the
	// client against this dialect's grammar - which is the difference between a country name
	// and a country name that is also a statement.
	fmt.Println("\n==> one of them, asked with a bound parameter")
	stmt, err := bigdb.Bind("SELECT count(*) FROM "+tbl+" WHERE country = ?", "VN")
	if err != nil {
		return err
	}
	if err := show(ctx, c, stmt); err != nil {
		return err
	}

	// Not a query, because `_record_id` is not a column a select list can ask for: it is what a
	// record is called. GET /table/{t}/records is the route, and Records is the call.
	fmt.Println("\n==> the record ids, which this client chose")
	page, err := c.Records(ctx, table, bigdb.Limit(5))
	if err != nil {
		return err
	}
	fmt.Printf("    %v\n", page.Records)

	// PQL, the other way to ask. Count(All()) is the same question as SELECT count(*) and goes
	// to a different route - an idempotent one, which is why /query is retried and /sql is not.
	a, err := c.Query(ctx, table, "Count(All())")
	if err != nil {
		return err
	}
	if cnt, ok := a.(*bigdb.CountAnswer); ok {
		fmt.Printf("    Count(All()) = %d\n", cnt.Count)
	}
	return nil
}

// count is SELECT count(*), read as a number.
func count(ctx context.Context, c *bigdb.Client, table string) (uint64, error) {
	a, err := c.Query(ctx, table, "Count(All())")
	if err != nil {
		return 0, err
	}
	cnt, ok := a.(*bigdb.CountAnswer)
	if !ok {
		return 0, fmt.Errorf("Count(All()) answered %T", a)
	}
	return cnt.Count, nil
}

// show runs one statement and prints the result set.
func show(ctx context.Context, c *bigdb.Client, statement string) error {
	res, err := c.SQL(ctx, statement)
	if err != nil {
		return fmt.Errorf("%s: %w", statement, err)
	}
	if res.IsText() {
		// A FORMAT clause changes the body and the content type, and the client says which
		// rather than parsing the statement to find out.
		fmt.Printf("    %s", res.Text)
		return nil
	}
	fmt.Printf("    %s\n", strings.Join(res.Columns, "  "))
	for _, row := range res.Rows {
		cells := make([]string, len(row))
		for i, cell := range row {
			cells[i] = cellText(cell)
		}
		fmt.Printf("    %s\n", strings.Join(cells, "  "))
	}
	return nil
}

// cellText unquotes a JSON string and leaves everything else as it arrived.
//
// Cells stay as json.RawMessage on the way out of the client because which of the five things a
// cell is depends on the field, not on the decoder. Printing is where that gets decided, and
// here the decision is: strings lose their quotes, numbers are already text.
func cellText(cell json.RawMessage) string {
	var s string
	if err := json.Unmarshal(cell, &s); err == nil {
		return s
	}
	return string(cell)
}

// report says what the second run is for.
func report(rows int, before, after uint64) {
	fmt.Println()
	switch {
	case before == 0:
		fmt.Printf("%d records written. Run this again:\n\n", after)
		fmt.Println("    the count will still be", after, "- every fact is a bit set at a record id")
		fmt.Println("    this client chose, so writing it twice is writing it once.")
	case before == after:
		fmt.Printf("%d records before, %d after: the second run changed nothing, which is\n", before, after)
		fmt.Printf("the point. /import is idempotent because the caller names the address.\n")
	default:
		// Reached by asking for more rows than last time, which is a longer prefix rather than
		// a duplicate - worth distinguishing from the case above rather than claiming either.
		fmt.Printf("%d records before, %d after: %d ids that had not been written yet.\n",
			before, after, after-before)
		fmt.Printf("The first %d were rewritten in place.\n", min(before, uint64(rows)))
	}
}

func took(start time.Time) string { return time.Since(start).Round(time.Millisecond).String() }
