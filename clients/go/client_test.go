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
	"math"
	"slices"
	"strings"
	"sync"
	"testing"
)

// The whole public surface, driven over a stub transport: no socket, and the exact target and
// body of every call is assertable.

type recorder struct {
	mu    sync.Mutex
	calls []call
	reply func(n int, c call) (*RawResponse, error)
}

type call struct {
	Method string
	Target string
	Body   string
}

func (r *recorder) Do(_ context.Context, method, target string, body []byte) (*RawResponse, error) {
	r.mu.Lock()
	n := len(r.calls)
	r.calls = append(r.calls, call{method, target, string(body)})
	r.mu.Unlock()

	if r.reply != nil {
		return r.reply(n, call{method, target, string(body)})
	}
	return jsonResp(200, `{}`), nil
}

func jsonResp(status int, body string) *RawResponse {
	h := make(map[string][]string)
	h["Content-Type"] = []string{"application/json"}
	return &RawResponse{Status: status, Header: h, Body: []byte(body)}
}

func stubClient(t *testing.T, r *recorder, opts ...Option) *Client {
	t.Helper()
	c, err := New(DefaultAddr, append([]Option{WithDoer(r), WithRetries(0)}, opts...)...)
	if err != nil {
		t.Fatal(err)
	}
	return c
}

func TestEveryCallBuildsTheTargetItShould(t *testing.T) {
	ctx := context.Background()

	for _, c := range []struct {
		name   string
		body   string
		run    func(*Client) error
		method string
		target string
		want   string // request body
	}{
		{"sql", `{"columns":[],"rows":[]}`,
			func(c *Client) error { _, err := c.SQL(ctx, "SELECT 1"); return err },
			"POST", "/sql?database=sales", "SELECT 1"},
		{"query", `{"count":1}`,
			func(c *Client) error { _, err := c.Query(ctx, "tx", "Count(All())"); return err },
			"POST", "/table/sales.tx/query?database=sales", "Count(All())"},
		{"query in another database", `{"count":1}`,
			func(c *Client) error {
				_, err := c.Query(ctx, "tx", "All()", InDatabase("other"), Limit(5), After(9))
				return err
			},
			"POST", "/table/other.tx/query?database=other&after=9&limit=5", "All()"},
		{"import", `{"imported":1}`,
			func(c *Client) error {
				_, err := c.Import(ctx, "tx", []Fact{{Field: "amount", Record: 0, Value: 1250}})
				return err
			},
			"POST", "/table/sales.tx/import?database=sales", "amount 0 1250\n"},
		{"import raw", `{"imported":1}`,
			func(c *Client) error { _, err := c.ImportRaw(ctx, "tx", []byte("a 1 2\n")); return err },
			"POST", "/table/sales.tx/import?database=sales", "a 1 2\n"},
		{"delete", `{"deleted":2}`,
			func(c *Client) error { _, err := c.DeleteRecords(ctx, "tx", []uint64{1, 2}); return err },
			"POST", "/table/sales.tx/delete?database=sales", "1\n2\n"},
		{"records", `{"records":[],"next":null}`,
			func(c *Client) error { _, err := c.Records(ctx, "tx", Limit(10)); return err },
			"GET", "/table/sales.tx/records?database=sales&limit=10", ""},
		{"schema", `{"tables":[]}`,
			func(c *Client) error { _, err := c.Schema(ctx); return err },
			"GET", "/schema?database=sales", ""},
		{"health", `{}`,
			func(c *Client) error { _, err := c.Health(ctx); return err },
			"GET", "/health", ""},
		{"ready", `{"status":"ready"}`,
			func(c *Client) error { _, err := c.Ready(ctx); return err },
			"GET", "/ready", ""},
		{"create table", `{"table":3}`,
			func(c *Client) error {
				_, err := c.CreateTable(ctx, "tx", WithEngine("bitmap+columnar"))
				return err
			},
			"POST", "/table/sales.tx?database=sales&engine=bitmap%2Bcolumnar", ""},
		{"drop table", `{}`,
			func(c *Client) error { return c.DropTable(ctx, "tx") },
			"DELETE", "/table/sales.tx?database=sales", ""},
		{"create field", `{"field":1}`,
			func(c *Client) error {
				_, err := c.CreateField(ctx, "tx", "amount", "signed", WithBitDepth(64))
				return err
			},
			"POST", "/table/sales.tx/field/amount?database=sales&kind=signed&bit_depth=64", ""},
		{"create decimal field", `{"field":1}`,
			func(c *Client) error {
				_, err := c.CreateField(ctx, "tx", "price", "decimal", WithScale(2))
				return err
			},
			"POST", "/table/sales.tx/field/price?database=sales&kind=decimal&scale=2", ""},
		{"drop field", `{}`,
			func(c *Client) error { return c.DropField(ctx, "tx", "amount") },
			"DELETE", "/table/sales.tx/field/amount?database=sales", ""},
		{"create database", `{"created":true}`,
			func(c *Client) error { return c.CreateDatabase(ctx, "sales") },
			"POST", "/database/sales", ""},
		{"drop database", `{}`,
			func(c *Client) error { return c.DropDatabase(ctx, "sales", Cascade()) },
			"DELETE", "/database/sales?cascade=true", ""},
	} {
		t.Run(c.name, func(t *testing.T) {
			body := c.body
			r := &recorder{reply: func(int, call) (*RawResponse, error) {
				return jsonResp(200, body), nil
			}}
			client := stubClient(t, r, WithDatabase("sales"))
			if err := c.run(client); err != nil {
				t.Fatal(err)
			}
			got := r.calls[0]
			if got.Method != c.method || got.Target != c.target {
				t.Errorf("%s %s, want %s %s", got.Method, got.Target, c.method, c.target)
			}
			if got.Body != c.want {
				t.Errorf("body = %q, want %q", got.Body, c.want)
			}
		})
	}
}

