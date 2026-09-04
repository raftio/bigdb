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
	"bytes"
	"encoding/json"
	"fmt"
	"strconv"
)

// RawResponse in, typed answer or typed error out. No I/O here, which is what makes the whole
// of it testable against bytes copied out of the server's own tests.

// decodeError turns a non-2xx response into a *ServerError.
//
// The envelope is {"error": <sentence>, "code": <code>} - big_http::json::error. An older
// document elsewhere in this repository calls the sentence "message"; that is wrong, and this
// reads "error" first and falls back to "message" rather than requiring either, because a body
// that is neither still has a status worth reporting.
func decodeError(r *RawResponse) *ServerError {
	var body struct {
		Error   string `json:"error"`
		Message string `json:"message"`
		Code    string `json:"code"`
	}
	_ = json.Unmarshal(r.Body, &body)

	msg := body.Error
	if msg == "" {
		msg = body.Message
	}
	if msg == "" {
		// A 5xx body is redacted, so there may be nothing here at all. Say so plainly rather
		// than reporting an empty sentence.
		msg = "the server gave no detail"
		if len(r.Body) > 0 && len(r.Body) < 200 {
			msg = string(bytes.TrimSpace(r.Body))
		}
	}
	e := &ServerError{
		Status:    r.Status,
		Code:      body.Code,
		Message:   msg,
		RequestID: r.RequestID(),
	}
	if d, ok := r.RetryAfter(); ok {
		e.RetryAfter = d
	}
	return e
}

func unmarshalObject(b []byte) (map[string]json.RawMessage, error) {
	var m map[string]json.RawMessage
	if err := json.Unmarshal(b, &m); err != nil {
		return nil, &ProtocolError{What: "the answer is not a JSON object: " + err.Error()}
	}
	return m, nil
}

func asUint64(raw json.RawMessage, what string) (uint64, error) {
	n, err := strconv.ParseUint(string(bytes.TrimSpace(raw)), 10, 64)
	if err != nil {
		return 0, &ProtocolError{What: fmt.Sprintf("%s is not a u64: %s", what, raw)}
	}
	return n, nil
}

func asInt64(raw json.RawMessage, what string) (int64, error) {
	n, err := strconv.ParseInt(string(bytes.TrimSpace(raw)), 10, 64)
	if err != nil {
		return 0, &ProtocolError{What: fmt.Sprintf("%s is not an i64: %s", what, raw)}
	}
	return n, nil
}

func isNull(raw json.RawMessage) bool {
	return string(bytes.TrimSpace(raw)) == "null"
}

// hasPoint reports whether a JSON number was written as a real rather than as an integer.
//
// big_http::json::real always writes a point ("a column that is sometimes 3 and sometimes 3.5
// is a column a client has to sniff"), so this is a reliable read of which kind of sum came
// back rather than a guess.
func hasPoint(raw json.RawMessage) bool {
	return bytes.ContainsAny(raw, ".eE")
}

