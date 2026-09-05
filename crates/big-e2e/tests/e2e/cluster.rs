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

//! Two `big serve` processes, two files, one query.
//!
//! **Ignored by default, and the reason is not that it is slow.** `big-http/tests/cluster.rs`
//! already covers what the coordinator decides, in-process and in-memory, where a test can hold
//! the clock still. What this file adds is the part that cannot be held still: two operating
//! system processes, two files on disk, and a failover decided by wall-clock heartbeats. That
//! makes it the one suite here whose result depends on whether the machine kept up, and a
//! timing test in the default run is a test that eventually fails for reasons nobody changed.
//!
//! Run it deliberately: `make e2e-cluster`, or `cargo test -p big-e2e -- --ignored`.

use crate::common::*;
use std::path::Path;

/// Two nodes, each owning half the shard space, written into the workspace.
///
/// The addresses have to be in the file *before* either daemon starts, so the ports are taken
/// first and the file is written around them.
fn two_node_file(dir: &Path, a: &str, b: &str) -> std::path::PathBuf {
    let path = dir.join("cluster.toml");
    std::fs::write(
        &path,
        format!(
            "schema_leader = \"a\"\n\n\
             [[node]]\nname = \"a\"\naddr = \"{a}\"\nshards = \"0..64\"\n\n\
             [[node]]\nname = \"b\"\naddr = \"{b}\"\nshards = \"64..\"\n"
        ),
    )
    .expect("a cluster file");
    path
}

/// Two daemons on one cluster file, each on its own database.
fn pair(workspace: &Workspace) -> (Daemon, Daemon) {
    // Ports first: the file names them, so they cannot be chosen by the daemons.
    let (port_a, port_b) = (reserved_port(), reserved_port());
    let (addr_a, addr_b) = (format!("127.0.0.1:{port_a}"), format!("127.0.0.1:{port_b}"));
    let file = two_node_file(workspace.path(), &addr_a, &addr_b);
    let cluster = file.display().to_string();

    let a = workspace.daemon_at("a.big", &addr_a, &["--cluster", &cluster, "--node", "a"]);
    let b = workspace.daemon_at("b.big", &addr_b, &["--cluster", &cluster, "--node", "b"]);
    (a, b)
}

/// The schema, created once at the leader, which applies it everywhere.
fn stock(a: &Daemon) {
    a.bigctl(&["create", "table", "tx"]).expect(0);
    a.bigctl(&["create", "field", "tx", "amount", "--kind", "int", "--bit-depth", "20"]).expect(0);
    a.bigctl(&["create", "field", "tx", "country", "--kind", "set"]).expect(0);
}

/// A record id in the first range, and one in the second.
///
/// A record names its shard by a shift, so the id is what decides the owner - which is why
/// placement needs no agreement and why this test can pick an owner by arithmetic.
const IN_A: u64 = 1;
const IN_B: u64 = 64 << 20;

#[test]
#[ignore = "two real processes and a wall-clock failover; run it deliberately"]
fn a_query_is_merged_from_two_processes() {
    let workspace = Workspace::new();
    let (a, b) = pair(&workspace);
    stock(&a);

    // Each fact lands on whichever node owns its record, whichever node was asked.
    a.bigctl_stdin(
        &["import", "tx", "-"],
        &format!("country {IN_A} GB\namount {IN_A} 100\ncountry {IN_B} US\namount {IN_B} 250\n"),
    )
    .expect(0);

    for (who, node) in [("a", &a), ("b", &b)] {
        let answer =
            node.bigctl(&["--format", "json", "sql", "SELECT count(*), sum(amount) FROM tx"]);
        let answer = answer.expect(0);
        assert!(answer.out.contains("[2,350]"), "{who} merged both owners' shares: {}", answer.out);
    }
}

#[test]
#[ignore = "two real processes and a wall-clock failover; run it deliberately"]
fn an_owner_that_is_gone_fails_the_query_naming_its_range() {
    // **The CAP choice, over real processes.** An answer here is an aggregate, so a count
    // missing one node's contribution looks exactly like a correct count. The refusal names the
    // range, because "which part of the space went quiet" is what an operator cannot work out
    // from a bare 503.
    let workspace = Workspace::new();
    let (a, b) = pair(&workspace);
    stock(&a);
    a.bigctl_stdin(&["import", "tx", "-"], &format!("country {IN_B} US\n")).expect(0);

    b.stop();

    let run = a.bigctl(&["sql", "SELECT count(*) FROM tx"]);

    assert_ne!(run.code, 0, "it refuses rather than answering: {:?}", run.out);
    assert!(run.said("64.."), "and names the range: {}{}", run.out, run.err);
}