func TestAnErrorFromTheServerReachesTheCallerWhole(t *testing.T) {
	r := &recorder{reply: func(int, call) (*RawResponse, error) {
		resp := jsonResp(404, `{"error":"no table named `+"`tx`"+`","code":"unknown_table"}`)
		resp.Header["X-Request-Id"] = []string{"r-1"}
		return resp, nil
	}}
	c := stubClient(t, r)

	_, err := c.Records(context.Background(), "tx")
	if !errors.Is(err, ErrNotFound) {
		t.Fatalf("want ErrNotFound, got %#v", err)
	}
	var se *ServerError
	if !errors.As(err, &se) {
		t.Fatal("want a *ServerError too")
	}
	// no_such_route and unknown_table are both 404; the code is the only thing that tells them
	// apart, so it has to survive.
	if se.Code != "unknown_table" || se.RequestID != "r-1" {
		t.Errorf("server error = %#v", se)
	}
}

func TestIterRecordsWalksEveryPage(t *testing.T) {
	// The server's cursor rule: a full page always reports a cursor, even the last one, so the
	// walk ends on an empty page rather than on a nil Next.
	pages := []string{
		`{"records":[1,2],"next":2}`,
		`{"records":[3,4],"next":4}`,
		`{"records":[],"next":null}`,
	}
	r := &recorder{reply: func(n int, _ call) (*RawResponse, error) {
		return jsonResp(200, pages[n]), nil
	}}
	c := stubClient(t, r)

	var got []uint64
	for id, err := range c.IterRecords(context.Background(), "tx", Limit(2)) {
		if err != nil {
			t.Fatal(err)
		}
		got = append(got, id)
	}
	if !slices.Equal(got, []uint64{1, 2, 3, 4}) {
		t.Errorf("ids = %v", got)
	}
	// The cursor from each page has to be carried into the next request.
	if !strings.Contains(r.calls[1].Target, "after=2") {
		t.Errorf("second page target = %s", r.calls[1].Target)
	}
	if !strings.Contains(r.calls[2].Target, "after=4") {
		t.Errorf("third page target = %s", r.calls[2].Target)
	}
}

