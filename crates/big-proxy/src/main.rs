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

//! `bigproxy`: one address in front of many nodes.
//!
//! The refusals here are the same shape as `big serve`'s, and for the same reason: a default
//! that quietly does the unsafe thing is worse than an error naming the flag that would allow
//! it. Each one is an error with its own override, so turning it off is a decision somebody
//! made rather than one nobody noticed.

use big_proxy::allowlist::Tier;
use big_proxy::health::Policy;
use big_proxy::listen::{Config, HealthConfig, Proxy};
use big_proxy::pool::Pool;
use big_proxy::upstream::Upstream;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;

const DEFAULT_ADDR: &str = "127.0.0.1:7650";

/// The prose that is about the proxy rather than about a flag.
const AFTER_LONG_HELP: &str = "\
Environment:
  BIG_LOG=off|error|warn|info|debug   log level, default info

Answered here, never forwarded:
  GET /health    liveness of this process, never authenticated
  GET /ready     which upstreams are in rotation and why, never authenticated.
                 503 when none are. This proxy does not fall back to a node that has said
                 it is not serving
  GET /metrics   Prometheus text about this proxy. The nodes' own /metrics are not
                 forwarded: one node's counters chosen at random are not a cluster's
";

/// Everything `bigproxy` was told.
///
/// `allowed` is the one field the command line does not spell directly: `--allow-ops` and
/// `--no-ddl` are two steps on one ladder, so they are parsed as themselves and folded into a
/// [`Tier`] by [`Options::tier`].
#[derive(clap::Parser, Debug)]
#[command(
    name = "bigproxy",
    version,
    about = "One address in front of many nodes",
    // Spelled out rather than left to the doc comment above, which is about the *struct* and
    // would otherwise become what `--help` says this program is for.
    long_about = "\
One address in front of many nodes: health-aware forwarding and a route allowlist.

This proxy checks no credential of its own - it forwards the client's Authorization header and
never reads it - and it links no engine, which is a fact about its dependency graph rather than
a sentence here. It does not route by key: every node is a coordinator.",
    after_long_help = AFTER_LONG_HELP
)]
struct Options {
    /// What to bind.
    #[arg(value_name = "ADDR", default_value = DEFAULT_ADDR)]
    addr: String,

