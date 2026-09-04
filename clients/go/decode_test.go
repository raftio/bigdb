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
	"encoding/json"
	"errors"
	"math"
	"net/http"
	"testing"
)

func raw(status int, ct, body string, extra ...[2]string) *RawResponse {
	h := make(http.Header)
	h.Set("Content-Type", ct)
	for _, e := range extra {
		h.Add(e[0], e[1])
	}
	return &RawResponse{Status: status, Header: h, Body: []byte(body)}
}

// A record id larger than a float64 holds exactly. This is the guard against anyone reaching
// for map[string]any again: decoded through `any`, 18446744073709551615 comes back as
// 18446744073709552000 and 9007199254740993 as ...992.
func TestARecordIDLargerThanAFloat64HoldsSurvives(t *testing.T) {
	body := `{"records":[18446744073709551615,9007199254740993],"next":18446744073709551615}`

	p, err := decodeRecordPage([]byte(body))
	if err != nil {
		t.Fatal(err)
	}
	if p.Records[0] != math.MaxUint64 {
		t.Errorf("records[0] = %d, want %d", p.Records[0], uint64(math.MaxUint64))
	}
	if p.Records[1] != 9007199254740993 {
		t.Errorf("records[1] = %d, want 9007199254740993", p.Records[1])
	}
	if p.Next == nil || *p.Next != math.MaxUint64 {
		t.Errorf("next = %v, want %d", p.Next, uint64(math.MaxUint64))
	}

	// And the same through the query route, which is a different decoder over the same keys.
	a, err := decodeAnswer([]byte(body))
	if err != nil {
		t.Fatal(err)
	}
	ra, isRecords := a.(*RecordsAnswer)
	if !isRecords {
		t.Fatalf("want a *RecordsAnswer, got %T", a)
	}
	if ra.Records[0] != math.MaxUint64 {
		t.Errorf("records[0] = %d, want %d", ra.Records[0], uint64(math.MaxUint64))
	}
}

func TestDecodeEveryAnswerShape(t *testing.T) {
	for _, c := range []struct {
		name string
		body string
		want func(*testing.T, Answer)
	}{
		{"count", `{"count":41}`, func(t *testing.T, a Answer) {
			if v := a.(*CountAnswer); v.Count != 41 {
				t.Errorf("count = %d", v.Count)
			}
		}},
		{"sum", `{"sum":-9223372036854775808}`, func(t *testing.T, a Answer) {
			v := a.(*SumAnswer)
			if v.IsReal || v.Value != math.MinInt64 {
				t.Errorf("sum = %#v", v)
			}
		}},
		{"real sum", `{"sum":3.5}`, func(t *testing.T, a Answer) {
			v := a.(*SumAnswer)
			if !v.IsReal || v.Real != 3.5 {
				t.Errorf("sum = %#v", v)
			}
		}},
		{"value", `{"value":7}`, func(t *testing.T, a Answer) {
			v := a.(*ValueAnswer)
			if !v.Present || v.Value != 7 {
				t.Errorf("value = %#v", v)
			}
		}},
		// Absent is null, not zero: nothing matched is a different answer from a total of
		// nothing, and the client must be able to tell them apart.
		{"absent value", `{"value":null}`, func(t *testing.T, a Answer) {
			if v := a.(*ValueAnswer); v.Present {
				t.Errorf("null must not read as present: %#v", v)
			}
		}},
		{"records", `{"records":[1,2,3],"next":3}`, func(t *testing.T, a Answer) {
			v := a.(*RecordsAnswer)
			if len(v.Records) != 3 || v.Next == nil || *v.Next != 3 {
				t.Errorf("records = %#v", v)
			}
		}},
		{"last page", `{"records":[1],"next":null}`, func(t *testing.T, a Answer) {
			if v := a.(*RecordsAnswer); v.Next != nil {
				t.Errorf("a null cursor must be nil, got %v", *v.Next)
			}
		}},
		{"tuples", `{"tuples":[{"keys":[{"key":"gb","row":3},{"key":null,"row":9}],"value":{"count":2}}]}`,
			func(t *testing.T, a Answer) {
				v := a.(*TuplesAnswer)
				if len(v.Tuples) != 1 || len(v.Tuples[0].Keys) != 2 {
					t.Fatalf("tuples = %#v", v)
				}
				if k := v.Tuples[0].Keys[0]; !k.HasKey || k.Key != "gb" || k.Row != 3 {
					t.Errorf("keys[0] = %#v", k)
				}
				if k := v.Tuples[0].Keys[1]; k.HasKey {
					t.Errorf("a null key must not read as present: %#v", k)
				}
				if c := v.Tuples[0].Value.(*CountAnswer); c.Count != 2 {
					t.Errorf("inner value = %#v", c)
				}
			}},
		{"groups", `{"groups":[{"key":"2026-09-04","row":20335,"value":{"sum":12}}]}`,
			func(t *testing.T, a Answer) {
				v := a.(*GroupsAnswer)
				if len(v.Groups) != 1 || v.Groups[0].Key != "2026-09-04" || v.Groups[0].Row != 20335 {
					t.Fatalf("groups = %#v", v)
				}
				if s := v.Groups[0].Value.(*SumAnswer); s.Value != 12 {
					t.Errorf("inner value = %#v", s)
				}
			}},
		{"rows", `{"rows":[{"record":18446744073709551615,"values":[null,3,"a",["x","y"]]}]}`,
			func(t *testing.T, a Answer) {
				v := a.(*RowsAnswer)
				if len(v.Rows) != 1 || v.Rows[0].Record != math.MaxUint64 {
					t.Fatalf("rows = %#v", v)
				}
				if len(v.Rows[0].Values) != 4 {
					t.Errorf("values = %#v", v.Rows[0].Values)
				}
			}},
	} {
		t.Run(c.name, func(t *testing.T) {
			a, err := decodeAnswer([]byte(c.body))
			if err != nil {
				t.Fatal(err)
			}
			c.want(t, a)
		})
	}
}