func TestIterRecordsStopsWhenTheCallerBreaks(t *testing.T) {
	r := &recorder{reply: func(int, call) (*RawResponse, error) {
		return jsonResp(200, `{"records":[1,2,3],"next":3}`), nil
	}}
	c := stubClient(t, r)

	n := 0
	for range c.IterRecords(context.Background(), "tx") {
		n++
		break
	}
	if n != 1 {
		t.Errorf("yielded %d ids after a break", n)
	}
	if len(r.calls) != 1 {
		t.Errorf("breaking out must not fetch another page; %d requests", len(r.calls))
	}
}

func TestIterRecordsYieldsAnErrorOnceAndStops(t *testing.T) {
	r := &recorder{reply: func(int, call) (*RawResponse, error) {
		return jsonResp(503, `{"error":"not now","code":"not_serving"}`), nil
	}}
	c := stubClient(t, r)

	seen := 0
	var last error
	for id, err := range c.IterRecords(context.Background(), "tx") {
		seen++
		last = err
		if id != 0 {
			t.Errorf("an error must come with a zero id, got %d", id)
		}
	}
	if seen != 1 || last == nil {
		t.Errorf("want exactly one error yield, got %d (%v)", seen, last)
	}
}

func TestImportStreamChunksAtTheCeiling(t *testing.T) {
	r := &recorder{reply: func(int, call) (*RawResponse, error) {
		return jsonResp(200, `{"imported":2}`), nil
	}}
	// "a 1 1\n" is six bytes, so fourteen holds two lines and not three.
	c := stubClient(t, r, WithMaxBytes(14))

	facts := func(yield func(Fact) bool) {
		for i := 1; i <= 5; i++ {
			if !yield(Fact{Field: "a", Record: uint64(i), Value: i}) {
				return
			}
		}
	}

	var checkpoints []int
	total, err := c.ImportStream(context.Background(), "tx", facts,
		func(sent int, _ *WriteResult) error {
			checkpoints = append(checkpoints, sent)
			return nil
		})
	if err != nil {
		t.Fatal(err)
	}
	if len(r.calls) != 3 {
		t.Errorf("five facts at two per request is %d requests, want 3", len(r.calls))
	}
	// The counts are the resume points: /import is idempotent, so a caller who recorded these
	// can replay from the last one.
	if !slices.Equal(checkpoints, []int{2, 4, 5}) {
		t.Errorf("checkpoints = %v, want [2 4 5]", checkpoints)
	}
	if total.Count != 6 { // the stub says 2 each time
		t.Errorf("total = %d", total.Count)
	}
}

func TestImportStreamRefusesAFactThatCanNeverFit(t *testing.T) {
	// Chunking will never help, so looping forever trying is the wrong answer.
	r := &recorder{}
	c := stubClient(t, r, WithMaxBytes(4))

	facts := func(yield func(Fact) bool) {
		yield(Fact{Field: "amount", Record: 1, Value: 1250})
	}
	_, err := c.ImportStream(context.Background(), "tx", facts, nil)
	var tl *TooLargeError
	if !errors.As(err, &tl) {
		t.Fatalf("want a *TooLargeError, got %#v", err)
	}
	if len(r.calls) != 0 {
		t.Error("nothing may reach the wire")
	}
}

func TestABodyOverTheCeilingNeverReachesTheWire(t *testing.T) {
	r := &recorder{}
	c := stubClient(t, r, WithMaxBytes(10))

	_, err := c.SQL(context.Background(), strings.Repeat("x", 100))
	if !errors.Is(err, ErrPayloadTooLarge) {
		t.Fatalf("want ErrPayloadTooLarge, got %#v", err)
	}
	if len(r.calls) != 0 {
		t.Error("the refusal must happen before the socket opens")
	}
}

