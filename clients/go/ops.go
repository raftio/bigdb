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
	"strconv"
	"strings"
)

// One builder per route. Pure: an op is a method, a target and a body, and building one opens
// no socket. That is what lets a test assert the exact bytes a call would send.

// An op is one request, ready to hand to a Doer.
type op struct {
	Method string
	Target string
	Body   []byte

	// Idempotent says whether sending this a second time is the same as sending it once.
	//
	// Set per route, once, here - not guessed from the method. Reads, /query, /import and
	// /delete are idempotent: a fact is a bit set at an address the caller chose, and deleting
	// a record that is already gone is not an error. /sql is not, because an allocating INSERT
	// writes twice. No DDL is, because in a cluster a schema change that got far enough may
	// already be partially_applied, and resending turns one thing to check into two.
	Idempotent bool

	// Name is what this operation is called in an error message.
	Name string
}

// tooLarge refuses a body before the socket opens.
//
// big_http checks MAX_BODY against Content-Length before reading the body, so an over-large
// request is a clean 413 rather than a half-written one. Checking here as well means the caller
// is told in a sentence that names the size, without a round trip.
func (o *op) check(maxBytes int) error {
	if len(o.Body) > maxBytes {
		return &TooLargeError{Bytes: len(o.Body), Cap: maxBytes}
	}
	return nil
}

// callOptions are the per-call knobs. Empty fields mean "use the client's".
type callOptions struct {
	database string
	after    *uint64
	limit    int
	engine   string
	kind     string
	bitDepth *uint32
	scale    *uint32
	cascade  bool
}

// A CallOpt narrows one call.
type CallOpt func(*callOptions)

// InDatabase overrides the client's database for this call.
func InDatabase(db string) CallOpt { return func(o *callOptions) { o.database = db } }

// After resumes a listing strictly after this record id.
func After(id uint64) CallOpt { return func(o *callOptions) { o.after = &id } }

// Limit caps how many ids come back.
//
// On /query this is only meaningful for an answer that is a list of records; asking for a page
// of a Count is a 422 not_pageable, which is the server declining to pretend a number has pages.
func Limit(n int) CallOpt { return func(o *callOptions) { o.limit = n } }

// WithEngine names the table engine at creation. Passed through as written: an engine name this
// client has never heard of is between the caller and the server.
func WithEngine(engine string) CallOpt { return func(o *callOptions) { o.engine = engine } }

// WithBitDepth sets how many planes a field gets. Absent means the server's default for the
// kind, which is not a constant 32 - a float64 gets 64, because a field whose name carries a
// width should have it.
func WithBitDepth(n uint32) CallOpt { return func(o *callOptions) { o.bitDepth = &n } }

// WithScale sets a decimal field's scale.
func WithScale(n uint32) CallOpt { return func(o *callOptions) { o.scale = &n } }

// Cascade drops a database along with what is in it.
func Cascade() CallOpt { return func(o *callOptions) { o.cascade = true } }

// qualify folds the database into the table name, so every route is given a name that already
// says which database it means.
//
// # Why the client does this rather than leaning on ?database=
//
// The parameter does not mean the same thing on every route. The data routes fold it in
// (big_http::routes::query::scoped), but the DDL routes ignore it and read the path alone - so
// a client that set a database and then called CreateTable would make the table in the default
// database and, moments later, write to a different one. Creating a table in one place and
// importing into another, with a 200 at every step, is the worst shape a bug can have.
//
// A qualified path works on every route and is documented to win over a disagreeing parameter,
// so folding here makes one meaning for "which database" across the whole client. The parameter
// is still sent: the route table documents it, the permission check reads it, and where the two
// agree there is nothing to disagree about.
//
// A name that already carries a dot is left alone - the caller said something more specific
// than the client's default, and the more specific of the two wins here for the same reason it
// wins at the server.
func qualify(table, database string) string {
	if database == "" || strings.Contains(table, ".") {
		return table
	}
	return database + "." + table
}

func apply(opts []CallOpt, database string) callOptions {
	o := callOptions{database: database}
	for _, f := range opts {
		f(&o)
	}
	return o
}

func (o callOptions) page() [][2]string {
	var out [][2]string
	if o.after != nil {
		out = append(out, [2]string{"after", strconv.FormatUint(*o.after, 10)})
	}
	if o.limit > 0 {
		out = append(out, [2]string{"limit", strconv.Itoa(o.limit)})
	}
	return out
}

