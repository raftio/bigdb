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
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/tls"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/pem"
	"math/big"
	"net"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// TLS, against a raw TLS listener.
//
// The certificate is generated with crypto/x509 rather than by shelling out to openssl, which
// is what the repository's Rust e2e tests have to do. That is one fewer thing to skip on: these
// run everywhere Go runs.

func selfSigned(t *testing.T, host string) (tls.Certificate, string) {
	t.Helper()

	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	tmpl := x509.Certificate{
		SerialNumber:          big.NewInt(1),
		Subject:               pkix.Name{CommonName: host},
		NotBefore:             time.Now().Add(-time.Hour),
		NotAfter:              time.Now().Add(time.Hour),
		KeyUsage:              x509.KeyUsageDigitalSignature | x509.KeyUsageCertSign,
		ExtKeyUsage:           []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth},
		IsCA:                  true,
		BasicConstraintsValid: true,
	}
	if ip := net.ParseIP(host); ip != nil {
		tmpl.IPAddresses = []net.IP{ip}
	} else {
		tmpl.DNSNames = []string{host}
	}

	der, err := x509.CreateCertificate(rand.Reader, &tmpl, &tmpl, &key.PublicKey, key)
	if err != nil {
		t.Fatal(err)
	}
	certPEM := pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der})
	keyDER, err := x509.MarshalECPrivateKey(key)
	if err != nil {
		t.Fatal(err)
	}
	keyPEM := pem.EncodeToMemory(&pem.Block{Type: "EC PRIVATE KEY", Bytes: keyDER})

	pair, err := tls.X509KeyPair(certPEM, keyPEM)
	if err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(t.TempDir(), "ca.pem")
	if err := os.WriteFile(path, certPEM, 0o600); err != nil {
		t.Fatal(err)
	}
	return pair, path
}

// serveTLS is the raw fake server wrapped in TLS.
func serveTLS(t *testing.T, cert tls.Certificate, h handler) *fake {
	t.Helper()
	ln, err := tls.Listen("tcp", "127.0.0.1:0", &tls.Config{Certificates: []tls.Certificate{cert}})
	if err != nil {
		t.Fatal(err)
	}
	f := &fake{t: t, ln: ln}
	go f.loop(h)
	t.Cleanup(func() { _ = ln.Close() })
	return f
}

func TestTLSFollowsTheSchemeAndVerifiesAgainstTheCAFile(t *testing.T) {
	cert, caPath := selfSigned(t, "127.0.0.1")
	f := serveTLS(t, cert, ok(`{"count":1}`))

	c, err := New("https://"+f.addr(), WithCAFile(caPath), WithRetries(0), WithTimeout(3*time.Second))
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()

	a, err := c.Query(context.Background(), "tx", "Count(All())")
	if err != nil {
		t.Fatal(err)
	}
	if a.(*CountAnswer).Count != 1 {
		t.Errorf("count = %#v", a)
	}
}

func TestAnUntrustedCertificateIsRefused(t *testing.T) {
	// The refusal is the feature. A client that fell back to plaintext, or that skipped
	// verification when the handshake failed, would be downgrading a connection the caller
	// asked to be encrypted.
	cert, _ := selfSigned(t, "127.0.0.1")
	f := serveTLS(t, cert, ok(`{}`))

	c, err := New("https://"+f.addr(), WithRetries(0), WithTimeout(3*time.Second))
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()

	if _, err := c.Health(context.Background()); err == nil {
		t.Fatal("a certificate signed by nobody the system trusts must be refused")
	}
}

func TestInsecureSkipVerifyDoesWhatItsNameSays(t *testing.T) {
	cert, _ := selfSigned(t, "127.0.0.1")
	f := serveTLS(t, cert, ok(`{}`))

	c, err := New("https://"+f.addr(),
		WithInsecureSkipVerify(true), WithRetries(0), WithTimeout(3*time.Second))
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()

	if _, err := c.Health(context.Background()); err != nil {
		t.Fatalf("verification was turned off, so this must connect: %v", err)
	}
}

func TestTheCertificateIsCheckedAgainstTheNameNotTheAuthority(t *testing.T) {
	// A certificate is issued to a name, and "example:7654" is not a name. This is the test
	// that the port is stripped before the check - without it, every TLS connection would fail
	// with a confusing mismatch.
	cert, caPath := selfSigned(t, "127.0.0.1")
	f := serveTLS(t, cert, ok(`{}`))

	c, err := New("https://"+f.addr(), WithCAFile(caPath), WithRetries(0), WithTimeout(3*time.Second))
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()

	// The certificate names 127.0.0.1 and nothing else, so this only succeeds if ServerName is
	// the host with the port removed.
	if _, err := c.Health(context.Background()); err != nil {
		t.Fatalf("the port must be stripped before the name is checked: %v", err)
	}
	if got := c.Addr().Host; got != "127.0.0.1" {
		t.Errorf("Host = %q, want no port", got)
	}
}

func TestAnUnreadableCAFileIsAConfigError(t *testing.T) {
	for _, c := range []struct {
		name string
		set  func(t *testing.T) string
	}{
		{"missing", func(*testing.T) string { return "/nonexistent/ca.pem" }},
		{"not a certificate", func(t *testing.T) string {
			p := filepath.Join(t.TempDir(), "ca.pem")
			if err := os.WriteFile(p, []byte("this is not a certificate"), 0o600); err != nil {
				t.Fatal(err)
			}
			return p
		}},
	} {
		t.Run(c.name, func(t *testing.T) {
			_, err := New("https://example.com:7654", WithCAFile(c.set(t)))
			var ce *ConfigError
			if !asConfigError(err, &ce) {
				t.Fatalf("want a *ConfigError, got %#v", err)
			}
			if !strings.Contains(err.Error(), "CA file") && !strings.Contains(err.Error(), "certificates") {
				t.Errorf("the message must say what was wrong: %v", err)
			}
		})
	}
}

func asConfigError(err error, target **ConfigError) bool {
	if ce, ok := err.(*ConfigError); ok {
		*target = ce
		return true
	}
	return false
}
