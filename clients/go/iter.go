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
	"iter"
)

// IterRecords walks every record id in a table, a page at a time.
//
// Returned as an iter.Seq2 so it reads as an ordinary loop:
//
//	for id, err := range c.IterRecords(ctx, "tx") {
//	    if err != nil { return err }
//	    ...
//	}
//
// A callback would have been the same code with the control flow inverted, and a caller who
// wants to break out of it early would have had to invent a sentinel error to do so.
//
// # Why the walk ends on an empty page
//
// big_http::json::records says it plainly: a listing asks the engine for exactly `limit` ids
// and cannot see whether a further one exists without another read, so a full page always
// reports a cursor - even the last one. Over-reading by one to avoid that would cost a shard
// read on every page to save one request at the end of a scan. So this loop stops when a page
// comes back with no cursor or with no records, and the final request returning nothing is the
// expected shape rather than a wasted call.
//
// An error is yielded once, with a zero id, and the loop then ends. There is no partial
// resumption: the caller has the last id they saw and can pass it to After.
func (c *Client) IterRecords(ctx context.Context, table string, opts ...CallOpt) iter.Seq2[uint64, error] {
	return func(yield func(uint64, error) bool) {
		o := apply(opts, c.cfg.database)
		page := o.limit
		if page <= 0 {
			page = DefaultPage
		}

		after := o.after
		for {
			call := []CallOpt{InDatabase(o.database), Limit(page)}
			if after != nil {
				call = append(call, After(*after))
			}
			p, err := c.Records(ctx, table, call...)
			if err != nil {
				yield(0, err)
				return
			}
			for _, id := range p.Records {
				if !yield(id, nil) {
					return
				}
			}
			if p.Next == nil || len(p.Records) == 0 {
				return
			}
			after = p.Next
		}
	}
}

// ImportStream writes facts in chunks, so a batch larger than one request can be sent as
// several.
//
// # Why the callback exists
//
// /import is idempotent, so a caller who records which chunks landed can resume after a failure
// by replaying from the last one they saw - the same shape `bigctl import --resume` has. onChunk
// is called after each chunk succeeds, with the number of facts sent so far. It is the only
// checkpoint there is: this client has no offset of its own to hand back.
//
// The returned WriteResult sums the chunks. Missed keys are concatenated in the order they were
// reported.
func (c *Client) ImportStream(
	ctx context.Context,
	table string,
	facts iter.Seq[Fact],
	onChunk func(sent int, r *WriteResult) error,
	opts ...CallOpt,
) (*WriteResult, error) {
	o := apply(opts, c.cfg.database)
	total := &WriteResult{}
	sent := 0

	var buf []byte
	var held int

	flush := func() error {
		if held == 0 {
			return nil
		}
		r, err := c.importBody(ctx, table, buf, o)
		if err != nil {
			return err
		}
		total.Count += r.Count
		total.Missed = append(total.Missed, r.Missed...)
		sent += held
		buf, held = buf[:0], 0
		if onChunk != nil {
			return onChunk(sent, r)
		}
		return nil
	}

	var outer error
	for f := range facts {
		line, err := f.line()
		if err != nil {
			return nil, &ValueError{What: "fact " + itoa(sent+held+1) + ": " + unwrapWhat(err)}
		}
		if len(line)+1 > c.cfg.maxBytes {
			// One fact that cannot fit in any request at all. Chunking will never help, so say
			// so here rather than looping forever trying.
			return nil, &TooLargeError{Bytes: len(line) + 1, Cap: c.cfg.maxBytes, Line: sent + held + 1}
		}
		if len(buf)+len(line)+1 > c.cfg.maxBytes {
			if err := flush(); err != nil {
				outer = err
				break
			}
		}
		buf = append(buf, line...)
		buf = append(buf, '\n')
		held++
	}
	if outer != nil {
		return nil, outer
	}
	if err := flush(); err != nil {
		return nil, err
	}
	return total, nil
}

func itoa(n int) string {
	if n == 0 {
		return "0"
	}
	var b [20]byte
	i := len(b)
	for n > 0 {
		i--
		b[i] = byte('0' + n%10)
		n /= 10
	}
	return string(b[i:])
}