func (o callOptions) db() [2]string { return [2]string{"database", o.database} }

func opSQL(statement string, o callOptions) op {
	return op{
		Method: "POST",
		Target: "/sql" + queryString(o.db()),
		Body:   []byte(statement),
		// An INSERT that lets the server allocate a record id writes a second row when it runs
		// twice, and this route does not say which statements those are. So: never.
		Idempotent: false,
		Name:       "sql",
	}
}

func opQuery(table, pql string, o callOptions) op {
	return op{
		Method:     "POST",
		Target:     "/table/" + EscapeSegment(qualify(table, o.database)) + "/query" + queryString(append([][2]string{o.db()}, o.page()...)...),
		Body:       []byte(pql),
		Idempotent: true,
		Name:       "query",
	}
}

func opImport(table string, body []byte, o callOptions) op {
	return op{
		Method:     "POST",
		Target:     "/table/" + EscapeSegment(qualify(table, o.database)) + "/import" + queryString(o.db()),
		Body:       body,
		Idempotent: true,
		Name:       "import",
	}
}

func opDelete(table string, body []byte, o callOptions) op {
	return op{
		Method:     "POST",
		Target:     "/table/" + EscapeSegment(qualify(table, o.database)) + "/delete" + queryString(o.db()),
		Body:       body,
		Idempotent: true,
		Name:       "delete",
	}
}

func opRecords(table string, o callOptions) op {
	return op{
		Method:     "GET",
		Target:     "/table/" + EscapeSegment(qualify(table, o.database)) + "/records" + queryString(append([][2]string{o.db()}, o.page()...)...),
		Idempotent: true,
		Name:       "records",
	}
}

func opSchema(o callOptions) op {
	return op{Method: "GET", Target: "/schema" + queryString(o.db()), Idempotent: true, Name: "schema"}
}

func opHealth() op {
	return op{Method: "GET", Target: "/health", Idempotent: true, Name: "health"}
}

func opReady() op {
	return op{Method: "GET", Target: "/ready", Idempotent: true, Name: "ready"}
}

func opCreateTable(table string, o callOptions) op {
	return op{
		Method: "POST",
		Target: "/table/" + EscapeSegment(qualify(table, o.database)) +
			queryString(o.db(), [2]string{"engine", o.engine}),
		Idempotent: false,
		Name:       "create table",
	}
}

func opDropTable(table string, o callOptions) op {
	return op{
		Method:     "DELETE",
		Target:     "/table/" + EscapeSegment(qualify(table, o.database)) + queryString(o.db()),
		Idempotent: false,
		Name:       "drop table",
	}
}

func opCreateField(table, field, kind string, o callOptions) op {
	pairs := [][2]string{o.db(), {"kind", kind}}
	if o.bitDepth != nil {
		pairs = append(pairs, [2]string{"bit_depth", strconv.FormatUint(uint64(*o.bitDepth), 10)})
	}
	if o.scale != nil {
		pairs = append(pairs, [2]string{"scale", strconv.FormatUint(uint64(*o.scale), 10)})
	}
	return op{
		Method: "POST",
		Target: "/table/" + EscapeSegment(qualify(table, o.database)) + "/field/" + EscapeSegment(field) +
			queryString(pairs...),
		Idempotent: false,
		Name:       "create field",
	}
}

func opDropField(table, field string, o callOptions) op {
	return op{
		Method: "DELETE",
		Target: "/table/" + EscapeSegment(qualify(table, o.database)) + "/field/" + EscapeSegment(field) +
			queryString(o.db()),
		Idempotent: false,
		Name:       "drop field",
	}
}

func opCreateDatabase(database string) op {
	return op{
		Method:     "POST",
		Target:     "/database/" + EscapeSegment(database),
		Idempotent: false,
		Name:       "create database",
	}
}

func opDropDatabase(database string, o callOptions) op {
	cascade := ""
	if o.cascade {
		cascade = "true"
	}
	return op{
		Method: "DELETE",
		Target: "/database/" + EscapeSegment(database) +
			queryString([2]string{"cascade", cascade}),
		Idempotent: false,
		Name:       "drop database",
	}
}
