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

package sqldriver

import (
	"context"
	"database/sql"

	bigdb "github.com/raftio/bigdb/clients/go"
)

// ExecBatch runs one INSERT template over many rows, merging them into as few statements as the
// ceilings allow.
//
// database/sql has no ExecMany, and a loop over Exec pays for a commit per row: the same four
// million facts took 3.36s as 250 statements and 1.81s as 5 (contrib/big-message/readme.md).
// This is exposed as its own function rather than hidden inside Exec so that a caller who wants
// the merge can see that they asked for it - and so that a caller who did not is never
// surprised by a statement that is not the one they wrote.
//
// A template that is not shaped like `... VALUES (?, ?)` is not merged; the rows are sent one
// statement each, which is always correct. See bigdb.MergeInserts.
//
// The rows are not one atomic write. This server commits per request, so a batch that fails
// part way has applied the statements before the failure. That is the same guarantee everything
// else here gives, said out loud at the one place a caller might expect otherwise.
func ExecBatch(ctx context.Context, db *sql.DB, query string, rows [][]any, opts ...BatchOption) (int, error) {
	cfg := batchConfig{maxBytes: bigdb.DefaultMaxBytes, maxRows: bigdb.DefaultMaxRows}
	for _, o := range opts {
		o(&cfg)
	}

	statements, err := bigdb.MergeInserts(query, rows, cfg.maxBytes, cfg.maxRows)
	if err != nil {
		return 0, err
	}
	for i, s := range statements {
		if _, err := db.ExecContext(ctx, s); err != nil {
			// Which statement, so the caller knows how far the batch got. There is no offset
			// finer than this: the server commits per request.
			return i, err
		}
	}
	return len(statements), nil
}

type batchConfig struct {
	maxBytes int
	maxRows  int
}

// A BatchOption tunes ExecBatch.
type BatchOption func(*batchConfig)

// WithBatchMaxBytes caps how large a merged statement may get. The default leaves the same
// 1 MiB margin under the server's 8 MiB that the rest of this client leaves.
func WithBatchMaxBytes(n int) BatchOption { return func(c *batchConfig) { c.maxBytes = n } }

// WithBatchMaxRows caps how many rows a merged statement carries.
func WithBatchMaxRows(n int) BatchOption { return func(c *batchConfig) { c.maxRows = n } }