func TestAnUnknownShapeIsSaidOutLoud(t *testing.T) {
	// A shape this build has never heard of must be reported, not returned as something
	// pretending to be typed.
	_, err := decodeAnswer([]byte(`{"histogram":[1,2,3]}`))
	var pe *ProtocolError
	if !errors.As(err, &pe) {
		t.Fatalf("want a *ProtocolError, got %#v", err)
	}
}

func TestDecodeErrorReadsTheEnvelopeTheServerWrites(t *testing.T) {
	// {"error": ..., "code": ...} - big_http::json::error. Not "message".
	r := raw(404, "application/json", `{"error":"no table named `+"`tx`"+`","code":"unknown_table"}`,
		[2]string{"X-Request-Id", "abc123"})
	e := decodeError(r)

	if e.Code != "unknown_table" || e.Status != 404 {
		t.Errorf("decoded = %#v", e)
	}
	if e.RequestID != "abc123" {
		t.Errorf("request id = %q", e.RequestID)
	}
	if !errors.Is(e, ErrNotFound) {
		t.Error("a 404 must match ErrNotFound")
	}
	// The request id comes first, because on a 5xx it is the only handle there is.
	if got := e.Error(); got[:12] != "bigdb: [abc1" {
		t.Errorf("Error() = %q, want the request id first", got)
	}
}

func TestTheCodeTableOnlyOverridesWhatTheStatusCannotSay(t *testing.T) {
	// partially_applied is a 500 that must not be treated like any other 500: some of the write
	// landed, so the repair path is different.
	partial := &ServerError{Status: 500, Code: "partially_applied"}
	if !errors.Is(partial, ErrPartiallyApplied) {
		t.Error("partially_applied needs its own sentinel")
	}
	if errors.Is(partial, ErrServerFault) {
		t.Error("and it must not also read as a plain server fault")
	}

	// A code this build has never heard of falls back to the status rather than to nothing.
	unheard := &ServerError{Status: 409, Code: "some_future_code"}
	if !errors.Is(unheard, ErrConflict) {
		t.Error("an unknown code must fall back to its status")
	}
}

func TestRetryabilityMatchesTheServersTable(t *testing.T) {
	for _, c := range []struct {
		status int
		code   string
		want   bool
	}{
		{503, "owner_unreachable", true},
		{503, "not_serving", true},
		{503, "stale_route", true},
		{503, "range_moving", true},
		{503, "schema_leader_unreachable", true},
		{503, "server_busy", true},
		{503, "busy_authenticating", true},
		// The safety wire: a 503 carrying a code this build never heard of is still a node
		// saying "not me, not now".
		{503, "some_future_code", true},
		// A query that has already run out of time will run out of time again.
		{504, "query_timeout", false},
		// Some of it landed. Sending it again turns one thing to check into two.
		{500, "partially_applied", false},
		{400, "bad_parameter", false},
		{404, "unknown_table", false},
		{409, "refused", false},
		{413, "request_too_large", false},
		{422, "unknown_field", false},
	} {
		got := (&ServerError{Status: c.status, Code: c.code}).Retryable()
		if got != c.want {
			t.Errorf("%d %s: retryable = %v, want %v", c.status, c.code, got, c.want)
		}
	}
}

func TestDecodeSQLFollowsContentTypeAndNeverTheStatement(t *testing.T) {
	j, err := decodeSQL(raw(200, "application/json", `{"columns":["n"],"rows":[[41]]}`))
	if err != nil {
		t.Fatal(err)
	}
	if j.IsText() || len(j.Columns) != 1 || j.Columns[0] != "n" {
		t.Fatalf("json result = %#v", j)
	}
	if string(j.Rows[0][0]) != "41" {
		t.Errorf("cell = %s", j.Rows[0][0])
	}

	// A FORMAT clause changes the body and the Content-Type together. The client reads the
	// header; it does not scan the statement.
	c, err := decodeSQL(raw(200, "text/csv; charset=utf-8", "n\r\n41\r\n"))
	if err != nil {
		t.Fatal(err)
	}
	if !c.IsText() || c.ContentType != "text/csv" {
		t.Fatalf("text result = %#v", c)
	}
}