    /// A node to send requests to; repeat for each one.
    ///
    /// The name is not decoration: it is the TLS server name, because a node certificate carries
    /// `subjectAltName = DNS:<node>`.
    // `long = "upstream"`, singular, because that is the flag: the field is plural because it
    // collects. Every deploy file and `scripts/local-cluster` writes `--upstream`.
    #[arg(
        long = "upstream",
        value_name = "NAME=ADDR",
        value_parser = upstream,
        verbatim_doc_comment
    )]
    upstreams: Vec<(String, String)>,

    /// Take name and addr from a cluster.toml instead.
    ///
    /// shards, replica and schema_leader are read and ignored: every node is a coordinator, so
    /// this proxy does not route by key. It also does not refuse a file with a shard gap, because
    /// that is the daemon's problem and a front door that will not start turns one outage into
    /// two. The file is a seed, not the truth: a node admitted after this proxy started is not in
    /// it. See docs/clustering.md.
    #[arg(long, value_name = "FILE", verbatim_doc_comment)]
    cluster: Option<String>,

    /// How often each node is asked GET /ready, in milliseconds.
    #[arg(long, value_name = "MS")]
    health_interval: Option<u64>,

    /// How long a node has to answer it, in milliseconds.
    #[arg(long, value_name = "MS")]
    health_timeout: Option<u64>,

    /// Consecutive failures before a node leaves rotation.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u8).range(1..))]
    health_fail: Option<u8>,

    /// Consecutive successes before it comes back.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u8).range(1..))]
    health_pass: Option<u8>,

    /// Tries on other nodes.
    ///
    /// Never applied to a write whose bytes have already left; see readme.md.
    #[arg(long, value_name = "N", verbatim_doc_comment)]
    max_retries: Option<usize>,

    /// Follow the cluster's membership instead of only the list above.
    ///
    /// Off by default: a proxy that grows an upstream nobody wrote down is one whose shape an
    /// operator cannot predict from what they wrote. On, a node admitted after this process
    /// started becomes reachable without restarting it - and a node removed from the cluster
    /// stops being sent requests. A discovered node starts *out* of rotation and earns its way in
    /// with --health-pass probes, because the moment a cluster announces a node is the moment it
    /// is least likely to have caught up.
    #[arg(long, verbatim_doc_comment)]
    discover: bool,

    /// One `user:password` line, mode 600, for a role holding Operate.
    ///
    /// Reading the membership is `GET /cluster/topology`, which demands it; a cluster with no
    /// users file needs nothing here.
    #[arg(long, value_name = "FILE", verbatim_doc_comment)]
    discover_credentials: Option<String>,

    /// A second listener where upstreams can be seeded while this runs.
    ///
    ///   POST   /admin/upstream?name=a&addr=10.0.0.1:7654
    ///   DELETE /admin/upstream?name=a
    ///
    /// **Loopback only, and refused anywhere else.** This proxy checks no credential of its own -
    /// it forwards the client's and never reads it - so a port that could add an upstream is a
    /// port that could point client traffic at anything. Reaching it has to mean already being on
    /// the machine. With --cluster-id, a seed must answer with that id before it is adopted, so a
    /// local caller cannot point this at another cluster.
    #[arg(long, value_name = "HOST:PORT", verbatim_doc_comment)]
    admin_addr: Option<String>,

    /// What the nodes behind this proxy call their cluster.
    ///
    /// Checked against what a seeded node reports.
    #[arg(long, value_name = "NAME", verbatim_doc_comment)]
    cluster_id: Option<String>,

    /// Also forward /verify, /repair, /cluster/topology and /admin/*.
    ///
    /// Off by default: every one of them asks about one node, and a proxy chooses which node
    /// without telling you.
    #[arg(long, conflicts_with = "no_ddl", verbatim_doc_comment)]
    allow_ops: bool,

    /// Refuse CREATE and DROP as well, leaving reads and writes.
    ///
    /// The daemon's privilege check is the real boundary; this narrows the front door as well.
    /// It and --allow-ops name overlapping ranges of one ladder, so asking for both is asking for
    /// two different answers to one question.
    #[arg(long, verbatim_doc_comment)]
    no_ddl: bool,

    /// Append to the client's X-Forwarded-For rather than replacing it.
    ///
    /// Only correct with a load balancer already in front.
    #[arg(long, verbatim_doc_comment)]
    trust_forwarded_for: bool,

    /// PEM certificate chain this proxy presents to clients.
    ///
    /// Needs a build with the `tls` feature.
    #[arg(long, value_name = "FILE", requires = "tls_key", verbatim_doc_comment)]
    tls_cert: Option<String>,

    /// PEM private key for --tls-cert; file must be mode 600.
    #[arg(long, value_name = "FILE", requires = "tls_cert")]
    tls_key: Option<String>,

    /// Requests handled at once.
    #[arg(long, value_name = "N")]
    workers: Option<usize>,

    /// Connections allowed to wait; past this, 503.
    #[arg(long, value_name = "N")]
    queue: Option<usize>,

    /// How long a client may take to send a request, in seconds.
    #[arg(long, value_name = "SECONDS")]
    read_timeout: Option<u64>,

    /// Allow a non-loopback bind in the clear. Says what it is.
    ///
    /// What you want when something else terminates TLS in front.
    #[arg(long, verbatim_doc_comment)]
    insecure_no_tls: bool,

    /// PEM CA the nodes' certificates must chain to.
    ///
    /// This MAY be the cluster's peer-ca.pem: trusting that CA is not being trusted by it. This
    /// proxy presents no client certificate and so can never reach /internal/*. There is
    /// deliberately no --upstream-cert and no --upstream-key; adding them would be adding
    /// /internal/* access.
    #[arg(long, value_name = "FILE", verbatim_doc_comment)]
    upstream_ca: Option<String>,

    /// Do not verify node certificates. Says so on every run.
    #[arg(long)]
    insecure_skip_verify: bool,
}

impl Options {
    /// Which rung of the allowlist the two forwarding flags name.
    ///
    /// `--allow-ops` and `--no-ddl` are one ladder rather than two switches, which is why they
    /// are declared as themselves and folded here: `conflicts_with` refuses both together, and
    /// this turns whichever was given into the tier it means.
    fn tier(&self) -> Tier {
        match (self.allow_ops, self.no_ddl) {
            (true, _) => Tier::Ops,
            (_, true) => Tier::Data,
            _ => Tier::Ddl,
        }
    }
}

/// `<name>=<addr>`, both halves non-empty.
fn upstream(raw: &str) -> Result<(String, String), String> {
    match raw.split_once('=') {
        Some((name, addr)) if !name.is_empty() && !addr.is_empty() => {
            Ok((name.to_string(), addr.to_string()))
        }
        _ => Err(format!("wants <name=addr>, got {raw:?}")),
    }
}

