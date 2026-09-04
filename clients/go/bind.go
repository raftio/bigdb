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
	"strings"
)

// Parameter binding, and the one scanner that makes it safe.
//
// # Why the placeholder is ?
//
// This dialect has no server-side parameters; every argument is substituted as text before the
// statement is sent. So the marker has to be a character the lexer never produces in a valid
// statement outside a string literal or a comment:
//
//   - ? is not a token. big_sql::lex falls through to its `other` arm and the statement is a
//     syntax error. An unbound ? cannot quietly mean something, and a ? left over after
//     substitution is caught by the server with a clear sentence.
//   - % IS a token (Tok::Arith). A %s-style marker would collide with modulo, and worse, with a
//     % inside a string literal - LIKE 'a%b' - forcing every caller to double it.
//
// # Why the scanner is shared
//
// The same four-state walk answers three questions: where the placeholders are, how many there
// are, and where the last top-level VALUES is when a batch is being merged. Three separate
// scanners would be three chances to disagree about whether a ? inside '...' is a placeholder.

// scanState is where the walk is in a statement.
type scanState int

const (
	outside   scanState = iota
	inText              // '...'  with '' meaning one '
	inIdent             // "..."  with "" meaning one "
	inComment           // -- ... to end of line
)

// scanPlaceholders walks a statement and calls at(i) for each byte index holding a top-level ?.
//
// The comment state has only the -- form: big_sql::lex says that is the only comment this
// dialect has, so there is no /* */ state to get wrong.
func scanPlaceholders(stmt string, at func(i int)) {
	state := outside
	for i := 0; i < len(stmt); i++ {
		c := stmt[i]
		switch state {
		case outside:
			switch {
			case c == '\'':
				state = inText
			case c == '"':
				state = inIdent
			case c == '-' && i+1 < len(stmt) && stmt[i+1] == '-':
				state = inComment
				i++
			case c == '?':
				at(i)
			}
		case inText:
			if c == '\'' {
				// A doubled quote is one quote and the literal continues.
				if i+1 < len(stmt) && stmt[i+1] == '\'' {
					i++
				} else {
					state = outside
				}
			}
		case inIdent:
			if c == '"' {
				if i+1 < len(stmt) && stmt[i+1] == '"' {
					i++
				} else {
					state = outside
				}
			}
		case inComment:
			if c == '\n' {
				state = outside
			}
		}
	}
}

// CountPlaceholders is how many arguments Bind will want.
func CountPlaceholders(stmt string) int {
	n := 0
	scanPlaceholders(stmt, func(int) { n++ })
	return n
}

// Bind substitutes args for the top-level ? markers in stmt.
//
// A mismatch in either direction is an error naming both numbers, because "wrong number of
// arguments" without them sends the caller back to count by hand.
func Bind(stmt string, args ...any) (string, error) {
	var at []int
	scanPlaceholders(stmt, func(i int) { at = append(at, i) })

	if len(at) != len(args) {
		return "", &ValueError{What: fmt.Sprintf(
			"the statement has %d placeholder(s) and %d argument(s) were given",
			len(at), len(args))}
	}
	if len(at) == 0 {
		return stmt, nil
	}

	var b strings.Builder
	b.Grow(len(stmt) + 16*len(args))
	prev := 0
	for n, i := range at {
		lit, err := Literal(args[n])
		if err != nil {
			// Which argument, because a statement with nine of them and one refusal is a
			// sentence the caller has to bisect otherwise.
			return "", &ValueError{What: fmt.Sprintf("argument %d: %s", n+1, unwrapWhat(err))}
		}
		b.WriteString(stmt[prev:i])
		b.WriteString(lit)
		prev = i + 1
	}
	b.WriteString(stmt[prev:])
	return b.String(), nil
}

func unwrapWhat(err error) string {
	if v, ok := err.(*ValueError); ok {
		return v.What
	}
	return err.Error()
}