// decodeAnswer reads one PQL answer.
//
// The keys are checked in a fixed order because the wire form has no discriminant. A body that
// matches none of them is a *ProtocolError: a shape this build has never heard of should be
// said out loud, not returned as a map wearing a type.
func decodeAnswer(body []byte) (Answer, error) {
	m, err := unmarshalObject(body)
	if err != nil {
		return nil, err
	}
	raw := json.RawMessage(body)

	if v, ok := m["count"]; ok {
		n, err := asUint64(v, "count")
		if err != nil {
			return nil, err
		}
		return &CountAnswer{Count: n, Raw: raw}, nil
	}

	if v, ok := m["sum"]; ok {
		a := &SumAnswer{Raw: raw}
		if hasPoint(v) {
			a.IsReal = true
			if err := json.Unmarshal(v, &a.Real); err != nil {
				return nil, &ProtocolError{What: "sum is not a number: " + string(v)}
			}
			return a, nil
		}
		if a.Value, err = asInt64(v, "sum"); err != nil {
			return nil, err
		}
		return a, nil
	}

	if v, ok := m["value"]; ok {
		a := &ValueAnswer{Raw: raw}
		if isNull(v) {
			return a, nil
		}
		a.Present = true
		if hasPoint(v) {
			a.IsReal = true
			if err := json.Unmarshal(v, &a.Real); err != nil {
				return nil, &ProtocolError{What: "value is not a number: " + string(v)}
			}
			return a, nil
		}
		if a.Value, err = asInt64(v, "value"); err != nil {
			return nil, err
		}
		return a, nil
	}

	if _, ok := m["records"]; ok {
		ids, next, err := decodeRecordList(m)
		if err != nil {
			return nil, err
		}
		return &RecordsAnswer{Records: ids, Next: next, Raw: raw}, nil
	}

	if v, ok := m["tuples"]; ok {
		var items []struct {
			Keys  []json.RawMessage `json:"keys"`
			Value json.RawMessage   `json:"value"`
		}
		if err := json.Unmarshal(v, &items); err != nil {
			return nil, &ProtocolError{What: "tuples is not a list: " + err.Error()}
		}
		out := make([]TupleItem, 0, len(items))
		for _, it := range items {
			keys := make([]GroupKey, 0, len(it.Keys))
			for _, k := range it.Keys {
				gk, err := decodeGroupKey(k)
				if err != nil {
					return nil, err
				}
				keys = append(keys, gk)
			}
			inner, err := decodeAnswer(it.Value)
			if err != nil {
				return nil, err
			}
			out = append(out, TupleItem{Keys: keys, Value: inner})
		}
		return &TuplesAnswer{Tuples: out, Raw: raw}, nil
	}

	if v, ok := m["groups"]; ok {
		var items []json.RawMessage
		if err := json.Unmarshal(v, &items); err != nil {
			return nil, &ProtocolError{What: "groups is not a list: " + err.Error()}
		}
		out := make([]GroupItem, 0, len(items))
		for _, it := range items {
			gk, err := decodeGroupKey(it)
			if err != nil {
				return nil, err
			}
			var wrap struct {
				Value json.RawMessage `json:"value"`
			}
			if err := json.Unmarshal(it, &wrap); err != nil {
				return nil, &ProtocolError{What: "a group is not an object: " + err.Error()}
			}
			inner, err := decodeAnswer(wrap.Value)
			if err != nil {
				return nil, err
			}
			out = append(out, GroupItem{GroupKey: gk, Value: inner})
		}
		return &GroupsAnswer{Groups: out, Raw: raw}, nil
	}

	if v, ok := m["rows"]; ok {
		var items []struct {
			Record json.RawMessage   `json:"record"`
			Values []json.RawMessage `json:"values"`
		}
		if err := json.Unmarshal(v, &items); err != nil {
			return nil, &ProtocolError{What: "rows is not a list: " + err.Error()}
		}
		out := make([]ProjectionRow, 0, len(items))
		for _, it := range items {
			id, err := asUint64(it.Record, "record")
			if err != nil {
				return nil, err
			}
			out = append(out, ProjectionRow{Record: id, Values: it.Values})
		}
		return &RowsAnswer{Rows: out, Raw: raw}, nil
	}

	return nil, &ProtocolError{What: "this answer is a shape this client does not know: " +
		truncate(string(body), 200)}
}

func decodeGroupKey(b []byte) (GroupKey, error) {
	var k struct {
		Key json.RawMessage `json:"key"`
		Row json.RawMessage `json:"row"`
	}
	if err := json.Unmarshal(b, &k); err != nil {
		return GroupKey{}, &ProtocolError{What: "a group key is not an object: " + err.Error()}
	}
	out := GroupKey{}
	if len(k.Key) > 0 && !isNull(k.Key) {
		if err := json.Unmarshal(k.Key, &out.Key); err != nil {
			return GroupKey{}, &ProtocolError{What: "a group key is not a string: " + string(k.Key)}
		}
		out.HasKey = true
	}
	row, err := asInt64(k.Row, "row")
	if err != nil {
		return GroupKey{}, err
	}
	out.Row = row
	return out, nil
}

// decodeRecordList reads the {"records": [...], "next": id|null} pair, which two routes share.
func decodeRecordList(m map[string]json.RawMessage) ([]uint64, *uint64, error) {
	var rawIDs []json.RawMessage
	if err := json.Unmarshal(m["records"], &rawIDs); err != nil {
		return nil, nil, &ProtocolError{What: "records is not a list: " + err.Error()}
	}
	// Decoded one at a time through ParseUint rather than into []uint64 wholesale, so that a
	// value that is not an integer names itself instead of coming back as a zero.
	ids := make([]uint64, 0, len(rawIDs))
	for _, r := range rawIDs {
		id, err := asUint64(r, "a record id")
		if err != nil {
			return nil, nil, err
		}
		ids = append(ids, id)
	}

	var next *uint64
	if v, ok := m["next"]; ok && !isNull(v) {
		n, err := asUint64(v, "next")
		if err != nil {
			return nil, nil, err
		}
		next = &n
	}
	return ids, next, nil
}