#[test]
#[ignore = "two real processes and a wall-clock failover; run it deliberately"]
fn each_node_reports_its_own_shards_and_ignores_its_peers() {
    // A node that is ready is one that can serve its own shards. A readiness probe that failed
    // because a *different* machine is down would take a healthy node out of rotation for
    // somebody else's outage.
    let workspace = Workspace::new();
    let (a, b) = pair(&workspace);

    let ready_a = a.bigctl(&["--format", "json", "ready"]).expect(0);
    assert!(ready_a.out.contains("\"a\""), "names itself: {}", ready_a.out);
    assert!(ready_a.out.contains("0..64"), "and its own range: {}", ready_a.out);

    b.stop();
    let still = a.bigctl(&["ready"]);
    assert_eq!(still.code, 0, "and stays ready without its peer: {}", still.err);
}

#[test]
#[ignore = "two real processes and a wall-clock failover; run it deliberately"]
fn a_schema_change_reaches_every_node() {
    let workspace = Workspace::new();
    let (a, b) = pair(&workspace);
    stock(&a);

    // Asked of the node that is not the schema leader, so the answer had to travel.
    let schema = b.bigctl(&["--format", "json", "schema"]).expect(0);
    assert!(schema.out.contains("country"), "the follower has it: {}", schema.out);

    a.bigctl(&["drop", "field", "tx", "country"]).expect(0);
    let after = b.bigctl(&["--format", "json", "schema"]).expect(0);
    assert!(!after.out.contains("country"), "and the drop reached it too: {}", after.out);
}

// -------------------------------------------------------------------------------------------
// Joining with no cluster file
// -------------------------------------------------------------------------------------------

/// Three nodes and a copy of the range, which is the smallest cluster that commits anything.
///
/// A range with no copy runs no agreement, and a node is admitted *by* the agreement - so this
/// shape is not a detail of the test, it is the precondition of the feature.
fn a_named_group(dir: &Path, id: &str, a: &str, spare: &str, third: &str) -> std::path::PathBuf {
    let path = dir.join("cluster.toml");
    std::fs::write(
        &path,
        format!(
            "cluster_id = \"{id}\"\nschema_leader = \"a\"\n\n\
             [[node]]\nname = \"a\"\naddr = \"{a}\"\nshards = \"0..\"\n\n\
             [[node]]\nname = \"a-spare\"\naddr = \"{spare}\"\nreplica = \"a\"\n\n\
             [[node]]\nname = \"a-third\"\naddr = \"{third}\"\nreplica = \"a\"\n"
        ),
    )
    .expect("a cluster file");
    path
}

/// **A fourth node joins holding no cluster file at all**, which is the whole feature end to
/// end: two flags and an address where there used to be a file that had to be copied to the
/// new machine and kept current.
///
/// The order matters and is the thing most likely to be got wrong, so it is what the test
/// asserts: `add-node` first, against a node already in the cluster, because the joining node's
/// certificate has to name somebody the roster already knows before it can be let through the
/// handshake at all.
#[test]
#[ignore = "four processes and an election; run with make e2e-cluster"]
fn a_node_with_no_cluster_file_joins_by_address() {
    let w = Workspace::new();
    let (pa, ps, pt, pd) = (reserved_port(), reserved_port(), reserved_port(), reserved_port());
    let addr = |p: u16| format!("127.0.0.1:{p}");
    let (aa, as_, at, ad) = (addr(pa), addr(ps), addr(pt), addr(pd));
    let file = a_named_group(w.path(), "big-e2e", &aa, &as_, &at).display().to_string();

    let a = w.daemon_at("a.big", &aa, &["--cluster", &file, "--node", "a"]);
    let _spare = w.daemon_at("s.big", &as_, &["--cluster", &file, "--node", "a-spare"]);
    let _third = w.daemon_at("t.big", &at, &["--cluster", &file, "--node", "a-third"]);
    // `bigctl` prints ``leader `a` `` when there is one and "no leader" when there is not, so
    // the backtick is what tells the two apart.
    until("an elected leader", || a.bigctl(&["cluster", "topology"]).said("leader `"));

    // **The step that has to come first.** Until this is committed, `d` names nobody the
    // cluster knows, and a node that dialled in would be refused by name.
    a.bigctl(&["cluster", "add-node", "d", &ad]).expect(0);

    // No `--cluster` anywhere in this command line. That is the point.
    let d = w.daemon_at("d.big", &ad, &["--join", &aa, "--cluster-id", "big-e2e", "--node", "d"]);

    until("d to see the cluster it joined", || d.bigctl(&["cluster", "topology"]).said("a-spare"));
    // One row per node, tab separated, so the name is the whole first field - `said("d")`
    // would match the `d` in an address or in `a-third`.
    let names_d = |r: &Run| r.out.lines().any(|line| line.starts_with("d\t"));

    let seen = d.bigctl(&["cluster", "topology"]).expect(0);
    assert!(names_d(&seen), "d is in the membership it was given: {}", seen.out);

    // And the cluster it dialled says the same, which is what makes this one cluster rather
    // than a node that believes it joined.
    let from_a = a.bigctl(&["cluster", "topology"]).expect(0);
    assert!(names_d(&from_a), "a holds d too: {}", from_a.out);
}