// lastTopLevelValues is the byte index just past the last top-level VALUES keyword, or -1.
//
// Used to merge a batch of INSERTs into one statement. Top-level means the same thing it means
// to scanPlaceholders: not inside a literal, an identifier or a comment - so a column called
// "values" and a string 'values' are both correctly ignored.
func lastTopLevelValues(stmt string) int {
	const word = "values"
	found := -1
	state := outside
	for i := 0; i < len(stmt); i++ {
		c := stmt[i]
		switch state {
		case outside:
			switch {
			case c == '\'':
				state = inText
			case c == '"':
				state = inIdent
			case c == '-' && i+1 < len(stmt) && stmt[i+1] == '-':
				state = inComment
				i++
			default:
				if i+len(word) <= len(stmt) &&
					strings.EqualFold(stmt[i:i+len(word)], word) &&
					!identByte(prevByte(stmt, i)) &&
					!identByte(nextByte(stmt, i+len(word))) {
					found = i + len(word)
					i += len(word) - 1
				}
			}
		case inText:
			if c == '\'' {
				if i+1 < len(stmt) && stmt[i+1] == '\'' {
					i++
				} else {
					state = outside
				}
			}
		case inIdent:
			if c == '"' {
				if i+1 < len(stmt) && stmt[i+1] == '"' {
					i++
				} else {
					state = outside
				}
			}
		case inComment:
			if c == '\n' {
				state = outside
			}
		}
	}
	return found
}

func prevByte(s string, i int) byte {
	if i == 0 {
		return ' '
	}
	return s[i-1]
}

func nextByte(s string, i int) byte {
	if i >= len(s) {
		return ' '
	}
	return s[i]
}

func identByte(c byte) bool {
	return c == '_' || c >= 'A' && c <= 'Z' || c >= 'a' && c <= 'z' || c >= '0' && c <= '9'
}

// MergeInserts folds several rows of one INSERT template into as few statements as possible.
//
// # Why merging is worth the code
//
// The server commits once per request. contrib/big-message/readme.md measured the same four
// million facts arriving as 250 statements in 3.36s and as 5 statements in 1.81s - so a loop
// that sends one INSERT per row is paying for a commit per row. database/sql has no ExecMany,
// which is why this is exposed here rather than hiding under Exec: a caller who wants the
// merge should be able to see that they asked for it.
//
// # How the split is decided
//
// The template is cut at its last top-level VALUES, using the same scanner that finds
// placeholders - so a column called "values" and a string 'values' are both correctly ignored.
// Everything before the cut is the header and must carry no placeholders; everything after is
// one tuple and must carry exactly as many as a row has values. A template that does not have
// that shape is not merged, and the rows come back one statement each: falling back is always
// correct, and guessing at an unfamiliar shape is not.
//
// maxBytes and maxRows are the same two ceilings contrib/big-message/src/batch.rs uses.
func MergeInserts(stmt string, rows [][]any, maxBytes, maxRows int) ([]string, error) {
	if len(rows) == 0 {
		return nil, nil
	}

	one := func() ([]string, error) {
		out := make([]string, 0, len(rows))
		for _, r := range rows {
			s, err := Bind(stmt, r...)
			if err != nil {
				return nil, err
			}
			out = append(out, s)
		}
		return out, nil
	}

	cut := lastTopLevelValues(stmt)
	if cut < 0 {
		return one()
	}
	header, tuple := stmt[:cut], stmt[cut:]
	if CountPlaceholders(header) != 0 || CountPlaceholders(tuple) != len(rows[0]) {
		return one()
	}

	var out []string
	var b strings.Builder
	held := 0

	flush := func() {
		if held > 0 {
			out = append(out, b.String())
			b.Reset()
			held = 0
		}
	}

	for i, r := range rows {
		if len(r) != len(rows[0]) {
			// Ragged rows are not one template's worth of arguments. One statement each, and
			// Bind will say which row is wrong if any is.
			return one()
		}
		bound, err := Bind(tuple, r...)
		if err != nil {
			return nil, fmt.Errorf("row %d: %w", i+1, err)
		}
		if held > 0 && (b.Len()+1+len(bound) > maxBytes || held >= maxRows) {
			flush()
		}
		if held == 0 {
			b.WriteString(header)
			if len(header)+len(bound) > maxBytes {
				// One row that cannot fit in any request at all. Splitting further will never
				// help, so say so rather than emitting a statement the server will refuse.
				return nil, &TooLargeError{Bytes: len(header) + len(bound), Cap: maxBytes, Line: i + 1}
			}
		} else {
			b.WriteByte(',')
		}
		b.WriteString(bound)
		held++
	}
	flush()
	return out, nil
}
