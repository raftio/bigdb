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

import "encoding/json"

// Every shape the server answers with, typed.
//
// # Why nothing here is map[string]any
//
// encoding/json decodes a number into any as a float64, and record ids are u64. Run it and see:
//
//	in : {"records":[18446744073709551615],"next":9007199254740993}
//	out: {"next":9007199254740992,"records":[18446744073709552000]}
//
// The largest record id comes back wrong by 1615 and the cursor by one. So every field that can
// hold a record id, a count or a sum is declared with the width it has, and the escape hatch
// for a field this build has not heard of is Raw - a json.RawMessage, which is lossless,
// allocates nothing, and lets a caller decode it into a type of their own.

// Answer is what POST /table/{t}/query returns. It is one of the concrete types in this file.
//
// The wire form is a union with no discriminant - seven shapes told apart only by which key is
// present (big_http::json::value_paged) - so this is a sealed interface and decoding walks the
// keys in a fixed order. A shape this build has never heard of comes back as a *ProtocolError
// rather than as something pretending to be typed.
type Answer interface{ isAnswer() }

// CountAnswer is {"count": n}.
type CountAnswer struct {
	Count uint64
	Raw   json.RawMessage
}

// SumAnswer is {"sum": n}.
//
// Signed, unsigned and real sums all arrive under the same key, because JSON numbers are signed
// and a client reading {"sum":...} should not have to know how the field was declared. Value
// holds an integer sum; Real holds it when the field is a float, and IsReal says which.
type SumAnswer struct {
	Value  int64
	Real   float64
	IsReal bool
	Raw    json.RawMessage
}

// ValueAnswer is {"value": n} or {"value": null}.
//
// Null is "nothing matched", which is a different answer from a total of nothing - so the
// absence is a bool rather than a zero.
type ValueAnswer struct {
	Value   int64
	Real    float64
	IsReal  bool
	Present bool
	Raw     json.RawMessage
}

// RecordsAnswer is {"records": [...], "next": id|null}.
type RecordsAnswer struct {
	Records []uint64
	// Next is the id to send back as `after`, or nil when this page is the last.
	Next *uint64
	Raw  json.RawMessage
}

// TupleItem is one pair grouping: the keys that name it and the value at it.
type TupleItem struct {
	Keys  []GroupKey
	Value Answer
}

// TuplesAnswer is {"tuples": [...]}.
type TuplesAnswer struct {
	Tuples []TupleItem
	Raw    json.RawMessage
}

// GroupKey names one bucket. Key is the string a keyed group carries beside its row id, or the
// date a calendar bucket stands for; Row is the number it is addressed by.
//
// A bucket names itself: a row id is meaningless without the dictionary that issued it, so the
// server hands back both. See big_http::json::named_at.
type GroupKey struct {
	Key    string
	HasKey bool
	Row    int64
}

// GroupItem is one group and its value.
type GroupItem struct {
	GroupKey
	Value Answer
}

// GroupsAnswer is {"groups": [...]}.
type GroupsAnswer struct {
	Groups []GroupItem
	Raw    json.RawMessage
}

// ProjectionRow is one record and the cells projected from it.
type ProjectionRow struct {
	Record uint64
	Values []json.RawMessage
}

// RowsAnswer is {"rows": [...]} - a projection asked for in the query language.
//
// The cells stay as RawMessage: a projection cell is one of five things (absent, an integer, a
// real, a string, a list of strings) and which one is the field's business, not this decoder's.
type RowsAnswer struct {
	Rows []ProjectionRow
	Raw  json.RawMessage
}

func (*CountAnswer) isAnswer()   {}
func (*SumAnswer) isAnswer()     {}
func (*ValueAnswer) isAnswer()   {}
func (*RecordsAnswer) isAnswer() {}
func (*TuplesAnswer) isAnswer()  {}
func (*GroupsAnswer) isAnswer()  {}
func (*RowsAnswer) isAnswer()    {}

