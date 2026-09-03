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

//! Two nodes proving themselves to each other with certificates.
//!
//! **What this covers that nothing else can.** `big-tls`'s tests prove a handshake names a node;
//! `big-http`'s prove the route table keeps a person off `/internal/*`. Neither starts a second
//! process, so neither can catch the half of this that lives in `big serve`: reading the peer CA
//! out of the cluster file, handing the roster to the listener, and refusing to start a node that
//! has no certificate to present.
//!
//! Ignored by default for the same reason `cluster.rs` is - two real processes and real sockets -
//! and run by the same job.

use crate::common::*;
use std::path::{Path, PathBuf};

/// A CA and one certificate per node, made with `openssl` because that is what an operator has.
///
/// Returns the CA path. Each node's pair is `<dir>/<name>.pem` and `<dir>/<name>.key`.
fn issue(dir: &Path, names: &[&str]) -> Option<PathBuf> {
    let ca = dir.join("peer-ca.pem");
    let ca_key = dir.join("peer-ca.key");
    let ok = |args: &[&str]| {
        std::process::Command::new("openssl")
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    // Skipped rather than failed where there is no openssl. This suite is already opt-in, and a
    // machine without it is a machine that cannot run the deploy scripts either.
    if !ok(&[
        "req",
        "-x509",
        "-newkey",
        "ed25519",
        "-nodes",
        "-days",
        "1",
        "-keyout",
        ca_key.to_str()?,
        "-out",
        ca.to_str()?,
        "-subj",
        "/CN=big test ca",
    ]) {
        return None;
    }
    for name in names {
        let key = dir.join(format!("{name}.key"));
        let csr = dir.join(format!("{name}.csr"));
        let pem = dir.join(format!("{name}.pem"));
        let ext = dir.join(format!("{name}.ext"));
        // The SAN is what is actually checked - a CN is not, by anything current - and it has to
        // be the node's name from the cluster file.
        std::fs::write(
            &ext,
            format!("subjectAltName=DNS:{name}\nextendedKeyUsage=serverAuth,clientAuth\n"),
        )
        .ok()?;
        ok(&[
            "req",
            "-newkey",
            "ed25519",
            "-nodes",
            "-keyout",
            key.to_str()?,
            "-out",
            csr.to_str()?,
            "-subj",
            &format!("/CN={name}"),
        ]);
        ok(&[
            "x509",
            "-req",
            "-in",
            csr.to_str()?,
            "-days",
            "1",
            "-CA",
            ca.to_str()?,
            "-CAkey",
            ca_key.to_str()?,
            "-CAcreateserial",
            "-extfile",
            ext.to_str()?,
            "-out",
            pem.to_str()?,
        ]);
        set_mode(&key, 0o600);
    }
    Some(ca)
}

fn cluster_file(dir: &Path, ca: &Path, a: &str, b: &str) -> PathBuf {
    let path = dir.join("cluster.toml");
    std::fs::write(
        &path,
        format!(
            "schema_leader = \"a\"\npeer_ca_file = \"{}\"\n\n\
             [[node]]\nname = \"a\"\naddr = \"{a}\"\nshards = \"0..64\"\n\n\
             [[node]]\nname = \"b\"\naddr = \"{b}\"\nshards = \"64..\"\n",
            ca.display()
        ),
    )
    .expect("a cluster file");
    path
}

#[test]
#[ignore = "two real processes and openssl; run it deliberately"]
fn a_node_with_no_certificate_is_refused_rather_than_left_broken() {
    // **This is the case that used to be a warning.** Under a shared token, a node with none
    // could still be reached by peers that asked for none. Under mutual TLS a node with no
    // client certificate cannot reach `/internal/*` on any peer at all, so every fan-out would
    // return 403 and the cluster would be silently broken - which is worse than the old case,
    // and so it is an error rather than a line in a log.
    let workspace = Workspace::new();
    let dir = workspace.path();
    let Some(ca) = issue(dir, &["a"]) else { return };
    let (pa, pb) = (reserved_port(), reserved_port());
    let file = cluster_file(dir, &ca, &format!("127.0.0.1:{pa}"), &format!("127.0.0.1:{pb}"));

    let db = dir.join("a.big");
    let run = run(
        "big",
        &[
            "serve",
            &db.display().to_string(),
            &format!("127.0.0.1:{pa}"),
            "--cluster",
            &file.display().to_string(),
            "--node",
            "a",
        ],
    );

    assert_ne!(run.code, 0, "it must not start: {}", run.err);
    assert!(run.said("--peer-cert"), "it names what is missing: {}", run.err);
    assert!(run.said("silently broken") || run.said("cluster is broken"), "{}", run.err);
}

#[test]
#[ignore = "two real processes and openssl; run it deliberately"]
fn a_certificate_from_another_ca_does_not_make_a_node() {
    // Signed by *a* CA, just not this cluster's. The node starts - its own certificate loads
    // fine - and then cannot reach a peer, which is the failure the test above exists to
    // prevent an operator from reaching by accident.
    let workspace = Workspace::new();
    let dir = workspace.path();
    let Some(ca) = issue(dir, &["a"]) else { return };
    let stranger_dir = dir.join("stranger");
    std::fs::create_dir_all(&stranger_dir).unwrap();
    let Some(_) = issue(&stranger_dir, &["b"]) else { return };

    let (pa, pb) = (reserved_port(), reserved_port());
    let file = cluster_file(dir, &ca, &format!("127.0.0.1:{pa}"), &format!("127.0.0.1:{pb}"));
    let users = users_file(dir, "ops admin\n");

    // `a` holds a certificate this cluster's CA signed; `b` holds one it did not.
    let a = workspace.daemon_at(
        "a.big",
        &format!("127.0.0.1:{pa}"),
        &[
            "--cluster",
            &file.display().to_string(),
            "--node",
            "a",
            "--users",
            &users.display().to_string(),
            "--peer-cert",
            &dir.join("a.pem").display().to_string(),
            "--peer-key",
            &dir.join("a.key").display().to_string(),
        ],
    );
    let b = workspace.daemon_at(
        "b.big",
        &format!("127.0.0.1:{pb}"),
        &[
            "--cluster",
            &file.display().to_string(),
            "--node",
            "b",
            "--users",
            &users.display().to_string(),
            "--peer-cert",
            &stranger_dir.join("b.pem").display().to_string(),
            "--peer-key",
            &stranger_dir.join("b.key").display().to_string(),
        ],
    );

    let cred = credentials_file(dir, "ops");
    // **A schema change, not a query.** `SHOW TABLES` is answered out of the node that receives
    // it and never crosses a socket, so it succeeds whatever the peers are doing - which is what
    // an earlier version of this test asserted on, and why it passed against a broken cluster.
    // DDL has to reach every node, so it has to cross the connection that cannot be made.
    let asked = a.bigctl(&[
        "--credentials-file",
        &cred.display().to_string(),
        "sql",
        "CREATE TABLE tx (n INT)",
    ]);
    assert_ne!(
        asked.code, 0,
        "a schema change over a refused handshake must not report success: {} {}",
        asked.out, asked.err
    );
    a.stop();
    b.stop();
}