fn main() {
    let options = <Options as clap::Parser>::parse();
    if let Err(e) = run(options) {
        die(&e);
    }
}

fn die(message: &str) -> ! {
    eprintln!("bigproxy: {message}");
    std::process::exit(2);
}

/// Reads `user:password` from a file and encodes the header the membership read carries.
///
/// The mode check is the one `bigctl` makes of `--credentials-file`, and it is made here for
/// the same reason it is made there: a credential anybody on the machine can read is not a
/// credential. Split on the *first* colon, so a password containing one still works and a file
/// cannot disagree with a header about where a password starts.
fn basic_auth(path: &str) -> Result<String, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .map_err(|e| format!("could not read {path}: {e}"))?
            .permissions()
            .mode()
            & 0o777;
        if mode & 0o077 != 0 {
            return Err(format!(
                "{path} is mode {mode:o}; a credentials file must not be readable by anyone \
                 else (chmod 600 {path})"
            ));
        }
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("could not read {path}: {e}"))?;
    let line = text.lines().next().unwrap_or_default().trim();
    if line.split_once(':').is_none() {
        return Err(format!("{path} must hold one `user:password` line"));
    }
    Ok(format!("Basic {}", big_tls::base64::encode(line.as_bytes())))
}

/// Every node this proxy will send to, from the file, the flags, or both.
///
/// Flags come second so they win: a `--upstream` naming a node already in the file replaces it,
/// which is how an operator points one node somewhere else without editing a file three
/// processes share.
fn upstreams(o: &Options) -> Result<Vec<(String, String)>, String> {
    let mut nodes: Vec<(String, String)> = match &o.cluster {
        Some(path) => big_proxy::config::read_cluster(std::path::Path::new(path))?
            .into_iter()
            .map(|n| (n.name, n.addr))
            .collect(),
        None => Vec::new(),
    };
    for (name, addr) in &o.upstreams {
        match nodes.iter_mut().find(|(n, _)| n == name) {
            Some(existing) => existing.1 = addr.clone(),
            None => nodes.push((name.clone(), addr.clone())),
        }
    }
    Ok(nodes)
}

