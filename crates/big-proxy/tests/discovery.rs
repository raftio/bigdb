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

//! Following the cluster's membership instead of a list read once at startup.
//!
//! The list this proxy is given names the cluster as it was when the process started. A node
//! admitted after that is one no request could ever reach, which is the gap these tests are
//! about - and the two things that must not follow from closing it are a node entering
//! rotation without having answered a probe, and a node losing its health record every time
//! the membership is read again.

use big_proxy::health::{Policy, Verdict, Why};
use big_proxy::pool::Pool;
use big_proxy::upstream::Upstream;

fn members(names: &[(&str, &str)]) -> Vec<(String, String)> {
    names.iter().map(|(n, a)| (n.to_string(), a.to_string())).collect()
}

fn pool_of(names: &[(&str, &str)]) -> Pool {
    let ups = names.iter().map(|(n, a)| Upstream::new(*n, *a)).collect();
    Pool::new(ups, Policy::default(), 2)
}

/// A node the cluster has admitted since this proxy started becomes reachable.
#[test]
fn a_node_admitted_later_is_adopted() {
    let pool = pool_of(&[("a", "127.0.0.1:1")]);

    let (added, removed) = pool.adopt(
        &members(&[("a", "127.0.0.1:1"), ("d", "127.0.0.1:4")]),
        Policy::default(),
        None,
    );

    assert_eq!(added, vec!["d".to_string()]);
    assert!(removed.is_empty());
    let names: Vec<String> = pool.nodes().iter().map(|n| n.up.name().to_string()).collect();
    assert_eq!(names, vec!["a".to_string(), "d".to_string()]);
}

/// **A node that appears is not in rotation until it has answered a probe.**
///
/// Membership says a node exists; only the health check says it is worth a request. Letting
/// the first imply the second would send traffic to a node that has answered nothing - which
/// is exactly the failure the health check exists to prevent, reintroduced by the back door.
#[test]
fn an_adopted_node_waits_for_the_health_check_like_any_other() {
    let pool = pool_of(&[("a", "127.0.0.1:1")]);
    pool.adopt(&members(&[("a", "127.0.0.1:1"), ("d", "127.0.0.1:4")]), Policy::default(), None);

    let d = pool.nodes().into_iter().find(|n| n.up.name() == "d").expect("d was adopted");
    assert!(!d.in_rotation(), "adopted, and not yet trusted with a request");
}

/// **A node already known keeps the health record it had.**
///
/// The membership is read every couple of seconds. Rebuilding a node each time would restart
/// its health record just as often, so a node that had been failing would come back into
/// rotation on the next poll and stay there - a discovery loop that silently switched the
/// health check off.
#[test]
fn adopting_does_not_reset_what_is_known_about_a_node() {
    // No floor: the default protects a node from being ejected for its first six seconds,
    // which is the right rule in production and a thing to switch off in a test about ejection.
    let policy = Policy { floor: std::time::Duration::ZERO, ..Policy::default() };
    let pool = Pool::new(vec![Upstream::new("a", "127.0.0.1:1")], policy, 2);

    // Fail it out of rotation the way the poller would.
    let a = pool.nodes().into_iter().next().unwrap();
    for _ in 0..policy.fail {
        a.observe(Verdict::Down(Why::Unreachable));
    }
    assert!(!a.in_rotation(), "the fixture has to start from a node that is out");

    pool.adopt(&members(&[("a", "127.0.0.1:1"), ("d", "127.0.0.1:4")]), policy, None);

    let after = pool.nodes().into_iter().find(|n| n.up.name() == "a").expect("a is still here");
    assert!(!after.in_rotation(), "still out: what was learned about it was not thrown away");
}

/// A node removed from the cluster stops being sent requests.
#[test]
fn a_node_the_cluster_dropped_is_removed() {
    let pool = pool_of(&[("a", "127.0.0.1:1"), ("b", "127.0.0.1:2")]);

    let (added, removed) = pool.adopt(&members(&[("a", "127.0.0.1:1")]), Policy::default(), None);

    assert!(added.is_empty());
    assert_eq!(removed, vec!["b".to_string()]);
    let names: Vec<String> = pool.nodes().iter().map(|n| n.up.name().to_string()).collect();
    assert_eq!(names, vec!["a".to_string()]);
}

/// A node that moved is the same node at a new address, so it is rebuilt rather than kept:
/// keeping it would leave this proxy talking to the machine it used to be on.
#[test]
fn a_node_that_moved_is_rebuilt_at_its_new_address() {
    let pool = pool_of(&[("a", "127.0.0.1:1")]);

    let (added, removed) = pool.adopt(&members(&[("a", "127.0.0.1:9")]), Policy::default(), None);

    assert_eq!(added, vec!["a".to_string()]);
    assert!(removed.is_empty(), "the name is still in the cluster: {removed:?}");
    assert_eq!(pool.nodes()[0].up.addr(), "127.0.0.1:9");
}