// SQLResult is what POST /sql returns.
//
// # Two shapes, and the client never guesses which
//
// A statement's FORMAT clause changes both the body and the Content-Type
// (big_sql::shape). So Columns and Rows are filled for application/json, and Text and
// ContentType for everything else. The decision is made from the response's Content-Type and
// never by scanning the statement - a client that parsed SQL to find a FORMAT clause would be a
// second parser to keep in step with big_sql.
type SQLResult struct {
	Columns []string
	Rows    [][]json.RawMessage

	// Text is the body when the format is not JSON, with ContentType saying which it is.
	Text        []byte
	ContentType string

	Raw json.RawMessage
}

// IsText reports whether this answer came back in a format other than JSON.
func (r *SQLResult) IsText() bool { return r.Columns == nil && r.Text != nil }

// WriteResult is what an import or a delete returns.
//
// Missed carries the keys the server could not place. It is absent from the body when empty,
// which is why it is a slice and not a count: the names are the useful part.
type WriteResult struct {
	Count  uint64
	Missed []string
	Raw    json.RawMessage
}

// RecordPage is one page of GET /table/{t}/records.
//
// Next is the cursor. Note the server's own caveat (big_http::json::records): a full page
// always reports a cursor, even when it happens to be the last one, because the listing asked
// for exactly `limit` ids and cannot see whether a further one exists without another read.
// A scan therefore ends with one empty page, not with a nil Next on the last full one.
type RecordPage struct {
	Records []uint64
	Next    *uint64
	Raw     json.RawMessage
}

// FieldInfo is one field in the schema.
//
// Kind is the server's spelling, kept as it arrived and never translated. Be aware that the
// read and write vocabularies differ: /schema renders "signedint" but the create-field route's
// parse_kind takes "signed", and posting ?kind=signedint is a 400. This client passes Kind
// through in both directions rather than pretending there is one vocabulary.
//
// Scale is present only for a decimal - a decimal without its scale is an integer wearing a
// different name - and Granularity only for a time quantum.
type FieldInfo struct {
	Name        string
	Kind        string
	BitDepth    uint32
	Scale       int64
	HasScale    bool
	Granularity []string
	Raw         json.RawMessage
}

// TableInfo is one table in the schema.
type TableInfo struct {
	Name   string
	Engine string
	Fields []FieldInfo
	Raw    json.RawMessage
}

// Field finds a field by name, or nil.
func (t *TableInfo) Field(name string) *FieldInfo {
	for i := range t.Fields {
		if t.Fields[i].Name == name {
			return &t.Fields[i]
		}
	}
	return nil
}

// Schema is the answer to GET /schema.
type Schema struct {
	Tables []TableInfo
	Raw    json.RawMessage
}

// Table finds a table by name, or nil.
//
// # Why this is not a trustworthy lookup across databases
//
// big_db::TableInfo carries the database a table belongs to, but big_http::json::schema does
// not render it. So two tables with the same name in two databases arrive here identical, and
// this returns whichever came first. A caller who needs to tell them apart has to ask the
// server with ?database= set rather than sort it out on this side.
func (s *Schema) Table(name string) *TableInfo {
	for i := range s.Tables {
		if s.Tables[i].Name == name {
			return &s.Tables[i]
		}
	}
	return nil
}

// Ready is the answer to GET /ready.
//
// Serving is the part a probe can act on: a node that has lost touch with the agreement refuses
// requests for its range, and a balancer that keeps sending them is sending them somewhere that
// will answer 503. It is still ready - the engine is fine and the node is one promotion away -
// so it is a field rather than a failure, and it is absent on a single node.
//
// Version and Wire are two different questions during a rolling upgrade: Version is which build
// this is, Wire is what it speaks to other nodes. Two nodes with different Version and the same
// Wire run side by side; two with different Wire do not.
type Ready struct {
	Status  string
	Tables  int
	TxnID   uint64
	Pages   uint64
	Node    string
	Shards  string
	Version string
	Wire    int

	Serving    bool
	HasServing bool
	Term       uint64
	Leader     string
	Behind     []string

	Raw json.RawMessage
}