fn run(o: Options) -> Result<(), String> {
    let nodes = upstreams(&o)?;
    // **Empty is a state, not a misconfiguration - but only when something can fill it.**
    //
    // A proxy whose upstreams are all down answers `503` and keeps running; that is the same
    // state as having none, one entry short. Refusing it at startup while tolerating it at
    // runtime was the inconsistency, and it made "start the front door, bring the nodes up
    // after" impossible for no reason a running proxy could name.
    //
    // Still refused without a way to be filled: a proxy with no upstreams, no discovery and no
    // admin port is a process that will answer `503` until somebody restarts it, and starting
    // one is a mistake worth hearing about at the moment it is made.
    if nodes.is_empty() && !o.discover && o.admin_addr.is_none() {
        return Err("no upstreams, and nothing that could find one. Pass --upstream <name=addr> \
             or --cluster <file>; or --discover, or --admin-addr to seed one while it runs."
            .to_string());
    }

    // A TLS listener needs both halves or neither - one without the other is a typo that would
    // otherwise serve in the clear on a port somebody believed was encrypted. That is now
    // `requires` on both flags, so it is refused before this function is reached; what is left
    // here is the question the parser cannot answer, which is what this *build* can do.
    if o.tls_cert.is_some() && !cfg!(feature = "tls") {
        return Err(
            "this build has no tls. Rebuild with --features tls, or terminate TLS in front"
                .to_string(),
        );
    }

    let policy = Policy {
        fail: o.health_fail.unwrap_or(Policy::default().fail),
        pass: o.health_pass.unwrap_or(Policy::default().pass),
        ..Policy::default()
    };
    let mut health = HealthConfig::default();
    if let Some(ms) = o.health_interval {
        health.every = std::time::Duration::from_millis(ms.max(50));
    }
    if let Some(ms) = o.health_timeout {
        health.budget = std::time::Duration::from_millis(ms.max(50));
    }

    // `peer_ca: None` takes rustls' `with_no_client_auth()` branch, so this listener never asks
    // for and never accepts a client certificate. It is a front door for people.
    let listener_tls = match (&o.tls_cert, &o.tls_key) {
        (Some(cert), Some(key)) => Some(Arc::new(
            big_tls::TlsConfig::load(
                std::path::Path::new(cert),
                std::path::Path::new(key),
                None,
                Vec::new(),
            )
            .map_err(|e| format!("cannot load the certificate: {e}"))?,
        )),
        _ => None,
    };

    let mut config = Config {
        allowed: o.tier(),
        trust_forwarded_for: o.trust_forwarded_for,
        health,
        proto: if o.tls_cert.is_some() { "https" } else { "http" },
        tls: listener_tls,
        ..Default::default()
    };
    if let Some(n) = o.workers {
        config.workers = n.max(1);
    }
    if let Some(n) = o.queue {
        config.queue_depth = n.max(1);
    }
    if let Some(s) = o.read_timeout {
        config.read_timeout = std::time::Duration::from_secs(s);
    }

    // **`identity: None`, always.** This proxy presents no client certificate, which is what
    // leaves it as `Identity::None` at a node and so refused by every `Guard::Node` route.
    // `--upstream-ca` may well point at the cluster's own peer-ca.pem: the CA *certificate* is
    // public — certs.sh writes it mode 644 and says so — and trusting a CA is not being trusted
    // by it. What would open /internal/* is a certificate signed by that CA and naming a node,
    // and there is deliberately no flag that loads one.
    let upstream_tls = match (&o.upstream_ca, o.insecure_skip_verify) {
        (_, true) => {
            Some(Arc::new(big_tls::ClientTls::insecure(None).map_err(|e| format!("tls: {e}"))?))
        }
        (Some(ca), _) => Some(Arc::new(
            big_tls::ClientTls::new(Some(std::path::Path::new(ca)), None)
                .map_err(|e| format!("cannot read {ca}: {e}"))?,
        )),
        (None, _) => None,
    };

    let ups: Vec<Upstream> = nodes
        .iter()
        .map(|(name, addr)| match &upstream_tls {
            Some(tls) => Upstream::secured(name, addr, Arc::clone(tls)),
            None => Upstream::new(name, addr),
        })
        .collect();
    if o.discover {
        // Read before the listener binds, so a credentials file that cannot be read is a
        // startup failure rather than a warning every two seconds afterwards.
        let authorization = match &o.discover_credentials {
            Some(path) => Some(basic_auth(path)?),
            None => None,
        };
        config.discovery = Some(big_proxy::discover::Discovery {
            every: health.every,
            budget: health.budget,
            authorization,
            tls: upstream_tls.clone(),
            policy,
        });
    } else if o.discover_credentials.is_some() {
        return Err("--discover-credentials does nothing without --discover".to_string());
    }

    if let Some(addr) = &o.admin_addr {
        // Bound before the public listener, so a refused address is a startup failure rather
        // than a port that is open with no way to fill it.
        config.admin = Some(
            big_proxy::admin::Admin::bind(
                addr,
                policy,
                upstream_tls.clone(),
                o.cluster_id.clone(),
                health.budget,
            )
            .map_err(|e| e.to_string())?,
        );
    } else if o.cluster_id.is_some() {
        return Err("--cluster-id is checked when an upstream is seeded, which needs --admin-addr"
            .to_string());
    }

    let pool = Pool::new(ups, policy, o.max_retries.unwrap_or(2));

    let proxy = Proxy::bind(o.addr.as_str(), pool, config)
        .map_err(|e| format!("cannot listen on {}: {e}", o.addr))?;
    let bound = proxy.local_addr().map_err(|e| format!("cannot read the bound address: {e}"))?;

    // **Checked after binding, on the resolved address.** `0.0.0.0:7650` and a hostname that
    // resolves off-box are the same decision, and only the resolved address shows it.
    refuse_an_open_port(bound, o.tls_cert.is_some(), o.insecure_no_tls)?;
    refuse_a_clear_upstream(&nodes, &o)?;
    announce(bound, &o, &nodes);

    proxy.serve().map_err(|e| format!("the listener stopped: {e}"))
}