// -------------------------------------------------------------------------------------------
// Reading a topology answer
// -------------------------------------------------------------------------------------------

/// The answer this proxy reads is the one the daemon writes, so the test uses that shape
/// verbatim rather than a convenient subset.
const TOPOLOGY: &str = r#"{"epoch":3,"leader":"a","schema_leader":"a","members":[{"name":"a","addr":"10.0.0.1:7654","state":"voter"},{"name":"b","addr":"10.0.0.2:7654","state":"learner"}],"ranges":[{"id":0,"shards":"0..","primary":"a","holders":["a","b"]}],"behind":[]}"#;

#[test]
fn every_member_is_read_with_the_address_a_request_goes_to() {
    assert_eq!(
        big_proxy::discover::members_of(TOPOLOGY),
        vec![
            ("a".to_string(), "10.0.0.1:7654".to_string()),
            ("b".to_string(), "10.0.0.2:7654".to_string()),
        ]
    );
}

/// **A learner is a member and is read as one.** It holds no range yet and the health check is
/// what decides whether it gets a request - which is the division this whole loop rests on:
/// membership says a node exists, the probe says it is worth asking.
#[test]
fn a_learner_is_a_member_like_any_other() {
    let members = big_proxy::discover::members_of(TOPOLOGY);
    assert!(members.iter().any(|(name, _)| name == "b"));
}

/// **`ranges` is not read, and the test says so.** Which node holds which shard is the
/// agreement's business; a proxy that started routing by range would be making a decision it
/// has no way of being told it got wrong. The `holders` array names `a` and `b` and must not
/// become a third and fourth member.
#[test]
fn what_a_node_holds_is_not_mistaken_for_who_is_in_the_cluster() {
    assert_eq!(big_proxy::discover::members_of(TOPOLOGY).len(), 2);
}

/// An answer with no members - or one this proxy cannot read - changes nothing rather than
/// emptying the rotation.
#[test]
fn an_unreadable_answer_names_nobody() {
    assert!(big_proxy::discover::members_of("not json at all").is_empty());
    assert!(big_proxy::discover::members_of(r#"{"members":[]}"#).is_empty());
}

// -------------------------------------------------------------------------------------------
// Seeding one in while it runs
// -------------------------------------------------------------------------------------------

/// **A bind that is not loopback is refused, and the message says why rather than what.**
///
/// This port can point every client's traffic at a machine of the caller's choosing, and this
/// proxy checks no credential of its own - so the bind is the whole of the access control.
#[test]
fn the_seeding_port_refuses_to_listen_anywhere_but_loopback() {
    let err = big_proxy::admin::Admin::bind(
        "0.0.0.0:0",
        Policy::default(),
        None,
        None,
        std::time::Duration::from_secs(1),
    )
    .expect_err("0.0.0.0 is not loopback");

    assert!(err.contains("loopback"), "{err}");
    assert!(err.contains("credential"), "it says why, not just what: {err}");
}

#[test]
fn the_seeding_port_binds_on_loopback() {
    let admin = big_proxy::admin::Admin::bind(
        "127.0.0.1:0",
        Policy::default(),
        None,
        None,
        std::time::Duration::from_secs(1),
    )
    .expect("127.0.0.1 is loopback");
    assert!(admin.local_addr().is_ok());
}

/// A seeded upstream is added, and starts out of rotation like anything else discovered.
#[test]
fn a_seeded_upstream_starts_out_of_rotation() {
    let pool = pool_of(&[("a", "127.0.0.1:1")]);

    assert!(pool.adopt_one(Upstream::new("b", "127.0.0.1:2"), Policy::default()));

    let b = pool.nodes().into_iter().find(|n| n.up.name() == "b").expect("b was seeded");
    assert!(!b.in_rotation(), "seeded, and not yet trusted with a request");
}

/// Seeding a name that is already here moves it rather than adding a second entry: a name is
/// one node, and saying it again at a new address is that node having moved.
#[test]
fn seeding_a_name_that_is_already_here_moves_it() {
    let pool = pool_of(&[("a", "127.0.0.1:1")]);

    assert!(!pool.adopt_one(Upstream::new("a", "127.0.0.1:9"), Policy::default()), "not new");

    assert_eq!(pool.nodes().len(), 1);
    assert_eq!(pool.nodes()[0].up.addr(), "127.0.0.1:9");
}

#[test]
fn an_upstream_can_be_forgotten_and_forgetting_an_unknown_one_says_so() {
    let pool = pool_of(&[("a", "127.0.0.1:1"), ("b", "127.0.0.1:2")]);

    assert!(pool.forget("b"));
    assert!(!pool.forget("b"), "gone already");
    assert_eq!(pool.nodes().len(), 1);
}