/// **A node the cluster has never been told about is refused at startup, by name**, rather than
/// starting up healthy and talking to nobody. The message names the command that was skipped.
#[test]
#[ignore = "three processes and an election; run with make e2e-cluster"]
fn a_node_that_was_never_added_is_refused_and_told_which_step_it_missed() {
    let w = Workspace::new();
    let (pa, ps, pt, pd) = (reserved_port(), reserved_port(), reserved_port(), reserved_port());
    let addr = |p: u16| format!("127.0.0.1:{p}");
    let (aa, as_, at, ad) = (addr(pa), addr(ps), addr(pt), addr(pd));
    let file = a_named_group(w.path(), "big-e2e", &aa, &as_, &at).display().to_string();

    let a = w.daemon_at("a.big", &aa, &["--cluster", &file, "--node", "a"]);
    let _spare = w.daemon_at("s.big", &as_, &["--cluster", &file, "--node", "a-spare"]);
    let _third = w.daemon_at("t.big", &at, &["--cluster", &file, "--node", "a-third"]);
    // `bigctl` prints ``leader `a` `` when there is one and "no leader" when there is not, so
    // the backtick is what tells the two apart.
    until("an elected leader", || a.bigctl(&["cluster", "topology"]).said("leader `"));

    // `add-node` deliberately not run.
    let refused = run(
        "big",
        &[
            "serve",
            &w.join("d.big").display().to_string(),
            &ad,
            "--join",
            &aa,
            "--cluster-id",
            "big-e2e",
            "--node",
            "d",
        ],
    );
    assert_ne!(refused.code, 0, "it exits rather than serving: {}{}", refused.out, refused.err);
    assert!(refused.said("add-node"), "and names the step: {}{}", refused.out, refused.err);
}

/// **`cluster join` does the half that belongs here and prints the half that does not.**
///
/// The two steps happen on two machines and only work in one order, so the verb that owns the
/// first one is also the thing best placed to say what the second one is - with the cluster's
/// name filled in, which is the flag an operator cannot work out from what they typed.
#[test]
#[ignore = "three processes and an election; run with make e2e-cluster"]
fn cluster_join_adds_the_node_and_prints_the_command_to_start_it() {
    let w = Workspace::new();
    let (pa, ps, pt, pd) = (reserved_port(), reserved_port(), reserved_port(), reserved_port());
    let addr = |p: u16| format!("127.0.0.1:{p}");
    let (aa, as_, at, ad) = (addr(pa), addr(ps), addr(pt), addr(pd));
    let file = a_named_group(w.path(), "big-e2e", &aa, &as_, &at).display().to_string();

    let a = w.daemon_at("a.big", &aa, &["--cluster", &file, "--node", "a"]);
    let _spare = w.daemon_at("s.big", &as_, &["--cluster", &file, "--node", "a-spare"]);
    let _third = w.daemon_at("t.big", &at, &["--cluster", &file, "--node", "a-third"]);
    until("an elected leader", || a.bigctl(&["cluster", "topology"]).said("leader `"));

    let joined = a.bigctl(&["cluster", "join", "d", &ad]).expect(0);

    // The command it prints has to be the one that works, so the test asserts every part an
    // operator would otherwise have to look up.
    assert!(joined.said("--join"), "{}", joined.out);
    assert!(
        joined.said("--cluster-id big-e2e"),
        "the cluster's name, read not guessed: {}",
        joined.out
    );
    assert!(joined.said("--node d"), "{}", joined.out);
    assert!(joined.said(&aa), "the address of a node already in the cluster: {}", joined.out);

    // And the node really was added, which is what makes the printed command work at all.
    let seen = a.bigctl(&["cluster", "topology"]).expect(0);
    assert!(seen.out.lines().any(|l| l.starts_with("d\t")), "d is a member: {}", seen.out);
}