/// Refuse to carry a password in the clear on a leg this process chose.
///
/// The daemon has no equivalent because it is the thing being connected *to*. This proxy picks
/// the outbound leg, so a password in the clear on it is this process's doing.
fn refuse_a_clear_upstream(nodes: &[(String, String)], o: &Options) -> Result<(), String> {
    if o.upstream_ca.is_some() || o.insecure_skip_verify {
        return Ok(());
    }
    let off_box: Vec<&str> = nodes
        .iter()
        .filter(|(_, addr)| !resolves_to_loopback(addr))
        .map(|(n, _)| n.as_str())
        .collect();
    if off_box.is_empty() {
        return Ok(());
    }
    Err(format!(
        "refusing to reach {} in the clear.\n\n\
         Every request this proxy forwards carries the client's Authorization header, and a\n\
         node on another machine means that header crosses a network.\n\n\
         Pass --upstream-ca <file> so the nodes' certificates can be verified, or\n\
         --insecure-skip-verify if the network really is private.",
        off_box.join(", ")
    ))
}

/// Whether every address this name resolves to is on this machine.
///
/// A name that resolves to nothing is treated as off-box: it is the answer that refuses rather
/// than the one that quietly allows, and a node that cannot be resolved at startup is worth
/// hearing about anyway.
fn resolves_to_loopback(addr: &str) -> bool {
    match addr.to_socket_addrs() {
        Ok(mut resolved) => {
            let mut any = false;
            let all = resolved.all(|a| {
                any = true;
                loopback(a.ip())
            });
            any && all
        }
        Err(_) => false,
    }
}

/// Refuse to carry passwords in the clear off this machine.
fn refuse_an_open_port(bound: SocketAddr, tls: bool, insecure_no_tls: bool) -> Result<(), String> {
    if loopback(bound.ip()) || tls || insecure_no_tls {
        return Ok(());
    }
    Err(format!(
        "refusing to serve {bound} in the clear.\n\n\
         Every request through this proxy carries an Authorization: Basic header, which is a\n\
         password in base64. A password is one a person also uses somewhere else, so sending\n\
         one in the clear risks something that was never ours to risk.\n\n\
         Either pass --tls-cert and --tls-key, or bind to loopback and terminate TLS in\n\
         front, or pass --insecure-no-tls if the port really is private."
    ))
}

fn loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