func TestNewRefusesAnAddressItCannotDial(t *testing.T) {
	if _, err := New("example.com"); err == nil {
		t.Error("a missing port must be refused rather than guessed")
	}
	var ce *ConfigError
	if _, err := New("ftp://x:1"); !errors.As(err, &ce) {
		t.Errorf("want a *ConfigError, got %#v", err)
	}
}

func TestACAFileOnAPlaintextAddressIsAConfigError(t *testing.T) {
	// A caller who passes a CA file thinks they are encrypted. They are not, and saying so is
	// the whole value of the check.
	_, err := New("127.0.0.1:7654", WithCAFile("/nonexistent.pem"))
	var ce *ConfigError
	if !errors.As(err, &ce) {
		t.Fatalf("want a *ConfigError, got %#v", err)
	}
	if !strings.Contains(err.Error(), "not be encrypted") {
		t.Errorf("the message must say why: %v", err)
	}
}

func TestSQLReadsATextAnswerWithoutScanningTheStatement(t *testing.T) {
	r := &recorder{reply: func(int, call) (*RawResponse, error) {
		h := make(map[string][]string)
		h["Content-Type"] = []string{"text/csv; charset=utf-8"}
		return &RawResponse{Status: 200, Header: h, Body: []byte("n\r\n41\r\n")}, nil
	}}
	c := stubClient(t, r)

	// Note the statement carries no FORMAT clause and the answer is still text: the client
	// reads the header, and would be wrong here if it read the statement instead.
	res, err := c.SQL(context.Background(), "SELECT count(*) FROM tx")
	if err != nil {
		t.Fatal(err)
	}
	if !res.IsText() || res.ContentType != "text/csv" {
		t.Fatalf("result = %#v", res)
	}
	if string(res.Text) != "n\r\n41\r\n" {
		t.Errorf("text = %q", res.Text)
	}
}

func TestScalarIntReadsTheNumberADDLRouteAnswersWith(t *testing.T) {
	r := &recorder{reply: func(int, call) (*RawResponse, error) {
		return jsonResp(200, `{"table":18446744073709551615}`), nil
	}}
	c := stubClient(t, r)

	// Out of an int64's range, so it comes back as zero rather than as a wrapped number - and
	// the call still succeeds, because the status already said it did.
	if _, err := c.CreateTable(context.Background(), "tx"); err != nil {
		t.Fatal(err)
	}

	r2 := &recorder{reply: func(int, call) (*RawResponse, error) {
		return jsonResp(200, `{"field":7}`), nil
	}}
	c2 := stubClient(t, r2)
	n, err := c2.CreateField(context.Background(), "tx", "a", "int")
	if err != nil {
		t.Fatal(err)
	}
	if n != 7 {
		t.Errorf("field = %d, want 7", n)
	}
}

func TestFactValueRendersEveryTypeThisRouteReads(t *testing.T) {
	for _, c := range []struct {
		in   any
		want string
	}{
		{"gb", "gb"},
		{true, "true"},
		{int64(-5), "-5"},
		{uint64(math.MaxUint64), "18446744073709551615"},
		{2.75, "2.75"},
		{Decimal("12.50"), "12.50"},
		{Keyed{Key: "gb", At: 1}, "gb@1"},
	} {
		got, err := FactValue(c.in)
		if err != nil {
			t.Fatalf("%#v: %v", c.in, err)
		}
		if got != c.want {
			t.Errorf("FactValue(%#v) = %q, want %q", c.in, got, c.want)
		}
	}
	if _, err := FactValue(struct{}{}); err == nil {
		t.Error("a type with no spelling must be refused")
	}
}

func TestAFactWithABadFieldNameIsRefused(t *testing.T) {
	for _, f := range []Fact{
		{Field: "", Record: 1, Value: 1},
		{Field: "a b", Record: 1, Value: 1},
	} {
		if _, err := RenderFacts([]Fact{f}, DefaultMaxBytes); err == nil {
			t.Errorf("%#v must be refused", f)
		}
	}
}
