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
	"bytes"
	"database/sql/driver"
	"encoding/json"
	"errors"
	"io"
	"strconv"
	"time"

	bigdb "github.com/raftio/bigdb/clients/go"
)

// rows walks a result set.
//
// # Why cells arrive as json.RawMessage and are decoded one at a time
//
// A JSON number decoded into `any` becomes a float64, and this database addresses records with
// u64. Decoding a whole row at once would quietly round every id above 2^53. So each cell is
// converted here, integers through ParseInt/ParseUint, and only a number that is actually
// fractional becomes a float64.
type rows struct {
	columns []string
	data    [][]json.RawMessage
	at      int
}

func newRows(res *bigdb.SQLResult) *rows {
	return &rows{columns: res.Columns, data: res.Rows}
}

func (r *rows) Columns() []string { return r.columns }

func (r *rows) Close() error { return nil }

func (r *rows) Next(dest []driver.Value) error {
	if r.at >= len(r.data) {
		return io.EOF
	}
	row := r.data[r.at]
	r.at++
	if len(row) != len(dest) {
		return errors.New("bigdb: a row has " + strconv.Itoa(len(row)) +
			" cells and the result set declared " + strconv.Itoa(len(dest)) + " columns")
	}
	for i, cell := range row {
		v, err := cellValue(cell)
		if err != nil {
			return err
		}
		dest[i] = v
	}
	return nil
}

// cellValue turns one JSON cell into a driver.Value without going through `any`.
func cellValue(cell json.RawMessage) (driver.Value, error) {
	s := bytes.TrimSpace(cell)
	if len(s) == 0 || string(s) == "null" {
		return nil, nil
	}
	switch s[0] {
	case '"':
		var str string
		if err := json.Unmarshal(s, &str); err != nil {
			return nil, err
		}
		return str, nil
	case 't', 'f':
		return s[0] == 't', nil
	case '[', '{':
		// A list of keys, or a shape this driver has no column type for. Handed over as the
		// bytes it is: a caller can scan it into a []byte or a json.RawMessage and decode it
		// themselves, which beats flattening it into a string that has to be re-parsed.
		return []byte(s), nil
	}

	// A number. Integers keep their width; only a genuinely fractional value becomes a float64.
	// big_http::json::real always writes a point, so this reads which kind it is rather than
	// guessing.
	if !bytes.ContainsAny(s, ".eE") {
		if n, err := strconv.ParseInt(string(s), 10, 64); err == nil {
			return n, nil
		}
		// Above MaxInt64 and still an integer: a u64 record id. database/sql has no uint64
		// column type, so it goes over as its digits rather than as a rounded float.
		if _, err := strconv.ParseUint(string(s), 10, 64); err == nil {
			return string(s), nil
		}
	}
	f, err := strconv.ParseFloat(string(s), 64)
	if err != nil {
		return nil, errors.New("bigdb: a cell is not a value this driver reads: " + string(s))
	}
	return f, nil
}

// CheckNamedValue lets a caller pass the types this dialect has spellings for.
//
// database/sql's default conversion would turn many of them into something lossy before this
// package ever saw them - a uint64 above MaxInt64 is rejected outright, and a Decimal would be
// stringified by the wrong rules. So the values pass through untouched and bigdb.Literal
// decides, which keeps one place that turns a caller's value into statement text.
func (c *conn) CheckNamedValue(nv *driver.NamedValue) error {
	switch nv.Value.(type) {
	case nil, string, bool,
		int, int8, int16, int32, int64,
		uint, uint8, uint16, uint32, uint64,
		float32, float64,
		bigdb.Decimal, bigdb.Keyed, time.Time:
		return nil
	}
	if v, ok := nv.Value.(driver.Valuer); ok {
		out, err := v.Value()
		if err != nil {
			return err
		}
		nv.Value = out
		return nil
	}
	return driver.ErrSkip
}

func (s *stmt) CheckNamedValue(nv *driver.NamedValue) error { return s.conn.CheckNamedValue(nv) }
