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

//! Which nodes are worth sending a request to.
//!
//! The signal is `GET /ready`, which is unauthenticated and which the daemon writes for exactly
//! this reader:
//!
//! > `serving` is the part a probe can act on: a node that has lost touch with the agreement is
//! > refusing requests for its range, and a load balancer that keeps sending them is sending
//! > them somewhere that will answer `503`.
//!
//! Two properties of that route decide the shape of everything here.
//!
//! **It is always `200`.** `ready()` builds its answer with `Response::ok` unconditionally, so a
//! node that is refusing every request still answers `200`. The status line is not the signal;
//! the body is, and a poller that watched only the status would keep a dead node in rotation.
//!
//! **`serving` is absent on a node with no agreement.** The field lives inside a block the
//! daemon emits only when `controller()` is `Some`, so a solo node — and every node of an
//! unreplicated cluster — never sends it. Reading absence as `false` would empty the rotation of
//! a perfectly healthy deployment, which is why [`Verdict::of`] reads it as *serving*.

use std::time::{Duration, Instant};

/// Why a node is out of rotation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Why {
    /// The socket could not be opened, or the handshake failed.
    Unreachable,
    /// The probe did not finish inside its budget.
    Timeout,
    /// `/ready` answered something other than `200`.
    Status(u16),
    /// The body was not a readiness answer this proxy understands.
    Unreadable,
    /// The node said `"serving":false`. Not a failure — the engine is fine and the node is one
    /// promotion away — but it is refusing requests for its range, so it gets none.
    NotServing,
}

impl std::fmt::Display for Why {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Why::Unreachable => write!(f, "unreachable"),
            Why::Timeout => write!(f, "timeout"),
            Why::Status(s) => write!(f, "status_{s}"),
            Why::Unreadable => write!(f, "unreadable"),
            Why::NotServing => write!(f, "not_serving"),
        }
    }
}

/// What one probe saw.
#[derive(Clone, Debug)]
pub enum Verdict {
    /// The node answered and is serving. Carries what it said about itself, for `/ready`.
    Up(Report),
    Down(Why),
}

/// The parts of a node's `/ready` answer worth repeating.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct Report {
    pub node: Option<String>,
    pub shards: Option<String>,
    /// `None` on a node with no agreement, which is not the same as `Some(false)`.
    pub serving: Option<bool>,
    pub wire: Option<u32>,
}

impl Verdict {
    /// Read one `/ready` answer.
    ///
    /// Scanned rather than parsed with a JSON library, the way `PeerResponse::code` reads a code
    /// out of an error body. Four fields of one shape, written by this repository's own encoder,
    /// do not justify a dependency in a workspace that has twice declined to add one.
    pub fn of(status: u16, body: &[u8]) -> Verdict {
        if status != 200 {
            return Verdict::Down(Why::Status(status));
        }
        let Ok(text) = std::str::from_utf8(body) else {
            return Verdict::Down(Why::Unreadable);
        };
        // Every readiness answer says this, and nothing else the daemon sends does.
        if !text.contains("\"status\":\"ready\"") {
            return Verdict::Down(Why::Unreadable);
        }
        let report = Report {
            node: string_field(text, "node"),
            shards: string_field(text, "shards"),
            serving: bool_field(text, "serving"),
            wire: string_field(text, "wire")
                .and_then(|s| s.parse().ok())
                .or_else(|| number_field(text, "wire")),
        };
        // Absent means no controller, which means no agreement to have lost touch with.
        match report.serving {
            Some(false) => Verdict::Down(Why::NotServing),
            _ => Verdict::Up(report),
        }
    }
}

fn field_at<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\":");
    let start = text.find(&needle)? + needle.len();
    Some(text[start..].trim_start())
}

