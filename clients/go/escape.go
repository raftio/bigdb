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

import "strings"

// EscapeSegment percent-encodes anything that would change the shape of a URL.
//
// A table name is user data and the engine allows more in one than a path segment does. Not a
// general encoder: it escapes what the server's router splits on and what a query string is
// delimited by, and leaves the rest, because a name that arrives mangled is worse than one that
// arrives long.
//
// Byte for byte the same rule as big_bin::client::mod::escape - unreserved characters through,
// everything else as %XX over the UTF-8 bytes. Two clients with two encoders would be two sets
// of rules about which byte is safe, and the table that arrived at one would not be the table
// that arrived at the other.
func EscapeSegment(s string) string {
	var b strings.Builder
	b.Grow(len(s))
	for i := 0; i < len(s); i++ {
		c := s[i]
		if unreserved(c) {
			b.WriteByte(c)
			continue
		}
		b.WriteByte('%')
		b.WriteByte(hex[c>>4])
		b.WriteByte(hex[c&0x0f])
	}
	return b.String()
}

const hex = "0123456789ABCDEF"

func unreserved(c byte) bool {
	switch {
	case c >= 'A' && c <= 'Z', c >= 'a' && c <= 'z', c >= '0' && c <= '9':
		return true
	case c == '-' || c == '.' || c == '_' || c == '~':
		return true
	}
	return false
}

// queryString joins already-known-good keys to escaped values, dropping empty ones.
//
// Values are escaped with the same encoder as a path segment. In particular `+` is escaped
// rather than left alone: the server deliberately does not read `+` as a space
// (big_http::Request::param says why), so an engine called "bitmap+columnar" has to arrive with
// its plus intact, and %2B is how it does.
func queryString(pairs ...[2]string) string {
	var b strings.Builder
	for _, p := range pairs {
		if p[1] == "" {
			continue
		}
		if b.Len() == 0 {
			b.WriteByte('?')
		} else {
			b.WriteByte('&')
		}
		b.WriteString(p[0])
		b.WriteByte('=')
		b.WriteString(EscapeSegment(p[1]))
	}
	return b.String()
}