func TestDecodeWriteReadsBothShapes(t *testing.T) {
	w, err := decodeWrite([]byte(`{"imported":3}`), "imported")
	if err != nil || w.Count != 3 || len(w.Missed) != 0 {
		t.Fatalf("plain = %#v, %v", w, err)
	}
	w, err = decodeWrite([]byte(`{"imported":3,"missed":["gb","fr"]}`), "imported")
	if err != nil || w.Count != 3 || len(w.Missed) != 2 {
		t.Fatalf("with missed = %#v, %v", w, err)
	}
	if _, err := decodeWrite([]byte(`{"deleted":1}`), "imported"); err == nil {
		t.Error("a body with the wrong key must be reported")
	}
}

func TestDecodeSchemaKeepsTheServersSpelling(t *testing.T) {
	// The eleven kinds as /schema renders them: format!("{:?}").to_lowercase() over FieldKind.
	// This test is the fence against a stale vocabulary creeping in from elsewhere.
	body := `{"tables":[{"name":"tx","engine":"bitmap+columnar","fields":[
		{"name":"a","kind":"set","bit_depth":32},
		{"name":"b","kind":"mutex","bit_depth":32},
		{"name":"c","kind":"bool","bit_depth":32},
		{"name":"d","kind":"int","bit_depth":32},
		{"name":"e","kind":"decimal","bit_depth":32,"scale":2},
		{"name":"f","kind":"timequantum","bit_depth":32,"granularity":["Y","M","D"]},
		{"name":"g","kind":"signedint","bit_depth":32},
		{"name":"h","kind":"float32","bit_depth":32},
		{"name":"i","kind":"float64","bit_depth":64},
		{"name":"j","kind":"date","bit_depth":32},
		{"name":"k","kind":"datetime","bit_depth":64}]}]}`

	s, err := decodeSchema([]byte(body))
	if err != nil {
		t.Fatal(err)
	}
	tbl := s.Table("tx")
	if tbl == nil {
		t.Fatal("no table")
	}
	want := []string{"set", "mutex", "bool", "int", "decimal", "timequantum",
		"signedint", "float32", "float64", "date", "datetime"}
	if len(tbl.Fields) != len(want) {
		t.Fatalf("%d fields, want %d", len(tbl.Fields), len(want))
	}
	for i, k := range want {
		if got := tbl.Fields[i].Kind; got != k {
			t.Errorf("field %d kind = %q, want %q (kept verbatim, never translated)", i, got, k)
		}
	}
	// Scale only for a decimal - a decimal without its scale is an integer wearing a
	// different name.
	if f := tbl.Field("e"); !f.HasScale || f.Scale != 2 {
		t.Errorf("decimal scale = %#v", f)
	}
	if f := tbl.Field("d"); f.HasScale {
		t.Error("an int must carry no scale")
	}
	if f := tbl.Field("i"); f.BitDepth != 64 {
		t.Errorf("float64 bit_depth = %d, want 64", f.BitDepth)
	}
}

func TestDecodeReady(t *testing.T) {
	r, err := decodeReady([]byte(
		`{"status":"ready","tables":2,"txn_id":9,"pages":100,"node":"n1","shards":"0-3",` +
			`"version":"0.1.0","wire":1}`))
	if err != nil {
		t.Fatal(err)
	}
	if r.Status != "ready" || r.Version != "0.1.0" || r.Wire != 1 {
		t.Fatalf("ready = %#v", r)
	}
	// serving is absent on a single node, and absent is not false.
	if r.HasServing {
		t.Error("serving must be absent here")
	}

	r, err = decodeReady([]byte(
		`{"status":"ready","tables":0,"txn_id":0,"pages":0,"node":"n1","shards":"0",` +
			`"version":"0.1.0","wire":1,"serving":false,"term":4,"leader":null,"behind":["n2"]}`))
	if err != nil {
		t.Fatal(err)
	}
	if !r.HasServing || r.Serving {
		t.Errorf("serving = %#v", r)
	}
	if r.Term != 4 || len(r.Behind) != 1 {
		t.Errorf("agreement = %#v", r)
	}
}

func TestRawIsCarriedSoAFutureFieldIsStillReachable(t *testing.T) {
	body := `{"count":1,"something_new":{"a":18446744073709551615}}`
	a, err := decodeAnswer([]byte(body))
	if err != nil {
		t.Fatal(err)
	}
	var probe struct {
		New struct {
			A json.Number `json:"a"`
		} `json:"something_new"`
	}
	if err := json.Unmarshal(a.(*CountAnswer).Raw, &probe); err != nil {
		t.Fatal(err)
	}
	// json.RawMessage rather than map[string]any, so the caller decodes it losslessly into
	// whatever type it actually is.
	if probe.New.A.String() != "18446744073709551615" {
		t.Errorf("a = %s, want it undamaged", probe.New.A)
	}
}