fn string_field(text: &str, key: &str) -> Option<String> {
    let rest = field_at(text, key)?;
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn number_field(text: &str, key: &str) -> Option<u32> {
    let rest = field_at(text, key)?;
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn bool_field(text: &str, key: &str) -> Option<bool> {
    let rest = field_at(text, key)?;
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

/// How eagerly a node leaves rotation and how cautiously it comes back.
#[derive(Clone, Copy, Debug)]
pub struct Policy {
    /// Consecutive failures before a node is taken out.
    pub fail: u8,
    /// Consecutive successes before it is put back.
    pub pass: u8,
    /// How long a newly admitted node is protected from being ejected on failure count.
    ///
    /// Without it a node that flaps at exactly the poll frequency oscillates in and out forever;
    /// with it the oscillation damps. It does **not** protect against `serving:false`, which is
    /// the node stating a fact rather than the network being unreliable.
    pub floor: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        // Asymmetric on purpose: leaving is cheap and can be undone, coming back too early is a
        // second outage. Three failures at the default two-second interval is about six seconds.
        Self { fail: 3, pass: 2, floor: Duration::from_secs(6) }
    }
}

/// One node's rotation state.
#[derive(Clone, Debug)]
pub struct Health {
    policy: Policy,
    in_rotation: bool,
    /// Failures in a row while in, or successes in a row while out.
    streak: u8,
    changed_at: Instant,
    last_ok: Option<Instant>,
    why: Option<Why>,
    report: Report,
}

impl Health {
    /// A node starts **in** rotation.
    ///
    /// Starting out would mean a proxy that answers `503` for its first poll interval, which is
    /// an outage it invented. The first probe corrects an optimistic start within one interval;
    /// nothing corrects a pessimistic one any faster.
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            in_rotation: true,
            streak: 0,
            changed_at: Instant::now(),
            last_ok: None,
            why: None,
            report: Report::default(),
        }
    }

    /// A node discovered while this proxy was running starts **out** of rotation.
    ///
    /// The opposite of [`Health::new`], and the argument for that one is what inverts it. An
    /// optimistic start is right at startup because the alternative is answering `503` while
    /// every node is fine - an outage this process invented. A node that appeared mid-flight
    /// invents no outage by waiting: the nodes already in rotation are still serving it. What
    /// it does risk is the other way round - a node is admitted to a cluster *before* it has
    /// caught up, so the one moment membership announces a node is the moment it is least
    /// likely to be ready. It earns its place with `pass` probes like anything else.
    pub fn joining(policy: Policy) -> Self {
        Self { in_rotation: false, why: Some(Why::NotServing), ..Self::new(policy) }
    }

    pub fn in_rotation(&self) -> bool {
        self.in_rotation
    }

    pub fn why(&self) -> Option<Why> {
        self.why
    }

    pub fn report(&self) -> &Report {
        &self.report
    }

    pub fn last_ok(&self) -> Option<Instant> {
        self.last_ok
    }

    pub fn consecutive_failures(&self) -> u8 {
        if self.in_rotation {
            self.streak
        } else {
            0
        }
    }

    /// Fold one probe in. Returns `true` when the rotation changed.
    pub fn observe(&mut self, verdict: Verdict, now: Instant) -> bool {
        match verdict {
            Verdict::Up(report) => {
                self.report = report;
                self.last_ok = Some(now);
                if self.in_rotation {
                    self.streak = 0;
                    self.why = None;
                    return false;
                }
                self.streak = self.streak.saturating_add(1);
                if self.streak >= self.policy.pass {
                    self.in_rotation = true;
                    self.streak = 0;
                    self.why = None;
                    self.changed_at = now;
                    return true;
                }
                false
            }
            // A node that says it is not serving has answered the question. There is nothing to
            // average over and no floor to wait out — sending it work would buy one round trip
            // and a `503`.
            Verdict::Down(Why::NotServing) => {
                self.report.serving = Some(false);
                self.last_ok = Some(now);
                self.eject(Why::NotServing, now)
            }
            Verdict::Down(why) => {
                if !self.in_rotation {
                    self.streak = 0;
                    self.why = Some(why);
                    return false;
                }
                self.streak = self.streak.saturating_add(1);
                if self.streak < self.policy.fail {
                    return false;
                }
                // A node admitted a moment ago keeps its place for one floor, so a node failing
                // at the poll frequency settles instead of oscillating.
                if now.duration_since(self.changed_at) < self.policy.floor {
                    return false;
                }
                self.eject(why, now)
            }
        }
    }

    /// A transport failure seen on real traffic rather than on a probe.
    ///
    /// Worth its own entry point because a node can die between two polls and eat every request
    /// in between. Only failures with no answer count: a `4xx` or `5xx` **is** an answer, and a
    /// node that refused is not a node that is gone.
    pub fn saw_failure(&mut self, now: Instant) -> bool {
        self.observe(Verdict::Down(Why::Unreachable), now)
    }

    fn eject(&mut self, why: Why, now: Instant) -> bool {
        let changed = self.in_rotation;
        self.in_rotation = false;
        self.streak = 0;
        self.why = Some(why);
        if changed {
            self.changed_at = now;
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const READY_CLUSTERED: &str = r#"{"status":"ready","tables":3,"txn_id":9,"pages":8,"node":"a","shards":"0..64","version":"0.1.0","wire":6,"serving":true,"term":2,"leader":"b","behind":[]}"#;
    /// Exactly what the local daemon answered when this was written.
    const READY_SOLO: &str = r#"{"status":"ready","tables":2,"txn_id":793,"pages":8737,"node":"local","shards":"0..","version":"0.1.0","wire":6}"#;
    const READY_NOT_SERVING: &str = r#"{"status":"ready","tables":3,"node":"a","shards":"","version":"0.1.0","wire":6,"serving":false,"term":4,"leader":null,"behind":["a"]}"#;

    fn up() -> Verdict {
        Verdict::of(200, READY_CLUSTERED.as_bytes())
    }

    fn down() -> Verdict {
        Verdict::Down(Why::Unreachable)
    }

    #[test]
    fn a_clustered_node_reports_what_it_serves() {
        let Verdict::Up(r) = up() else { panic!("a serving node is up") };
        assert_eq!(r.node.as_deref(), Some("a"));
        assert_eq!(r.shards.as_deref(), Some("0..64"));
        assert_eq!(r.serving, Some(true));
        assert_eq!(r.wire, Some(6));
    }

    /// The trap: absence is not `false`.
    #[test]
    fn a_solo_node_sends_no_serving_field_and_stays_in_rotation() {
        let Verdict::Up(r) = Verdict::of(200, READY_SOLO.as_bytes()) else {
            panic!("a node with no agreement has no agreement to have lost touch with")
        };
        assert_eq!(r.serving, None, "absent must not be read as false");
        assert_eq!(r.node.as_deref(), Some("local"));
    }

    #[test]
    fn not_serving_is_read_from_the_body_not_the_status() {
        // The daemon answers 200 either way; a poller watching the status alone sees nothing.
        assert!(matches!(
            Verdict::of(200, READY_NOT_SERVING.as_bytes()),
            Verdict::Down(Why::NotServing)
        ));
    }

    #[test]
    fn a_body_that_is_not_a_readiness_answer_is_unreadable() {
        assert!(matches!(Verdict::of(200, b"{}"), Verdict::Down(Why::Unreadable)));
        assert!(matches!(Verdict::of(200, b"not json"), Verdict::Down(Why::Unreadable)));
        assert!(matches!(Verdict::of(200, &[0xff, 0xfe]), Verdict::Down(Why::Unreadable)));
        assert!(matches!(
            Verdict::of(503, READY_CLUSTERED.as_bytes()),
            Verdict::Down(Why::Status(503))
        ));
    }

    #[test]
    fn a_node_starts_in_rotation() {
        assert!(Health::new(Policy::default()).in_rotation());
    }

    #[test]
    fn three_failures_eject_and_two_fewer_do_not() {
        let mut h = Health::new(Policy { floor: Duration::ZERO, ..Policy::default() });
        let t = Instant::now();
        assert!(!h.observe(down(), t));
        assert!(!h.observe(down(), t));
        assert!(h.in_rotation(), "two failures is not enough");
        assert!(h.observe(down(), t), "the third ejects");
        assert!(!h.in_rotation());
        assert_eq!(h.why(), Some(Why::Unreachable));
    }

    #[test]
    fn one_success_resets_the_failure_streak() {
        let mut h = Health::new(Policy { floor: Duration::ZERO, ..Policy::default() });
        let t = Instant::now();
        h.observe(down(), t);
        h.observe(down(), t);
        h.observe(up(), t);
        h.observe(down(), t);
        h.observe(down(), t);
        assert!(h.in_rotation(), "the streak restarted, so two is still not three");
    }

    #[test]
    fn two_successes_readmit_and_one_does_not() {
        let mut h = Health::new(Policy { floor: Duration::ZERO, ..Policy::default() });
        let t = Instant::now();
        for _ in 0..3 {
            h.observe(down(), t);
        }
        assert!(!h.in_rotation());
        assert!(!h.observe(up(), t), "one success is not enough");
        assert!(!h.in_rotation());
        assert!(h.observe(up(), t), "the second readmits");
        assert!(h.in_rotation());
        assert_eq!(h.why(), None);
    }

    /// The node stated a fact. There is nothing to average.
    #[test]
    fn not_serving_ejects_without_waiting_for_a_streak() {
        let mut h = Health::new(Policy::default());
        let t = Instant::now();
        assert!(h.observe(Verdict::of(200, READY_NOT_SERVING.as_bytes()), t));
        assert!(!h.in_rotation());
        assert_eq!(h.why(), Some(Why::NotServing));
    }

    /// ...and it is not held off by the floor either, unlike a transport failure.
    #[test]
    fn the_floor_protects_against_flapping_but_not_against_an_answer() {
        let t = Instant::now();
        let mut h = Health::new(Policy { fail: 1, pass: 1, floor: Duration::from_secs(60) });

        assert!(!h.observe(down(), t), "the floor holds a fresh node in");
        assert!(h.in_rotation());

        assert!(
            h.observe(Verdict::of(200, READY_NOT_SERVING.as_bytes()), t),
            "an answer is not flapping"
        );
        assert!(!h.in_rotation());
    }

    #[test]
    fn the_floor_expires() {
        let t = Instant::now();
        let mut h = Health::new(Policy { fail: 1, pass: 1, floor: Duration::from_millis(1) });
        assert!(h.observe(down(), t + Duration::from_secs(1)), "past the floor it ejects");
        assert!(!h.in_rotation());
    }

    #[test]
    fn traffic_can_eject_a_node_between_polls() {
        let mut h = Health::new(Policy { fail: 2, pass: 2, floor: Duration::ZERO });
        let t = Instant::now();
        assert!(!h.saw_failure(t));
        assert!(h.saw_failure(t), "a node that dies between polls should not eat two seconds");
        assert!(!h.in_rotation());
    }

    #[test]
    fn a_node_already_out_does_not_report_a_second_change() {
        let mut h = Health::new(Policy { fail: 1, pass: 5, floor: Duration::ZERO });
        let t = Instant::now();
        assert!(h.observe(down(), t));
        assert!(!h.observe(down(), t), "already out");
        assert!(!h.observe(down(), t));
    }

    /// A node coming back has to start its success streak from zero each time it fails.
    #[test]
    fn a_failure_while_out_resets_the_readmission_streak() {
        let mut h = Health::new(Policy { fail: 1, pass: 3, floor: Duration::ZERO });
        let t = Instant::now();
        h.observe(down(), t);
        h.observe(up(), t);
        h.observe(up(), t);
        h.observe(down(), t);
        h.observe(up(), t);
        h.observe(up(), t);
        assert!(!h.in_rotation(), "the streak restarted, so two is still not three");
        assert!(h.observe(up(), t));
    }
}