/// Print what this process decided, every time, whichever way it went.
///
/// A line that only appears sometimes is one an operator has to remember the absence of.
fn announce(bound: SocketAddr, o: &Options, nodes: &[(String, String)]) {
    let scheme = if o.tls_cert.is_some() { "https" } else { "http" };
    println!("bigproxy listening on {scheme}://{bound}");
    println!(
        "bigproxy: {}",
        if cfg!(feature = "tls") { "tls built in" } else { "no tls in this build" }
    );

    let listed: Vec<String> = nodes.iter().map(|(n, a)| format!("{n}={a}")).collect();
    let source = match &o.cluster {
        Some(path) if o.upstreams.is_empty() => format!(" from {path}"),
        Some(path) => format!(" from {path} and {} flags", o.upstreams.len()),
        None => String::new(),
    };
    println!("bigproxy: {} upstreams{source}: {}", nodes.len(), listed.join(" "));

    // The sentence that has to be true, printed where it can be checked against the flags.
    match (&o.upstream_ca, o.insecure_skip_verify) {
        (_, true) => {
            println!("bigproxy: upstream certificates are NOT verified (--insecure-skip-verify)")
        }
        (Some(ca), _) => println!(
            "bigproxy: upstream transport tls, CA {ca}, no client certificate — \
             /internal/* is unreachable through this proxy"
        ),
        (None, _) => println!(
            "bigproxy: upstream transport plaintext, no client certificate — \
             /internal/* is unreachable through this proxy"
        ),
    }

    println!(
        "bigproxy: health every {}ms, {}ms budget, out after {} failures, back after {} successes",
        o.health_interval.unwrap_or(2000),
        o.health_timeout.unwrap_or(1000),
        o.health_fail.unwrap_or(Policy::default().fail),
        o.health_pass.unwrap_or(Policy::default().pass),
    );
    // Said either way. An operator reading a log to find out why a new node is getting no
    // traffic should not have to remember whether they passed the flag.
    if let Some(addr) = &o.admin_addr {
        println!(
            "bigproxy: seeding port on http://{addr} — loopback only, and it checks no \
             credential; reaching it means being on this machine"
        );
    }
    match (o.discover, o.discover_credentials.is_some()) {
        (true, true) => println!(
            "bigproxy: following the cluster's membership every {}ms, as the credentials file says",
            o.health_interval.unwrap_or(2000)
        ),
        (true, false) => println!(
            "bigproxy: following the cluster's membership every {}ms, unauthenticated — a \
             cluster with a users file needs --discover-credentials",
            o.health_interval.unwrap_or(2000)
        ),
        (false, _) => println!(
            "bigproxy: upstreams are the {} given here and nothing else; --discover follows the \
             cluster's membership instead",
            nodes.len()
        ),
    }
    println!(
        "bigproxy: set --query-timeout at or above the daemon's, or this proxy gives up while \
         a node is still working"
    );

    let forwarded = big_proxy::allowlist::ROUTES.iter().filter(|r| r.tier <= o.tier()).count();
    println!(
        "bigproxy: {forwarded} of {} routes forwarded; /internal/* is not one of them",
        big_proxy::allowlist::ROUTES.len()
    );
    if !loopback(bound.ip()) && o.tls_cert.is_none() {
        println!("bigproxy: serving in the clear; terminate TLS in front of this port");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(args: &[&str]) -> Result<Options, clap::Error> {
        <Options as clap::Parser>::try_parse_from(
            std::iter::once("bigproxy").chain(args.iter().copied()),
        )
    }

    #[test]
    fn the_address_is_positional_and_optional() {
        assert_eq!(opts(&["--upstream", "a=x:1"]).unwrap().addr, DEFAULT_ADDR);
        assert_eq!(opts(&["0.0.0.0:9", "--upstream", "a=x:1"]).unwrap().addr, "0.0.0.0:9");
    }

    #[test]
    fn an_upstream_needs_a_name_because_the_name_is_the_server_name() {
        assert!(opts(&["--upstream", "127.0.0.1:7654"]).is_err());
        assert!(opts(&["--upstream", "=127.0.0.1:7654"]).is_err());
        assert!(opts(&["--upstream", "a="]).is_err());
        assert_eq!(opts(&["--upstream", "a=x:1"]).unwrap().upstreams, [("a".into(), "x:1".into())]);
    }

    #[test]
    fn the_tier_ladder_defaults_to_reads_writes_and_ddl() {
        assert_eq!(opts(&["--upstream", "a=x:1"]).unwrap().tier(), Tier::Ddl);
        assert_eq!(opts(&["--upstream", "a=x:1", "--no-ddl"]).unwrap().tier(), Tier::Data);
        assert_eq!(opts(&["--upstream", "a=x:1", "--allow-ops"]).unwrap().tier(), Tier::Ops);
    }

    #[test]
    fn asking_to_widen_and_narrow_at_once_is_refused() {
        assert!(opts(&["--upstream", "a=x:1", "--no-ddl", "--allow-ops"]).is_err());
    }

    #[test]
    fn a_flag_missing_its_value_names_the_flag() {
        let e = opts(&["--upstream"]).unwrap_err().to_string();
        assert!(e.contains("--upstream"), "{e}");
    }

    /// **The wording is the parser's now; naming the flag and offering the usage is not.**
    /// A refusal that does not say which word was wrong is a refusal somebody has to guess at.
    #[test]
    fn an_unknown_flag_names_it_and_points_at_the_usage() {
        let e = opts(&["--turbo"]).unwrap_err().to_string();
        assert!(e.contains("--turbo") && e.contains("--help"), "{e}");
    }

    /// A cert without its key would otherwise serve in the clear on a port somebody believed
    /// was encrypted. `requires` refuses it before `run` is reached.
    #[test]
    fn half_a_tls_listener_is_refused() {
        assert!(opts(&["--upstream", "a=x:1", "--tls-cert", "c"]).is_err());
        assert!(opts(&["--upstream", "a=x:1", "--tls-key", "k"]).is_err());
        assert!(opts(&["--upstream", "a=x:1", "--tls-cert", "c", "--tls-key", "k"]).is_ok());
    }

    #[test]
    fn loopback_may_serve_in_the_clear_and_anything_else_may_not() {
        let local: SocketAddr = "127.0.0.1:7650".parse().unwrap();
        let open: SocketAddr = "10.0.0.1:7650".parse().unwrap();
        assert!(refuse_an_open_port(local, false, false).is_ok());
        assert!(refuse_an_open_port(open, false, false).is_err());
        assert!(refuse_an_open_port(open, false, true).is_ok(), "the override must work");
        assert!(refuse_an_open_port(open, true, false).is_ok(), "tls is the real answer");
    }

    /// The refusal has to say which flag allows it, or it is a dead end rather than a decision.
    #[test]
    fn the_refusal_names_its_override() {
        let open: SocketAddr = "10.0.0.1:7650".parse().unwrap();
        let e = refuse_an_open_port(open, false, false).unwrap_err();
        assert!(e.contains("--insecure-no-tls"), "{e}");
    }
}
