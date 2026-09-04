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

// Package bigdb is a client for bigdb, written against its HTTP surface and nothing else.
//
// # Scope
//
// The data plane and schema: /sql, /table/{t}/query, /table/{t}/import, /table/{t}/delete,
// /table/{t}/records, /schema, /health, /ready, and the DDL routes. Not /metrics, /verify,
// /repair, /admin/*, /cluster/* or /internal/* - those are an operator's surface and a client
// that spoke them would be a second bigctl.
//
// # Zero dependencies, and why that is a promise rather than a boast
//
// This module requires nothing outside the standard library, has no go.sum, and CI fails if
// either changes. A database client is the piece of a program most likely to be linked into
// something that already has opinions about its dependency tree, and every module it drags in
// is one that has to be audited by whoever ships it.
//
// # At-least-once, said plainly
//
// A write whose outcome is not known is never retried automatically (see TransportError).
// Duplicates can happen and they happen silently; the client only ever repeats a request it can
// prove never arrived. There is no offset to resume from. A caller who needs exactly-once picks
// its own record ids and posts facts to /table/{t}/import, where writing the same bit twice is
// the same as writing it once.
//
// # Databases
//
// WithDatabase means one thing on every call, and this client works for that rather than
// relying on the server to. ?database= scopes the data routes but is ignored by the DDL routes,
// which read the path alone - so a client that only sent the parameter would create a table in
// the default database and then write to another one, with a 200 at every step. Instead the
// database is folded into the table name, which works on every route and is documented to win
// over a disagreeing parameter. A name the caller already qualified wins over the client's
// default, for the same reason.
//
// # Concurrency
//
// A Client holds one connection and serialises calls on it. It is safe to use from several
// goroutines, but it will not make them faster: for real parallelism open several Clients, or
// go through database/sql, which pools them. See the sqldriver subpackage.
package bigdb
