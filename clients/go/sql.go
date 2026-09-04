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
	"fmt"
	"math"
	"strconv"
	"strings"
	"time"
)

// Values and names, written as the statement text the server reads back.
//
// # The one place a caller's bytes become SQL
//
// Every string that reaches a statement passes through QuoteText or QuoteIdent, and nothing
// else in this package writes a quote. That is deliberate and it is the whole of the injection
// argument: there is one file to read, it has no I/O, and its tests run before anything in this
// package opens a socket.
//
// This is a port of contrib/big-message/src/sql.rs, function for function and test for test.
//
// # Why identifiers are always quoted
//
// big_sql's bare_ident takes a Tok::Word or a Tok::Quoted and treats them alike, and the lexer
// builds a Quoted from "..." with "" meaning one " - the same doubling rule '...' has. So
// quoting always is legal everywhere a name may appear, and it removes a class of mistake
// rather than managing it: a column called `values` or `select` is a Tok::Word that the parser
// would read as the keyword it spells. Quoting is not a fallback for awkward names here; it is
// the only path.
//
// # Why numbers are checked against the lexer's own grammar
//
// big_sql::lex::number reads [-]digits[.digits] and nothing else - there is no exponent form -
// and it builds units / 10^scale with units a u64 and scale a u8. So a great many float64
// values have no spelling in this dialect. Checking here means the caller is told which value
// was wrong; leaving it to the server means one value refuses a batch of eight thousand and the
// sentence names none of them.

// RecordColumn is the column that names a record id, which this package refuses to write.
//
// Matched the way the parser matches it - case-insensitively, see big_sql::parse::insert - so
// that _RECORD_ID cannot slip past a check written against the lower-case spelling and turn an
// allocating statement into one that names its own addresses.
const RecordColumn = "_record_id"

// maxScale is the largest scale a literal can carry, because the lexer keeps it in a u8.
const maxScale = 255

// CheckColumn reports whether this package will write a column of this name.
//
// Two refusals, and both are about what the caller meant rather than about what would parse.
// An empty name parses perfectly well as "" and names nothing; _record_id parses perfectly well
// and would quietly turn off the allocation this client relies on.
func CheckColumn(name string) error {
	if name == "" {
		return &ValueError{What: "a column with no name"}
	}
	if strings.EqualFold(name, RecordColumn) {
		return &ValueError{What: fmt.Sprintf(
			"%s is the record id, which this client does not write: leave the column out and "+
				"the server allocates one", name)}
	}
	return nil
}

// QuoteIdent writes one name, double-quoted, with " doubled to mean itself.
func QuoteIdent(name string) (string, error) {
	if name == "" {
		return "", &ValueError{What: "a name with no characters in it"}
	}
	var b strings.Builder
	b.Grow(len(name) + 2)
	b.WriteByte('"')
	for i := 0; i < len(name); i++ {
		if name[i] == '"' {
			b.WriteByte('"')
		}
		b.WriteByte(name[i])
	}
	b.WriteByte('"')
	return b.String(), nil
}

// QuoteTable writes a table, which may be spelled database.table.
//
// Split on the first ".", matching big_db::TableRef::parse, so a qualified name reaches the
// server qualified. The cost is that a table whose own name contains a dot cannot be addressed
// - the same cost every other client in this repository pays, for the same reason.
func QuoteTable(name string) (string, error) {
	database, table, ok := strings.Cut(name, ".")
	if !ok {
		return QuoteIdent(name)
	}
	d, err := QuoteIdent(database)
	if err != nil {
		return "", err
	}
	t, err := QuoteIdent(table)
	if err != nil {
		return "", err
	}
	return d + "." + t, nil
}

// QuoteText writes one string literal, single-quoted, with ' doubled to mean itself.
//
// This is big_sql::lex::string's rule read backwards, and it is total: there is no character a
// caller can send that ends the literal early, because the only character that could is the one
// being doubled.
func QuoteText(s string) string {
	var b strings.Builder
	b.Grow(len(s) + 2)
	b.WriteByte('\'')
	for i := 0; i < len(s); i++ {
		if s[i] == '\'' {
			b.WriteByte('\'')
		}
		b.WriteByte(s[i])
	}
	b.WriteByte('\'')
	return b.String()
}

