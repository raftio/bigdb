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
	"net"
	"strings"
)

// Address is where to connect, and whether to do it over TLS.
//
// # TLS follows the scheme, and only the scheme
//
// Same rule as big_bin::client::http::transport: https means TLS, http means none, and a bare
// host:port means none. There is no port-sniffing and no "try TLS and fall back", because a
// fallback is a downgrade that a caller did not ask for and cannot see.
type Address struct {
	// Host with no port. This is what goes in the certificate check, because a certificate is
	// issued to a name and "example:7654" is not a name.
	Host string
	Port string
	TLS  bool
}

// Dial is host:port, ready for net.Dial. IPv6 hosts come back bracketed.
func (a Address) Dial() string { return net.JoinHostPort(a.Host, a.Port) }

// HostHeader is what goes in the Host header: the authority as written, brackets and all.
func (a Address) HostHeader() string { return net.JoinHostPort(a.Host, a.Port) }

// IsLoopback reports whether this address is one only this machine can reach.
//
// Used for one decision: whether sending a password in the clear deserves a warning. It does
// not, on loopback - "127.0.0.1:7654 with a users file" is an entirely ordinary way to run
// this, and a client that complained about it would teach people to ignore its warnings.
func (a Address) IsLoopback() bool {
	if ip := net.ParseIP(a.Host); ip != nil {
		return ip.IsLoopback()
	}
	h := strings.ToLower(a.Host)
	return h == "localhost" || strings.HasSuffix(h, ".localhost")
}

// ParseAddress reads "host:port", "http://host:port" or "https://host:port".
//
// The port is required in the bare form. Guessing 7654 for a caller who wrote "example.com"
// would connect them somewhere they did not name.
func ParseAddress(s string) (Address, error) {
	bad := func(why string) (Address, error) {
		return Address{}, &ConfigError{What: why}
	}

	useTLS := false
	rest := s
	switch {
	case strings.HasPrefix(s, "https://"):
		useTLS, rest = true, s[len("https://"):]
	case strings.HasPrefix(s, "http://"):
		rest = s[len("http://"):]
	case strings.Contains(s, "://"):
		return bad(s + " is not an address this client dials: use http://, https:// or host:port")
	}

	// A path would be silently dropped otherwise, and a caller who wrote one meant something.
	if i := strings.IndexAny(rest, "/?#"); i >= 0 {
		if rest[i:] != "/" {
			return bad(s + " carries a path; this client addresses a server, not a route")
		}
		rest = rest[:i]
	}
	if rest == "" {
		return bad("an address with nothing in it")
	}

	host, port, err := net.SplitHostPort(rest)
	if err != nil {
		return bad(s + " needs a port: this client does not guess one")
	}
	if host == "" {
		return bad(s + " has no host")
	}
	if port == "" {
		return bad(s + " has no port")
	}
	return Address{Host: host, Port: port, TLS: useTLS}, nil
}
