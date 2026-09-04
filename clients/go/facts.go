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

// The import body: one fact per line, `field record value`.
//
// A line format rather than JSON because ingest is the one route where volume matters. How the
// value is read is the field's kind to decide at the server - a number for an integer, true or
// false for a boolean, key@seconds for a time quantum field, a string for the rest - so this
// side does not interpret it either. See big_http::routes::query::parse_into.

// A Fact is one bit set at one address: a field, a record, and the value.
//
// Value is rendered with FactValue rather than with Literal: this route is not SQL and its
// values are not quoted. A string here is the text the server's big_embed::fact::from_text
// reads, and quoting it would make the quotes part of the key.
type Fact struct {
	Field  string
	Record uint64
	Value  any
}

// FactValue renders one value the way the import route reads it.
//
// The framing is the separator, so the two things a value must not contain are a space and a
// newline. Both are refused rather than escaped - the format has no escape, and a client that
// invented one would be writing a format the server does not read.
func FactValue(v any) (string, error) {
	var s string
	switch x := v.(type) {
	case nil:
		return "", &ValueError{What: "a fact with no value; leave the fact out"}
	case string:
		s = x
	case bool:
		s = strconv.FormatBool(x)
	case int:
		s = strconv.FormatInt(int64(x), 10)
	case int8:
		s = strconv.FormatInt(int64(x), 10)
	case int16:
		s = strconv.FormatInt(int64(x), 10)
	case int32:
		s = strconv.FormatInt(int64(x), 10)
	case int64:
		s = strconv.FormatInt(x, 10)
	case uint:
		s = strconv.FormatUint(uint64(x), 10)
	case uint8:
		s = strconv.FormatUint(uint64(x), 10)
	case uint16:
		s = strconv.FormatUint(uint64(x), 10)
	case uint32:
		s = strconv.FormatUint(uint64(x), 10)
	case uint64:
		s = strconv.FormatUint(x, 10)
	case float32:
		s = strconv.FormatFloat(float64(x), 'f', -1, 32)
	case float64:
		s = strconv.FormatFloat(x, 'f', -1, 64)
	case Decimal:
		s = string(x)
	case Keyed:
		s = x.Key + "@" + strconv.FormatInt(x.At, 10)
	default:
		return "", &ValueError{What: "a fact value of an unsupported type"}
	}

	if s == "" {
		return "", &ValueError{What: "a fact value with nothing in it"}
	}
	// A space is fine. The server cuts a line at its first two spaces and takes the whole rest
	// as the value (big_http::routes::query::three, which says so: "the value keeps whatever
	// spaces it contains, which is what a keyed value needs"). Refusing spaces here would have
	// made a perfectly ordinary value unwritable.
	if strings.ContainsAny(s, "\r\n") {
		// A newline ends the line, so this value would arrive as two facts or as one malformed
		// one. There is no escape in this format, and inventing one would be inventing a format
		// the server does not read.
		return "", &ValueError{What: "a fact value cannot contain a newline, and this one " +
			"does: " + strconv.Quote(s)}
	}
	// The server trims the line before splitting it, so whitespace at either end is framing
	// rather than data - it would simply be gone. Saying so beats writing a value that comes
	// back subtly different from the one that was sent.
	if s != strings.Trim(s, " \t\v\f") {
		return "", &ValueError{What: "a fact value cannot begin or end with whitespace, " +
			"because the server trims the line before reading it: " + strconv.Quote(s)}
	}
	return s, nil
}

// line writes one fact as the server reads it.
func (f Fact) line() (string, error) {
	if f.Field == "" {
		return "", &ValueError{What: "a fact with no field name"}
	}
	// The field name is cut at the first space, so unlike a value it really cannot hold one.
	if strings.ContainsAny(f.Field, " \t\r\n") {
		return "", &ValueError{What: "a field name cannot contain a space or a newline: " +
			strconv.Quote(f.Field)}
	}
	v, err := FactValue(f.Value)
	if err != nil {
		return "", err
	}
	return f.Field + " " + strconv.FormatUint(f.Record, 10) + " " + v, nil
}

// RenderFacts writes a whole batch, refusing before the socket opens if it is too large.
//
// The ceiling is checked as the body grows, and the refusal names the line that crossed it.
// big_http checks MAX_BODY against Content-Length before reading the body, so an over-large
// request is a 413 rather than a half-written one - but a producer that sent a million facts
// needs to know which one, not that there was one. That is the same reason the server's own
// Error::MessageTooLarge carries { at, len, cap }.
func RenderFacts(facts []Fact, maxBytes int) ([]byte, error) {
	var b strings.Builder
	for i, f := range facts {
		line, err := f.line()
		if err != nil {
			return nil, &ValueError{What: "fact " + strconv.Itoa(i+1) + ": " + unwrapWhat(err)}
		}
		if b.Len()+len(line)+1 > maxBytes {
			return nil, &TooLargeError{Bytes: b.Len() + len(line) + 1, Cap: maxBytes, Line: i + 1}
		}
		b.WriteString(line)
		b.WriteByte('\n')
	}
	return []byte(b.String()), nil
}

// RenderRecords writes a delete body: one record id per line.
func RenderRecords(records []uint64, maxBytes int) ([]byte, error) {
	var b strings.Builder
	for i, r := range records {
		line := strconv.FormatUint(r, 10)
		if b.Len()+len(line)+1 > maxBytes {
			return nil, &TooLargeError{Bytes: b.Len() + len(line) + 1, Cap: maxBytes, Line: i + 1}
		}
		b.WriteString(line)
		b.WriteByte('\n')
	}
	return []byte(b.String()), nil
}
