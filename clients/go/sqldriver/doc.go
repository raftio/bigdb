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

// Package sqldriver registers bigdb as a database/sql driver.
//
//	import (
//	    "database/sql"
//	    _ "github.com/raftio/bigdb/clients/go/sqldriver"
//	)
//
//	db, err := sql.Open("bigdb", "bigdb://alice:s3cret@127.0.0.1:7654/sales")
//
// A subpackage rather than the same package, so that a caller who only wants a Client does not
// link database/sql to get one.
//
// # What this driver does not have, and why
//
// No transactions. big_http::routes::query::sql answers one statement and remembers nothing, so
// there is nothing to commit or roll back. Begin returns a Tx whose Commit is a no-op and whose
// Rollback is an error - returning an error from Begin itself would break every library that
// calls it defensively, and a Rollback that silently succeeded would be a lie at the one moment
// a caller is relying on it.
//
// No prepared statements at the server. Prepare exists because database/sql wants it, and it
// carries the statement text; the work happens at Query or Exec time. QueryerContext and
// ExecerContext are implemented so the common path does not make a round trip to pretend
// otherwise.
//
// # Placeholders
//
// ?, and read the note in the parent package's bind.go for why it has to be that and not %s or
// $1: ? is not a token in this dialect's lexer, and % is.
package sqldriver