// Literal writes one value as the literal the server will read.
//
// The accepted types are the ones this dialect has a spelling for. Everything else is refused
// here rather than at the server, so the sentence names the value.
func Literal(value any) (string, error) {
	switch v := value.(type) {
	case nil:
		// The dialect has no NULL literal at all - big_plan::ast::Literal has no null variant -
		// so there is nothing honest to write. Rendering the four letters would produce a
		// syntax error at the server with a sentence about parsing rather than about the value.
		return "", &ValueError{What: "this dialect has no NULL literal; leave the column out"}
	case string:
		return QuoteText(v), nil
	case bool:
		// TRUE and FALSE, which big_sql::parse reads with eat_word and therefore reads in any
		// case. Upper because that is how the rest of the dialect is written.
		if v {
			return "TRUE", nil
		}
		return "FALSE", nil
	case int:
		return strconv.FormatInt(int64(v), 10), nil
	case int8:
		return strconv.FormatInt(int64(v), 10), nil
	case int16:
		return strconv.FormatInt(int64(v), 10), nil
	case int32:
		return strconv.FormatInt(int64(v), 10), nil
	case int64:
		return strconv.FormatInt(v, 10), nil
	case uint:
		return strconv.FormatUint(uint64(v), 10), nil
	case uint8:
		return strconv.FormatUint(uint64(v), 10), nil
	case uint16:
		return strconv.FormatUint(uint64(v), 10), nil
	case uint32:
		return strconv.FormatUint(uint64(v), 10), nil
	case uint64:
		return strconv.FormatUint(v, 10), nil
	case float32:
		return formatFloat(float64(v), 32)
	case float64:
		return formatFloat(v, 64)
	case Decimal:
		// An exact decimal, checked and passed through as written. Not routed via float64,
		// which is the point of the type: a DECIMAL field stores units and a scale, and a
		// round trip through a binary float is where the last digit goes missing.
		return Number(string(v))
	case Keyed:
		return v.literal(), nil
	case time.Time:
		// Rendered as text, which is what a keyed or string field reads. A date or datetime
		// field takes the same spelling through big_embed::fact::from_text.
		return QuoteText(v.Format(time.RFC3339)), nil
	case []byte:
		// Refused rather than treated as text. A blob has no spelling in this dialect, and
		// []byte in Go is far too easy to reach for where a string was meant - so the refusal
		// is louder than a silent reinterpretation would be.
		return "", &ValueError{What: "this dialect has no spelling for a byte string; " +
			"convert it to text if that is what it is"}
	}
	return "", &ValueError{What: fmt.Sprintf("%T has no spelling in this dialect", value)}
}

// A Decimal is an exact decimal, written the way the caller wrote it.
//
// A string rather than a number because that is the only representation that survives: the
// server stores units and a scale, and "12.50" and "12.5" are different scales of the same
// value. Passing through a float64 would decide which one for the caller, wrongly.
type Decimal string

// A Keyed value is a key and the second it belongs to, which is what a time-quantum field takes.
type Keyed struct {
	Key string
	At  int64
}

func (k Keyed) literal() string {
	// Built as one string rather than pushed in three pieces, because the quoting rule has to
	// apply to the whole of it: a key containing a ' must still be doubled, and a key containing
	// an @ is still safe because the server splits on the last one.
	return QuoteText(k.Key + "@" + strconv.FormatInt(k.At, 10))
}

// Number checks text against the grammar the lexer has, and passes it through unchanged.
func Number(s string) (string, error) {
	if err := readable(s); err != nil {
		return "", err
	}
	return s, nil
}

// formatFloat writes a float in the one notation this dialect has.
//
// # Where Go is easier than Rust, and what that removes
//
// Rust's {:?} produces exponent form for magnitudes an ordinary program still reaches (1e-7,
// 1e18), so push_float in sql.rs searches scale 0..=255 for a fixed spelling that round-trips.
// strconv.FormatFloat with 'f' and precision -1 never emits an exponent: 1e-7 comes out as
// "0.0000001" and 1e18 as "1000000000000000000". So the search has nothing to search for and
// is not ported.
//
// 1e300 comes out as 301 digits and is then refused by readable, because units is a u64 - which
// is the same refusal Rust reaches by a longer road, and for the same reason. Writing a
// different number than the caller sent is worse than saying no.
func formatFloat(v float64, bits int) (string, error) {
	if math.IsNaN(v) || math.IsInf(v, 0) {
		return "", &ValueError{What: fmt.Sprintf(
			"%v cannot be written as a number: this dialect has no spelling for it",
			strconv.FormatFloat(v, 'g', -1, bits))}
	}
	s := strconv.FormatFloat(v, 'f', -1, bits)
	if err := readable(s); err != nil {
		return "", err
	}
	return s, nil
}

// readable reports whether the server's lexer would read this text as one number.
//
// big_sql::lex::number: an optional -, at least one digit, then optionally a . and at least one
// more digit. The digits either side of the point are concatenated into units, which is a u64 -
// and an i64 when the sign is there - and the digits after the point are counted into scale,
// which is a u8.
func readable(s string) error {
	refuse := func(why string) error {
		return &ValueError{What: fmt.Sprintf("%s is not a number this dialect reads: %s", s, why)}
	}

	digits := s
	negative := false
	if rest, ok := strings.CutPrefix(s, "-"); ok {
		negative, digits = true, rest
	}

	whole, frac, hasPoint := strings.Cut(digits, ".")

	if whole == "" || !allDigits(whole) {
		return refuse("it needs at least one digit before the point")
	}
	if hasPoint && (frac == "" || !allDigits(frac)) {
		return refuse("it needs at least one digit after the point")
	}
	if len(frac) > maxScale {
		return refuse("a literal carries at most 255 digits after the point")
	}

	// Concatenated exactly as the lexer concatenates them, so a value that overflows here is the
	// value that would have come back as NumberTooLarge.
	units, err := strconv.ParseUint(whole+frac, 10, 64)
	if err != nil {
		return refuse("it has more digits than a literal holds")
	}
	if negative && units > uint64(math.MaxInt64) {
		return refuse("it has more digits than a negative literal holds")
	}
	return nil
}

func allDigits(s string) bool {
	for i := 0; i < len(s); i++ {
		if s[i] < '0' || s[i] > '9' {
			return false
		}
	}
	return len(s) > 0
}