// decodeRecordPage reads GET /table/{t}/records.
func decodeRecordPage(body []byte) (*RecordPage, error) {
	m, err := unmarshalObject(body)
	if err != nil {
		return nil, err
	}
	if _, ok := m["records"]; !ok {
		return nil, &ProtocolError{What: "a record page with no records key"}
	}
	ids, next, err := decodeRecordList(m)
	if err != nil {
		return nil, err
	}
	return &RecordPage{Records: ids, Next: next, Raw: body}, nil
}

// decodeSQL reads POST /sql, deciding on the response's Content-Type and nothing else.
func decodeSQL(r *RawResponse) (*SQLResult, error) {
	if ct := r.ContentType(); ct != "application/json" {
		return &SQLResult{Text: r.Body, ContentType: ct}, nil
	}
	var set struct {
		Columns []string            `json:"columns"`
		Rows    [][]json.RawMessage `json:"rows"`
	}
	if err := json.Unmarshal(r.Body, &set); err != nil {
		return nil, &ProtocolError{What: "the result set is not readable: " + err.Error()}
	}
	if set.Columns == nil {
		set.Columns = []string{}
	}
	return &SQLResult{Columns: set.Columns, Rows: set.Rows, Raw: r.Body}, nil
}

// decodeWrite reads the {"<name>": n} or {"<name>": n, "missed": [...]} that an import or a
// delete answers with. See big_http::json::wrote.
func decodeWrite(body []byte, name string) (*WriteResult, error) {
	m, err := unmarshalObject(body)
	if err != nil {
		return nil, err
	}
	v, ok := m[name]
	if !ok {
		return nil, &ProtocolError{What: "a write answer with no " + name + " key"}
	}
	n, err := asUint64(v, name)
	if err != nil {
		return nil, err
	}
	out := &WriteResult{Count: n, Raw: body}
	if raw, ok := m["missed"]; ok {
		if err := json.Unmarshal(raw, &out.Missed); err != nil {
			return nil, &ProtocolError{What: "missed is not a list of strings: " + err.Error()}
		}
	}
	return out, nil
}

// decodeSchema reads GET /schema.
func decodeSchema(body []byte) (*Schema, error) {
	var doc struct {
		Tables []struct {
			Name   string `json:"name"`
			Engine string `json:"engine"`
			Fields []struct {
				Name        string          `json:"name"`
				Kind        string          `json:"kind"`
				BitDepth    uint32          `json:"bit_depth"`
				Scale       *int64          `json:"scale"`
				Granularity []string        `json:"granularity"`
				Raw         json.RawMessage `json:"-"`
			} `json:"fields"`
		} `json:"tables"`
	}
	if err := json.Unmarshal(body, &doc); err != nil {
		return nil, &ProtocolError{What: "the schema is not readable: " + err.Error()}
	}
	out := &Schema{Raw: body}
	for _, t := range doc.Tables {
		ti := TableInfo{Name: t.Name, Engine: t.Engine}
		for _, f := range t.Fields {
			fi := FieldInfo{
				Name:        f.Name,
				Kind:        f.Kind,
				BitDepth:    f.BitDepth,
				Granularity: f.Granularity,
			}
			if f.Scale != nil {
				fi.Scale, fi.HasScale = *f.Scale, true
			}
			ti.Fields = append(ti.Fields, fi)
		}
		out.Tables = append(out.Tables, ti)
	}
	return out, nil
}

// decodeReady reads GET /ready.
func decodeReady(body []byte) (*Ready, error) {
	var doc struct {
		Status  string   `json:"status"`
		Tables  int      `json:"tables"`
		TxnID   uint64   `json:"txn_id"`
		Pages   uint64   `json:"pages"`
		Node    string   `json:"node"`
		Shards  string   `json:"shards"`
		Version string   `json:"version"`
		Wire    int      `json:"wire"`
		Serving *bool    `json:"serving"`
		Term    uint64   `json:"term"`
		Leader  string   `json:"leader"`
		Behind  []string `json:"behind"`
	}
	if err := json.Unmarshal(body, &doc); err != nil {
		return nil, &ProtocolError{What: "the readiness answer is not readable: " + err.Error()}
	}
	r := &Ready{
		Status:  doc.Status,
		Tables:  doc.Tables,
		TxnID:   doc.TxnID,
		Pages:   doc.Pages,
		Node:    doc.Node,
		Shards:  doc.Shards,
		Version: doc.Version,
		Wire:    doc.Wire,
		Term:    doc.Term,
		Leader:  doc.Leader,
		Behind:  doc.Behind,
		Raw:     body,
	}
	if doc.Serving != nil {
		r.Serving, r.HasServing = *doc.Serving, true
	}
	return r, nil
}

func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "..."
}
