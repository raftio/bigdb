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

//go:build integration

package bigdb_test

import (
	"context"
	"errors"
	"fmt"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	bigdb "github.com/raftio/bigdb/clients/go"
)

// Integration, against a real `big serve`.
//
// Behind a build tag rather than testing.Short(): -short is for skipping slow tests, and these
// are tests that cannot run at all without something outside this module. A tag makes that
// visible in the command rather than hidden in a default flag.
//
//	go test -tags integration ./...
//
// # The fixture does not build the server
//
// A `go test` that ran `cargo build` would take minutes on a cold tree and fail outright on a
// machine with no Rust. So the binary is looked for and, when it is not there, the tests skip
// with a reason that says how to fix it.
//
// # What is covered here and not above
//
// Only what a stub would keep agreeing with long after the two sides had diverged: the
// escaping round trip, the line numbers in a refusal, the format-to-content-type mapping, the
// keep-alive ceiling, and the field-kind spellings.

var server *daemon

func TestMain(m *testing.M) {
	// Two outcomes, and they must not be confused. No binary is a skip: this module is meant to
	// be testable on a machine with no Rust. A binary that is there and does not answer is a
	// failure - reporting that as "ok" is how a suite goes green while testing nothing, which
	// is the exact failure this fixture exists to avoid.
	bin, err := findBinary()
	if err != nil {
		fmt.Fprintf(os.Stderr, "skipping integration tests: %v\n", err)
		os.Exit(0)
	}
	d, err := start(bin)
	if err != nil {
		fmt.Fprintf(os.Stderr, "the server at %s did not start: %v\n", bin, err)
		os.Exit(1)
	}
	server = d
	code := m.Run()
	d.stop()
	os.Exit(code)
}

type daemon struct {
	addr   string
	cmd    *exec.Cmd
	dir    string
	exited chan struct{}
}

// findBinary looks where the repository's own e2e tests look.
func findBinary() (string, error) {
	if p := os.Getenv("BIGDB_BIN"); p != "" {
		if _, err := os.Stat(p); err == nil {
			return p, nil
		}
		return "", fmt.Errorf("BIGDB_BIN is set to %s, which is not there", p)
	}
	root, err := repoRoot()
	if err != nil {
		return "", err
	}
	for _, p := range []string{"target/release/big", "target/debug/big"} {
		full := filepath.Join(root, p)
		if _, err := os.Stat(full); err == nil {
			return full, nil
		}
	}
	return "", errors.New("no `big` binary found; set BIGDB_BIN or run `make -C clients/go e2e`")
}

func repoRoot() (string, error) {
	dir, err := os.Getwd()
	if err != nil {
		return "", err
	}
	for i := 0; i < 6; i++ {
		if _, err := os.Stat(filepath.Join(dir, "Cargo.toml")); err == nil {
			return dir, nil
		}
		dir = filepath.Dir(dir)
	}
	return "", errors.New("could not find the repository root")
}

// freePort asks the OS for one and gives it straight back, which is what the repository's Rust
// e2e helper does too. It races with anything else doing the same, and in practice does not.
func freePort() (int, error) {
	l, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return 0, err
	}
	defer l.Close()
	return l.Addr().(*net.TCPAddr).Port, nil
}

func start(bin string) (*daemon, error) {
	port, err := freePort()
	if err != nil {
		return nil, err
	}
	dir, err := os.MkdirTemp("", "bigdb-go-e2e")
	if err != nil {
		return nil, err
	}
	addr := fmt.Sprintf("127.0.0.1:%d", port)

	// `big serve <file> [addr]` - both positional. --durability none because this is a
	// throwaway database in a temp directory and fsync per commit is the slowest thing in the
	// suite by a wide margin.
	cmd := exec.Command(bin, "serve", filepath.Join(dir, "db"), addr, "--durability", "none")
	cmd.Stdout, cmd.Stderr = os.Stderr, os.Stderr
	if err := cmd.Start(); err != nil {
		os.RemoveAll(dir)
		return nil, err
	}
	d := &daemon{addr: addr, cmd: cmd, dir: dir}
	// Watched rather than polled through ProcessState, which is only set once Wait returns.
	exited := make(chan struct{})
	go func() { _ = cmd.Wait(); close(exited) }()
	d.exited = exited

	// Polled rather than slept: "started" has to mean "answered", not "was spawned". `big
	// serve` takes an exclusive lock and refuses a corrupt file after the process already
	// exists, so a sleep would race with a failure that has not happened yet.
	c, err := bigdb.New(addr, bigdb.WithTimeout(time.Second))
	if err != nil {
		d.stop()
		return nil, err
	}
	defer c.Close()

	deadline := time.Now().Add(20 * time.Second)
	for time.Now().Before(deadline) {
		ctx, cancel := context.WithTimeout(context.Background(), time.Second)
		ok, err := c.Health(ctx)
		cancel()
		if err == nil && ok {
			return d, nil
		}
		select {
		case <-exited:
			d.cmd = nil // already reaped
			d.stop()
			return nil, errors.New("the server exited before it answered")
		default:
		}
		time.Sleep(100 * time.Millisecond)
	}
	d.stop()
	return nil, errors.New("the server did not answer /health within 20s")
}

func (d *daemon) stop() {
	if d.cmd != nil && d.cmd.Process != nil {
		_ = d.cmd.Process.Signal(os.Interrupt)
		select {
		case <-d.exited:
		case <-time.After(5 * time.Second):
			_ = d.cmd.Process.Kill()
			<-d.exited
		}
	}
	if d.dir != "" {
		_ = os.RemoveAll(d.dir)
	}
}

func client(t *testing.T, opts ...bigdb.Option) *bigdb.Client {
	t.Helper()
	c, err := bigdb.New(server.addr, opts...)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = c.Close() })
	return c
}

// table makes a uniquely named table so tests do not tread on each other, and drops it after.
//
// The name carries a counter as well as the test's name because several tests want more than
// one table, and because a name nothing else in the run uses keeps a failure local to the test
// that caused it.
func table(t *testing.T, c *bigdb.Client, fields map[string]string) string {
	t.Helper()
	name := fmt.Sprintf("t%d_%s", nextTable.Add(1),
		strings.ReplaceAll(strings.ToLower(t.Name()), "/", "_"))
	ctx := context.Background()
	if _, err := c.CreateTable(ctx, name); err != nil {
		t.Fatal(err)
	}
	// Asserted rather than assumed, and it stays asserted. This is what caught the schema
	// snapshot walking table ids from zero and stopping at the first gap a drop had left: the
	// create answered 200, the field route answered 200, and the table was in no snapshot. A
	// test that only checked the status codes would have gone green.
	s, err := c.Schema(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if s.Table(name) == nil {
		t.Fatalf("POST /table/%s answered 200 but the table is not in /schema", name)
	}
	for field, kind := range fields {
		if _, err := c.CreateField(ctx, name, field, kind); err != nil {
			t.Fatalf("create field %s %s: %v", field, kind, err)
		}
	}
	t.Cleanup(func() { _ = c.DropTable(context.Background(), name) })
	return name
}

// nextTable keeps names unique within a run.
var nextTable atomic.Int64
