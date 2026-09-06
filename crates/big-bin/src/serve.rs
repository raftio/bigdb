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

//! `big serve <file> [addr] [options]` - serve one database over HTTP.
//!
//! **Binding off loopback without a users file is refused**, and so is binding off loopback in
//! the clear. Both used to print a warning and bind anyway, which is a warning nobody reads on a
//! port anybody can reach. A warning is the right shape for something recoverable; neither of
//! these is, so each is an error with its own flag to override it deliberately.
//!
//! **TLS is here now, and this paragraph used to say it never would be.** The old answer -
//! terminate at a reverse proxy - was the right one while the credential was a bearer token that
//! belonged to this database and to nothing else. A password is not that: it is a thing a person
//! also uses somewhere else, so sending one in the clear risks something that was never ours to
//! risk. The proxy is still supported and is still a perfectly good deployment; it is just no
//! longer the only answer. See `--tls-cert` and `--insecure-no-tls`.

use big_cluster::{Cluster, ClusterFile};
use big_embed::Api;
use big_http::{log, Auth, Server, ServerConfig};
use big_tls::TlsConfig;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const DEFAULT_ADDR: &str = "127.0.0.1:7654";

/// The prose that is about the *server* rather than about a flag.
///
/// It used to be the tail of one hand-written usage string. It is still hand-written - these are
/// routes and their privileges, which no argument parser can generate - but it is now attached to
/// `--help` rather than being the thing `--help` printed.
const AFTER_LONG_HELP: &str = "\
Environment:
  BIG_LOG=off|error|warn|info|debug   log level, default info

Listing:
  GET /table/{t}/records?after=<id>&limit=<n>   records in order, a page at a time
                              `next` in the response is the id to send back as `after`

Reading your own write:
  every write answers with X-Big-Txn: <node>/<transaction>
  ?min_txn=<node>/<transaction>   do not answer until this node is at least there
  ?wait=<ms>                      how long that may take, default 1000, capped at 60000
                              a transaction from another node is 409, not a wait: an id
                              means nothing outside the file that issued it

Replication:
  GET  /verify   do the copies of every range still hold the same facts
                 a scan, not a probe; needs OPERATE on *.*
  POST /repair   catch up every copy the agreement has marked behind
                 a scan and a copy; needs OPERATE on *.*

Backup:
  POST /admin/backup?name=<f>  an online, compact copy of this node's file
                 needs --backup-dir and OPERATE on *.*; one at a time
                 a cluster is backed up one node at a time, and the copies
                 are not one snapshot - see docs/clustering.md

Probes and metrics:
  GET /health    liveness, never authenticated
  GET /ready     readiness, never authenticated
  GET /metrics   Prometheus text; needs OPERATE on *.* when --users is configured

The operational routes above are one server-wide privilege rather than a role that also
happened to read tables - GRANT OPERATE ON *.* TO <role>. So is everything under
/admin/cluster. See docs/access-control.md.
";

/// `big serve`, already parsed by the dispatcher.
pub fn main(opts: Options) -> std::io::Result<()> {
    let opts = match opts.normalize() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("big serve: {e}");
            std::process::exit(2);
        }
    };

    // The cluster file is read here, **before anything is bound and before the certificate is
    // loaded**, because it holds two things the listener needs: which CA signs a peer's
    // certificate, and the names those certificates are allowed to claim. A file that disagrees
    // with itself is a fact the operator has to fix, and finding that out after the port is open
    // would mean a node answering for a range nobody agreed it owns.
    let cluster_file = read_cluster_file(&opts)?;
    let auth = authenticator(&opts)?;
    let tls = transport(&opts, cluster_file.as_ref())?;

    // One process per file: the engine takes an exclusive lock, so a second daemon on the same
    // path fails here rather than fighting over it later.
    let api = Api::open_sized(&opts.path, opts.mapsize)
        .map_err(|e| std::io::Error::other(format!("could not open {}: {e}", opts.path)))?;

    api.set_key_limit(opts.max_row_keys);

    let cluster = assemble(api, &opts, cluster_file)?;
    if let Some(d) = opts.durability {
        cluster
            .local()
            .set_durability(d)
            .map_err(|e| std::io::Error::other(format!("could not set durability: {e}")))?;
    }
    cluster.local().configure_group_commit(big_embed::GroupConfig {
        enabled: opts.write_coalesce,
        max_jobs: opts.write_group_jobs,
        max_facts: opts.write_group_facts,
        async_writes: opts.write_async,
        max_async_bytes: opts.write_queue_bytes,
        linger: opts.write_linger,
        when_full: opts.write_when_full,
    });

    let server = Server::bind_cluster(cluster, opts.addr.as_str(), serving(auth, tls, &opts))?;
    let bound = server.local_addr()?;
    refuse_an_open_port(&server, bound, &opts);
    announce(&server, bound, &opts);

    // **Stopped by a signal, not killed by one.** `SIGTERM` is what every supervisor sends
    // first - `docker stop`, a pod being evicted, `systemctl stop` - and the default disposition
    // ends the process mid-request: the workers die with sockets open, and the agreement learns
    // this node has gone only when its heartbeats stop. Standing down instead finishes what is
    // in flight, takes nothing more, and leaves the agreement before the port closes.
    //
    // Two things this does *not* promise, said here rather than found out. The agreement is
    // stopped before the workers drain, so a request that arrives during the drain for a range
    // this node was serving gets `503 not_serving` - correct, because a node standing down must
    // not answer for a range it may already have lost, but not the same as "every request
    // completes". And a move this node was driving as the agreement's leader is not finished
    // for it: the range stays marked moving until an operator cancels or another leader
    // finishes it. A signal narrows that window; it does not close it.
    server.serve_while(stand_down_on_signal())
}

/// Installs the handlers that ask the server to stand down, and hands back the flag they clear.
///
/// **One of the two `unsafe` blocks in this crate** - the other is `tty::Echo` - and the whole
/// of it is two `libc::signal` calls. The handler does exactly one thing: a relaxed store into a
/// static `AtomicBool`, which is on the short list of what a signal handler may do. Nothing is
/// allocated, locked, or printed from it; the accept loop notices the flag on its next poll.
#[allow(unsafe_code)]
fn stand_down_on_signal() -> &'static AtomicBool {
    static RUNNING: AtomicBool = AtomicBool::new(true);
    extern "C" fn stand_down(_: libc::c_int) {
        RUNNING.store(false, Ordering::Relaxed);
    }
    // SAFETY: `stand_down` is an `extern "C"` function with the signature `signal` expects, it
    // touches only a `static AtomicBool` through an atomic store (async-signal-safe), and the
    // static lives for the whole program, so the handler can never run against freed memory.
    // `signal` itself has no memory-safety preconditions beyond a valid handler pointer.
    unsafe {
        libc::signal(libc::SIGTERM, stand_down as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, stand_down as *const () as libc::sighandler_t);
    }
    &RUNNING
}

/// The certificate this listener presents, or the decision to serve in the clear.
///
/// Read **before the listener is bound**, like the cluster file and for the same reason: a
/// certificate that cannot be loaded is a fact the operator has to fix, and finding it out after
/// the port is open would mean a port that accepts connections it can never complete.
fn transport(opts: &Options, cluster: Option<&ClusterFile>) -> std::io::Result<Option<TlsConfig>> {
    match (&opts.tls_cert, &opts.tls_key) {
        // The half-configured cases are refused by `Options::parse`, which is where a flag that
        // needs another flag belongs: that path exits 2 and reprints the usage, and this one
        // does neither.
        (None, _) | (_, None) => Ok(None),
        (Some(cert), Some(key)) => {
            // **The cluster file is the roster.** A client certificate this node's peer CA signed
            // that names none of these is refused at accept: a certificate that got that far was
            // meant to be a node, and letting it through as an anonymous client would hide a
            // renamed node behind a `401` nobody can explain.
            let peer_ca = cluster.and_then(ClusterFile::peer_ca_file);
            // The CRL travels with the CA it belongs to, and only means anything beside one.
            let peer_crl = cluster.and_then(ClusterFile::peer_crl_file);
            let roster: Vec<String> = cluster
                .map(|c| c.nodes().iter().map(|n| n.name.clone()).collect())
                .unwrap_or_default();
            if peer_ca.is_some() && roster.is_empty() {
                return Err(std::io::Error::other(
                    "a peer CA is configured but the cluster file names no nodes",
                ));
            }
            let tls = TlsConfig::load(
                Path::new(cert),
                Path::new(key),
                peer_ca.map(Path::new),
                peer_crl.map(Path::new),
                roster,
            )
            .map_err(|e| std::io::Error::new(e.kind(), format!("could not load {cert}: {e}")))?;
            eprintln!("big: tls certificate from {cert}");
            if let Some(crl) = peer_crl {
                eprintln!("big: peer certificates checked against the revocation list in {crl}");
            }
            if tls.checks_peers() {
                eprintln!("big: peers are checked against the names in the cluster file");
            }
            Ok(Some(tls))
        }
    }
}

/// The users, or the decision to have none.
fn authenticator(opts: &Options) -> std::io::Result<Auth> {
    let Some(path) = &opts.users else { return Ok(Auth::disabled()) };
    let auth = Auth::from_file(path)
        .map_err(|e| std::io::Error::new(e.kind(), format!("could not read {path}: {e}")))?;
    eprintln!("big: {} users loaded from {path}", auth.len());
    Ok(auth)
}

/// The coordinator this process runs, clustered or not.
///
/// The cluster file is read here, **before the listener is bound**. A file that disagrees with
/// itself is a fact the operator has to fix, and finding that out after the port is open would
/// mean a node answering for a range nobody agreed it owns.
/// The cluster file, parsed once and used twice: by the listener, for the peer CA and the
/// roster, and by the coordinator, for who owns what.
fn read_cluster_file(opts: &Options) -> std::io::Result<Option<ClusterFile>> {
    let Some(path) = &opts.cluster else { return Ok(None) };
    let text = std::fs::read_to_string(path)
        .map_err(|e| std::io::Error::new(e.kind(), format!("could not read {path}: {e}")))?;
    ClusterFile::parse(&text).map(Some).map_err(|e| std::io::Error::other(format!("{path}: {e}")))
}

/// How long one attempt at the joining request may take, and how long the whole thing may.
///
/// A peer that is mid-election answers nothing for a moment, so a single attempt would make
/// startup a coin toss. A cap rather than forever, because the common failure here is a typo in
/// an address, and a daemon that retries a typo until somebody notices is worse than one that
/// exits saying what it could not reach.
const JOIN_ATTEMPT: Duration = Duration::from_secs(5);
const JOIN_LIMIT: Duration = Duration::from_secs(60);

/// Builds this node's configuration from a node already in the cluster.
///
/// Three things have to be given rather than discovered, and each has a reason it cannot be
/// asked for: the id, because the request carries a stamp derived from it; the name, because
/// the answer is a list this node has to find itself in; and the CA, because trusting a peer is
/// what makes its answer worth reading at all.
fn join_cluster(opts: &Options, addr: &str) -> std::io::Result<big_cluster::ClusterConfig> {
    let want = |flag: &str, why: &str| std::io::Error::other(format!("--join needs {flag}: {why}"));
    let id = opts.cluster_id.as_deref().ok_or_else(|| {
        want("--cluster-id", "the request that asks who is in the cluster is stamped with it")
    })?;
    let me = opts.node.as_deref().ok_or_else(|| {
        want("--node", "the answer is a list of nodes, and this one has to know which it is")
    })?;
    if opts.cluster.is_some() {
        return Err(std::io::Error::other(
            "--join and --cluster are two answers to one question: either this node is told the              cluster or it asks for it. Pass one",
        ));
    }

    let tls = match (&opts.peer_ca, &opts.peer_cert, &opts.peer_key) {
        (Some(ca), Some(cert), Some(key)) => Some(std::sync::Arc::new(
            big_tls::ClientTls::new(Some(Path::new(ca)), Some((Path::new(cert), Path::new(key))))
                .map_err(|e| std::io::Error::new(e.kind(), format!("--peer-ca: {e}")))?,
        )),
        (Some(_), _, _) => {
            return Err(std::io::Error::other(
                "--peer-ca names a CA, so this node needs --peer-cert and --peer-key as well: a                  peer that asks for a certificate refuses a node that presents none",
            ))
        }
        _ => None,
    };

    // The name is what a peer certificate must claim, and this node's certificate names it.
    let peer = big_cluster::Peer::new(me, addr, tls, big_cluster::config::fingerprint_of(id));
    let started = std::time::Instant::now();
    let mut wait = Duration::from_millis(200);
    let body = loop {
        match peer.post(
            big_cluster::path::JOIN,
            &[],
            Some(JOIN_ATTEMPT),
            big_cluster::client::Repeatable::Yes,
        ) {
            Ok(r) if r.is_ok() => break r.body,
            // A refusal is an answer: the cluster is there and has said no. Retrying a `409`
            // for a minute would turn "these are different clusters" into a slow hang.
            Ok(r) => {
                return Err(std::io::Error::other(format!(
                    "{addr} refused the joining request: {} {}",
                    r.status,
                    r.message().unwrap_or_else(|| "no message".to_string())
                )))
            }
            Err(e) if started.elapsed() + wait < JOIN_LIMIT => {
                eprintln!("big: {addr} did not answer ({e}); trying again in {wait:?}");
                std::thread::sleep(wait);
                wait = (wait * 2).min(Duration::from_secs(5));
            }
            Err(e) => {
                return Err(std::io::Error::other(format!(
                    "could not reach {addr} to join in {JOIN_LIMIT:?}: {e}"
                )))
            }
        }
    };

    let (id, members) = big_cluster::wire::get_join(&body)
        .map_err(|e| std::io::Error::other(format!("{addr} answered something unreadable: {e}")))?;
    big_cluster::ClusterConfig::joining(&id, &members, me)
        .map_err(|e| std::io::Error::other(format!("{addr}: {e}")))
}

fn assemble(
    api: Api<big_embed::MmapPager>,
    opts: &Options,
    file: Option<ClusterFile>,
) -> std::io::Result<Cluster<big_embed::MmapPager>> {
    let (path, peer_ca, config) = match (&opts.join, &opts.cluster, file) {
        // Dialled rather than read. What comes back is the same kind of configuration a file
        // produces, so everything below this line is the path a clustered node has always taken.
        (Some(addr), _, _) => {
            let config = join_cluster(opts, addr)?;
            (format!("--join {addr}"), opts.peer_ca.clone(), config)
        }
        (None, Some(path), Some(file)) => {
            let peer_ca = file.peer_ca_file().map(str::to_string);
            let config = file
                .for_node(opts.node.as_deref(), &opts.addr)
                .map_err(|e| std::io::Error::other(format!("{path}: {e}")))?;
            (path.clone(), peer_ca, config)
        }
        _ => return Ok(Cluster::solo(api, &opts.addr)),
    };
    let path = &path;
    eprintln!(
        "big: node `{}` owns shards {}, schema leader is `{}`, {} peers",
        config.this().name,
        config.this().shards,
        config.leader().name,
        config.nodes().len() - 1
    );

    let clustered = config.nodes().len() > 1;
    let tls = match (&peer_ca, &opts.peer_cert, &opts.peer_key) {
        (None, _, _) => {
            if clustered {
                // Not refused: a private network with no credentials anywhere is a configuration
                // this daemon already allowed, and refusing it here would refuse it only for
                // clusters. Said out loud, because a peer that presents nothing can only talk to
                // a peer that asks for nothing.
                eprintln!(
                    "big: no peer_ca_file in {path}; this node presents no credential to its peers"
                );
            }
            None
        }
        // **Refused, where the missing peer token used to be a warning.** Under a shared token, a
        // node with none could still be reached by peers that asked for none. Under mutual TLS a
        // node with no client certificate cannot reach `/internal/*` on any peer at all, so every
        // fan-out would return 403 and the cluster would be silently broken - which is worse than
        // the old case, and so it is an error rather than a line in a log.
        (Some(_), None, _) | (Some(_), _, None) if clustered => {
            return Err(std::io::Error::other(format!(
                "{path} names a peer CA, so this node needs --peer-cert and --peer-key to \
                 reach its peers. Without them every request to another node is refused and \
                 the cluster is broken in a way that only shows up under load."
            )))
        }
        (Some(ca), Some(cert), Some(key)) => {
            let tls = big_tls::ClientTls::new(
                Some(Path::new(ca)),
                Some((Path::new(cert), Path::new(key))),
            )
            .map_err(|e| std::io::Error::new(e.kind(), format!("{path}: {e}")))?;
            // Printed rather than folded into the cluster fingerprint. Two nodes trusting
            // different CAs cannot exchange the header that carries a fingerprint - the handshake
            // fails first - so a `409` for it would be unreachable. This is what lets an operator
            // compare two nodes without provoking a handshake to find out.
            eprintln!("big: peer CA {ca}");
            Some(std::sync::Arc::new(tls))
        }
        // A solo node with a peer CA configured and no certificate of its own. Nothing to reach,
        // so nothing to refuse.
        (Some(_), _, _) => None,
    };

    // Next to the database, because it belongs to this node and to this file: two daemons on one
    // machine are two databases, and giving them one vote between them would be giving one node
    // two.
    let state = format!("{}.raft", opts.path);
    eprintln!("big: agreement state in {state}");
    let leases = big_cluster::controller::Leases {
        move_schema_after: opts
            .elect_schema_leader
            .then_some(big_cluster::controller::Leases::SCHEMA_FAILOVER),
        ..big_cluster::controller::Leases::default()
    };
    Cluster::with_timing(
        api,
        config,
        tls,
        Box::new(big_cluster::raft::FileStore::new(state)),
        big_cluster::raft::Timing::default(),
        leases,
    )
}

/// The listener's settings: the defaults, with whatever the operator overrode.
fn serving(auth: Auth, tls: Option<TlsConfig>, opts: &Options) -> ServerConfig {
    let base = ServerConfig::default();
    ServerConfig {
        auth,
        tls,
        backup_dir: opts.backup_dir.clone(),
        watch_max: opts.watch_max,
        query_timeout: opts.query_timeout,
        workers: opts.workers.unwrap_or(base.workers),
        queue_depth: opts.queue.unwrap_or(base.queue_depth),
        read_timeout: opts.read_timeout.unwrap_or(base.read_timeout),
        // Only the switch. The thresholds stay at what the policy ships with until somebody
        // has a reason to move them; three knobs would triple what has to be tested for a
        // setting nobody has yet needed to change.
        balance: big_cluster::balance::Policy { enabled: opts.balance, ..base.balance },
        reclaim: opts.reclaim,
        ..base
    }
}

/// Refuses a public port with no authentication, which is the one thing this daemon will not do.
///
/// Checked **after** binding, because the address that matters is the one actually bound: an
/// operator who wrote `0.0.0.0:7654` and one who wrote a hostname that resolves off-box have
/// made the same decision, and only the resolved address shows it.
fn refuse_an_open_port(
    server: &Server<big_embed::MmapPager>,
    bound: std::net::SocketAddr,
    opts: &Options,
) {
    if bound.ip().is_loopback() {
        return;
    }
    if !server.config().auth.is_enabled() && !opts.insecure {
        eprintln!(
            "big: refusing to serve {bound} with no authentication.\n\
             \n\
             Anyone who can reach this port can read and delete everything in the database.{}\n\
             Either pass --users <file>, or bind to loopback and put a reverse proxy in\n\
             front of it, or pass --insecure-no-auth if the port really is private.",
            peer_routes_warning(opts)
        );
        std::process::exit(2);
    }
    // **A second refusal, and a second flag, because these are two decisions.** An operator with
    // a TLS-terminating proxy in front wants exactly `--insecure-no-tls` and wants to keep their
    // credentials; folding the two into one flag would make them buy the second with the first.
    if server.config().tls.is_none() && !opts.insecure_no_tls {
        eprintln!(
            "big: refusing to serve {bound} in the clear.\n\
             \n\
             A password sent in the clear is worse than a bearer token sent in the clear.\n\
             A token belongs to this database; a password is one a person also uses\n\
             somewhere else.\n\
             \n\
             Either pass --tls-cert and --tls-key, or bind to loopback and terminate TLS\n\
             at a reverse proxy, or pass --insecure-no-tls if the port really is private."
        );
        std::process::exit(2);
    }
}

/// The extra sentence a node in a cluster has earned, and a lone daemon has not.
///
/// **Because the two are not the same offer.** With no users file, `Guard::Node` is satisfied
/// by anything - "auth off means allow all" is the contract, and it extends to the peer routes
/// like it extends to everything else. On one daemon that is what the line above already says:
/// the data is readable and deletable. On a node with peers it is more than that. `/internal/*`
/// carries the agreement itself and the storage underneath it - raft messages, row-key
/// assignments, whole fragments written byte for byte - none of which the SQL surface would
/// have let through, and none of which "read and delete everything in the database" describes
/// to somebody deciding whether their network is private enough.
fn peer_routes_warning(opts: &Options) -> &'static str {
    if opts.cluster.is_none() {
        return "";
    }
    "\n\
     This node has peers, so it also serves /internal/*: the agreement's own\n\
     messages, row-key assignments, and whole fragments written straight to disk.\n\
     Those are normally reachable only by a node holding a peer certificate. With\n\
     no users file they are reachable by anyone who can reach the port, and they\n\
     bypass every check the SQL surface makes.\n"
}

/// What a starting daemon says about itself.
///
/// The durability line is printed on every start, not only when it is relaxed. An operator
/// reading a log after an incident needs to know what the setting *was*, and a line that only
/// appears sometimes is one they have to remember the absence of.
fn announce(server: &Server<big_embed::MmapPager>, bound: std::net::SocketAddr, opts: &Options) {
    let scheme = if server.config().tls.is_some() { "https" } else { "http" };
    eprintln!("big serving {} on {scheme}://{bound}", opts.path);
    // Printed on every start, whichever it is. Which binary an operator was running is a thing
    // they have to be able to read out of a log after the fact, and a line that only appears
    // sometimes is one they have to remember the absence of.
    if big_tls::built_with_tls() {
        eprintln!("big: tls built in");
    } else {
        eprintln!("big: no tls in this build");
    }
    eprintln!("big: durability {}", server.api().durability().label());
    // Printed on every start for the same reason durability is: the ceiling that made a write
    // fail is one an operator has to be able to read out of a log after the fact, and a line
    // that only appears when the flag was passed is one they have to remember the absence of.
    eprintln!("big: mapsize {}", human_size(opts.mapsize));
    if let Some(n) = opts.max_row_keys {
        eprintln!("big: at most {n} row keys");
    }
    match &opts.backup_dir {
        Some(d) => eprintln!("big: POST /admin/backup writes into {d}"),
        None => eprintln!("big: no --backup-dir; POST /admin/backup is not configured"),
    }
    // Printed on every start, both ways: whether a cluster was allowed to reshape itself is
    // the first thing an operator asks after a range has moved, and a line that only appears
    // when the flag was passed is one they have to remember the absence of.
    match (opts.balance, opts.cluster.is_some()) {
        (true, true) => eprintln!("big: balancer on; the agreement's leader reshapes the cluster"),
        // Not refused: the flag is harmless alone, and a deployment that sets it everywhere
        // and adds --cluster later should not have to change twice. But a switch that does
        // nothing has to say so, or somebody will wait for it.
        (true, false) => eprintln!("big: --balance without --cluster: nothing to balance"),
        (false, _) => eprintln!("big: balancer off; POST /admin/cluster/rebalance is one step"),
    }
    // Both ways again, and this one is about the file rather than the cluster: an operator
    // asking why a database is not shrinking should be able to read the answer out of a log.
    if opts.reclaim {
        eprintln!("big: reclaiming trailing free pages while serving, once a quarter is free");
    } else {
        eprintln!("big: no --reclaim; the file gives space back only to an offline `big compact`");
    }
    // Both ways again. What a commit costs is the first thing an operator reaches for when
    // write latency is the question, and a line that appears only when the flag was passed is
    // one they have to remember the absence of.
    if opts.write_coalesce {
        eprintln!(
            "big: coalescing writes, at most {} batches or {} facts per commit",
            opts.write_group_jobs, opts.write_group_facts
        );
    } else {
        eprintln!("big: no --write-coalesce; every write is its own commit and its own fsyncs");
    }
    // Both ways, and this one says what is at risk rather than only what is on: an operator
    // reading a log after a crash needs to know whether anything could have been acknowledged
    // and lost.
    if opts.write_async && opts.write_coalesce {
        eprintln!(
            "big: ?ack=queued offered, holding at most {} for at most {}ms",
            human_size(opts.write_queue_bytes as u64),
            opts.write_linger.as_millis()
        );
    } else {
        eprintln!("big: no --write-async; a write is answered only once it is durable");
    }
    // Both ways, and the number matters: a subscriber holds a worker, so an operator sizing a
    // pool needs to see this next to --workers rather than infer it.
    if opts.watch_max > 0 {
        eprintln!(
            "big: GET /watch holding at most {} subscriptions, of {} workers",
            opts.watch_max,
            server.config().workers
        );
    } else {
        eprintln!("big: no --watch-max; GET /watch is not configured");
    }
    // Printed both ways for the reason the balancer is, and with the consequence spelled out:
    // an automatic handover can burn row ids the dead leader promised to writes that never
    // landed, which is not something to find out from a doc after the fact.
    match (opts.elect_schema_leader, opts.cluster.is_some()) {
        (true, true) => eprintln!(
            "big: the agreement may move the row-key namespace after 15s of silence; a \
             deposed leader must be repaired before it is trusted again"
        ),
        (true, false) => {
            eprintln!("big: --elect-schema-leader without --cluster: nothing to elect")
        }
        (false, _) => eprintln!(
            "big: row-key namespace stays put; bigctl cluster schema-leader is the only mover"
        ),
    }
    if !server.config().auth.is_enabled() {
        eprintln!("big: no authentication; this port must not be reachable from anywhere else");
    }
    if server.config().tls.is_none() {
        eprintln!(
            "big: serving in the clear; terminate TLS in front of this port or use --tls-cert"
        );
    }
    log::emit(
        log::Level::Info,
        "starting",
        &[("file", log::F::S(&opts.path)), ("addr", log::F::S(&bound.to_string()))],
    );
}

/// Everything `big serve` was told, before the two flags that spell "no limit" as `0` are folded.
///
/// **This is the clap type and the daemon's type at once.** They were separate for about an hour
/// while this was being written, and the copy between them was thirty-five lines that could only
/// ever be wrong in one direction. What keeps the defaults honest instead is
/// `clap_defaults_match_the_engines`, below: the engine owns them in
/// `big_embed::GroupConfig::default()`, `Default for Options` names them once, and the test
/// asserts the command line agrees.
#[derive(clap::Args, Debug)]
#[command(after_long_help = AFTER_LONG_HELP)]
pub struct Options {
    /// The database file. One process holds it: the engine takes an exclusive lock.
    #[arg(value_name = "FILE")]
    path: String,

    /// What to bind.
    #[arg(value_name = "ADDR", default_value = DEFAULT_ADDR)]
    addr: String,

    /// One `username role hash` per line, made by `big passwd`. Must be mode 600.
    ///
    /// A role is a name the catalog holds - made with CREATE ROLE, given privileges with GRANT -
    /// and a name it does not hold is no privileges at all. `superuser` is reserved and holds
    /// everything, which is how a new database gets its first GRANT.
    #[arg(long, value_name = "FILE", verbatim_doc_comment)]
    users: Option<String>,

    /// PEM certificate chain this server presents. Needs a build with the `tls` feature.
    #[arg(long, value_name = "FILE", requires = "tls_key")]
    tls_cert: Option<String>,

    /// PEM private key for --tls-cert; file must be mode 600.
    #[arg(long, value_name = "FILE", requires = "tls_cert")]
    tls_key: Option<String>,

    /// PEM chain this node presents to its peers.
    ///
    /// A cluster whose file names a peer CA needs this and --peer-key. Nodes prove themselves to
    /// each other with a certificate, not a shared secret, so one leaked key is one node.
    #[arg(long, value_name = "FILE", requires = "peer_key", verbatim_doc_comment)]
    peer_cert: Option<String>,

    /// PEM private key for --peer-cert; file must be mode 600.
    #[arg(long, value_name = "FILE", requires = "peer_cert")]
    peer_key: Option<String>,

    /// PEM CA the peers' certificates chain to, for --join.
    ///
    /// The cluster file's `peer_ca_file` says the same thing for everybody else.
    #[arg(long, value_name = "FILE", verbatim_doc_comment)]
    peer_ca: Option<String>,

    /// Take the cluster from a node already in it, instead of from a file.
    ///
    /// Needs --cluster-id and --node, and needs this node to have been added already: run
    /// `bigctl cluster add-node <name> <addr>` against a node that is in the cluster first, or
    /// this one is refused by name at startup. Add --peer-ca where the peers speak TLS.
    #[arg(
        long,
        value_name = "ADDR",
        conflicts_with = "cluster",
        requires_all = ["cluster_id", "node"],
        verbatim_doc_comment
    )]
    join: Option<String>,

    /// What names the cluster, as in the cluster file. Required with --join.
    ///
    /// Every peer request carries a stamp derived from it, so a joining node cannot ask for what
    /// it has not been told.
    #[arg(long, value_name = "NAME", verbatim_doc_comment)]
    cluster_id: Option<String>,

    /// Who owns which shards; see docs/clustering.md.
    ///
    /// Without it, this node owns every shard and has no peers. A node owns a range (shards) or
    /// copies one (replica), and a cluster with any replica needs three nodes or more.
    #[arg(long, value_name = "FILE", verbatim_doc_comment)]
    cluster: Option<String>,

    /// Which node in the cluster file this daemon is.
    ///
    /// Defaults to the one whose addr is the addr above.
    #[arg(long, value_name = "NAME", verbatim_doc_comment)]
    node: Option<String>,

    /// Allow a non-loopback bind with no users. Says what it is.
    #[arg(long = "insecure-no-auth")]
    insecure: bool,

    /// Allow a non-loopback bind in the clear. Says what it is.
    ///
    /// What you want when a reverse proxy terminates TLS in front.
    #[arg(long, verbatim_doc_comment)]
    insecure_no_tls: bool,

    /// Requests handled at once.
    #[arg(long, value_name = "N")]
    workers: Option<usize>,

    /// Connections allowed to wait; past this, 503.
    #[arg(long, value_name = "N")]
    queue: Option<usize>,

    /// How long a client may take to send a request.
    #[arg(long, value_name = "SECONDS", value_parser = secs)]
    read_timeout: Option<Duration>,

    /// Wall-clock budget for one query; 0 means none.
    #[arg(long, value_name = "SECONDS", value_parser = secs)]
    query_timeout: Option<Duration>,

    /// Let the cluster reshape itself. Needs --cluster to mean anything.
    ///
    /// A node that is draining has its ranges moved away, a node with nothing is given the tail,
    /// and a node much fuller than another gives a range up. One step at a time, decided by the
    /// agreement's leader. Off unless passed - a cluster that reshapes itself unasked is one
    /// whose shape an operator cannot predict.
    #[arg(long, verbatim_doc_comment)]
    balance: bool,

    /// Let the agreement move the row-key namespace after fifteen seconds of silence.
    ///
    /// Without it, `bigctl cluster schema-leader` is the only way it moves, and a dead leader
    /// means no *new* row key can be assigned until it is back. With it, a failover copies every
    /// row key of every table to the successor - and can burn row ids the dead leader assigned to
    /// writes that never landed, so the deposed node must be repaired before it is trusted again.
    /// See docs/clustering.md. Needs --cluster.
    #[arg(long, verbatim_doc_comment)]
    elect_schema_leader: bool,

    /// Give trailing free pages back to the filesystem while serving.
    ///
    /// Copy-on-write leaves holes and the freelist reuses them, so a file that has churned is
    /// mostly free space it never hands back; without this only an offline `big compact` shrinks
    /// it. Acts once a quarter of the file is reclaimable, needs a moment with no query in
    /// flight, and takes the write lock for one truncation. Watch big_pages_reclaimed_total and
    /// big_reclaim_blocked_total.
    #[arg(long, verbatim_doc_comment)]
    reclaim: bool,

    /// Let concurrent writes share a commit.
    ///
    /// The store allows one writer, so writes arriving together are serialised anyway; this makes
    /// them share one transaction and therefore one pair of fsyncs instead of one pair each. The
    /// group is collected while the first writer waits for the write lock, so a write with no
    /// company waits for none. A batch the engine refuses still fails alone. Off unless passed;
    /// watch big_write_commit_jobs_total / big_write_commits_total.
    #[arg(long, default_value_t = big_embed::GroupConfig::default().enabled, verbatim_doc_comment)]
    write_coalesce: bool,

    /// Batches one commit may carry.
    #[arg(long, value_name = "N", default_value_t = big_embed::GroupConfig::default().max_jobs)]
    write_group_jobs: usize,

    /// Facts one commit may carry, across every batch in it.
    ///
    /// Bounds the wait a small batch inherits from a large one it arrived behind; a batch larger
    /// than this still goes on its own.
    #[arg(
        long,
        value_name = "N",
        default_value_t = big_embed::GroupConfig::default().max_facts,
        verbatim_doc_comment
    )]
    write_group_facts: usize,

    /// Allow `?ack=queued` on an import or a delete. Needs --write-coalesce.
    ///
    /// Answered when the facts are held rather than when they are committed. **A different
    /// promise, not a faster one** - what has been acknowledged and not committed is lost if this
    /// process dies, and a batch the engine then refuses has nobody left to tell, so it is
    /// counted in big_write_acknowledged_lost_total instead. Single node only; a node with peers
    /// refuses ?ack=queued and says so. Watch big_write_queue_oldest_seconds.
    #[arg(
        long,
        default_value_t = big_embed::GroupConfig::default().async_writes,
        requires = "write_coalesce",
        verbatim_doc_comment
    )]
    write_async: bool,

    /// The longest an acknowledged write may wait to be committed, in milliseconds.
    ///
    /// A ceiling, not a delay: any writer arriving sooner carries it along.
    #[arg(
        long,
        value_name = "MS",
        value_parser = millis,
        default_value = "200",
        verbatim_doc_comment
    )]
    write_linger: Duration,

    /// Acknowledged writes this may hold. Suffixes K, M, G. Past it, see --write-when-full.
    #[arg(
        long,
        value_name = "SIZE",
        value_parser = size_usize,
        default_value_t = big_embed::GroupConfig::default().max_async_bytes
    )]
    write_queue_bytes: usize,

    /// What a full buffer does.
    ///
    /// `block` answers the write the durable way instead, which is backpressure aimed at whoever
    /// filled it; `refuse` answers 503 server_busy.
    #[arg(
        long,
        value_name = "WHEN",
        value_parser = when_full,
        default_value = "block",
        verbatim_doc_comment
    )]
    write_when_full: big_embed::WhenFull,

    /// Subscriptions GET /watch may hold at once; 0 turns the route off.
    ///
    /// A client subscribes with a SELECT and is pushed the answer again whenever it changes - a
    /// live query, not a change feed: the engine keeps no log of logical changes, and an answer
    /// here is a number rather than a row. **Each subscriber holds a worker for as long as it
    /// stays connected**, so this is the one route that can take the pool away from everything
    /// else; keep it well under --workers. On a node writing alone a push follows a commit at
    /// once; with peers it follows ?interval= instead, because a commit elsewhere notifies
    /// nothing here.
    #[arg(long, value_name = "N", default_value_t = 0, verbatim_doc_comment)]
    watch_max: usize,

    /// How hard a commit flushes.
    ///
    /// full    survives power loss
    /// barrier survives the OS dying, not the drive's cache
    /// none    survives this process dying, nothing more
    #[arg(long, value_name = "LEVEL", value_parser = durability, verbatim_doc_comment)]
    durability: Option<big_db::Durability>,

    /// Refuse to invent more than n row keys; 0 means no limit.
    ///
    /// Every row key is resident in memory in both directions, so this is the ceiling on the one
    /// allocation that grows with cardinality rather than with size. Watch big_row_keys.
    #[arg(long, value_name = "N", verbatim_doc_comment)]
    max_row_keys: Option<usize>,

    /// Where POST /admin/backup may write.
    ///
    /// Without it that route is not configured and says so. The request names a file inside this
    /// directory and cannot name one outside.
    #[arg(long, value_name = "DIR", verbatim_doc_comment)]
    backup_dir: Option<String>,

    /// Address space reserved for the file. Suffixes K, M, G, T.
    ///
    /// Reserved once and never remapped, so it is this file's ceiling until the daemon restarts.
    #[arg(
        long,
        value_name = "SIZE",
        value_parser = size_u64,
        default_value_t = big_embed::DEFAULT_MAPSIZE,
        verbatim_doc_comment
    )]
    mapsize: u64,

    /// Gone: bearer tokens were replaced by usernames and passwords.
    ///
    /// Recognised so that it can say what happened, rather than falling through to "unknown
    /// option" and sending an operator to check their spelling.
    #[arg(long, hide = true, num_args = 0..=1, value_name = "FILE")]
    tokens: Option<Option<String>>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            path: String::new(),
            addr: DEFAULT_ADDR.to_string(),
            users: None,
            tls_cert: None,
            peer_cert: None,
            peer_key: None,
            peer_ca: None,
            join: None,
            cluster_id: None,
            tls_key: None,
            insecure: false,
            insecure_no_tls: false,
            workers: None,
            queue: None,
            read_timeout: None,
            query_timeout: None,
            balance: false,
            elect_schema_leader: false,
            reclaim: false,
            write_coalesce: big_embed::GroupConfig::default().enabled,
            write_group_jobs: big_embed::GroupConfig::default().max_jobs,
            write_group_facts: big_embed::GroupConfig::default().max_facts,
            write_async: big_embed::GroupConfig::default().async_writes,
            write_linger: big_embed::GroupConfig::default().linger,
            write_queue_bytes: big_embed::GroupConfig::default().max_async_bytes,
            write_when_full: big_embed::GroupConfig::default().when_full,
            watch_max: 0,
            durability: None,
            cluster: None,
            node: None,
            backup_dir: None,
            max_row_keys: None,
            mapsize: big_embed::DEFAULT_MAPSIZE,
            tokens: None,
        }
    }
}

impl Options {
    /// Folds the two flags that spell "no limit" as `0`, and the removed one that explains itself.
    ///
    /// Everything else clap has already checked. What is left here is the handful of rules that
    /// are about a *value* rather than about which flags may appear together: `0` meaning "none"
    /// rather than "a budget of nothing", which would refuse every query and be a confusing way
    /// to spell it.
    fn normalize(mut self) -> Result<Self, String> {
        if self.tokens.is_some() {
            return Err("--tokens is gone: bearer tokens were replaced by usernames and \
                        passwords. Make a users file with `big passwd <file> set <user>` and \
                        pass --users."
                .to_string());
        }
        if self.query_timeout == Some(Duration::ZERO) {
            self.query_timeout = None;
        }
        if self.max_row_keys == Some(0) {
            self.max_row_keys = None;
        }
        // Clamped rather than refused, which is what the hand-rolled parser did: a group of zero
        // batches is a group of one, and there is nothing for an operator to fix.
        self.write_group_jobs = self.write_group_jobs.max(1);
        self.write_group_facts = self.write_group_facts.max(1);
        Ok(self)
    }
}

/// The size flag, spelled the way it was passed rather than in bytes.
fn human_size(bytes: u64) -> String {
    for (suffix, scale) in [("T", 1u64 << 40), ("G", 1 << 30), ("M", 1 << 20), ("K", 1 << 10)] {
        if bytes >= scale && bytes.is_multiple_of(scale) {
            return format!("{}{suffix}", bytes / scale);
        }
    }
    format!("{bytes}")
}

fn secs(s: &str) -> Result<Duration, String> {
    s.parse().map(Duration::from_secs).map_err(|_| format!("`{s}` is not a number of seconds"))
}

fn millis(s: &str) -> Result<Duration, String> {
    s.parse().map(Duration::from_millis).map_err(|_| format!("`{s}` is not a number of ms"))
}

fn durability(s: &str) -> Result<big_db::Durability, String> {
    big_db::Durability::parse(s).ok_or_else(|| format!("expected full, barrier or none, got `{s}`"))
}

fn when_full(s: &str) -> Result<big_embed::WhenFull, String> {
    match s {
        "block" => Ok(big_embed::WhenFull::Block),
        "refuse" => Ok(big_embed::WhenFull::Refuse),
        _ => Err(format!("expected block or refuse, got `{s}`")),
    }
}

fn size_usize(s: &str) -> Result<usize, String> {
    size_u64(s).map(|n| n as usize)
}

/// A byte count, with the suffixes an operator actually types.
///
/// Powers of 1024 rather than 1000: the number is a count of pages times 8 KiB, and a
/// "gigabyte" that did not divide by the page size would be a number the pager silently
/// rounds. Zero is refused here rather than in the pager, so the message names the flag.
fn size_u64(s: &str) -> Result<u64, String> {
    let (digits, scale) = match s.as_bytes().last() {
        Some(b'K' | b'k') => (&s[..s.len() - 1], 1u64 << 10),
        Some(b'M' | b'm') => (&s[..s.len() - 1], 1u64 << 20),
        Some(b'G' | b'g') => (&s[..s.len() - 1], 1u64 << 30),
        Some(b'T' | b't') => (&s[..s.len() - 1], 1u64 << 40),
        _ => (s, 1),
    };
    let n: u64 = digits.parse().map_err(|_| format!("expected a size like 64G, got `{s}`"))?;
    let bytes = n
        .checked_mul(scale)
        .ok_or_else(|| format!("larger than this machine can address: `{s}`"))?;
    if bytes == 0 {
        return Err("cannot be zero".to_string());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(clap::Parser)]
    struct Harness {
        #[command(flatten)]
        options: Options,
    }

    fn parse(args: &[&str]) -> Result<Options, clap::Error> {
        let argv = std::iter::once("big serve").chain(args.iter().copied());
        Harness::try_parse_from(argv).map(|h| h.options)
    }

    /// The command line's defaults are the engine's defaults.
    ///
    /// **This is what lets `Options` be the clap type and the daemon's type at once.** clap needs
    /// every default spelled in an attribute; the engine owns them in
    /// `big_embed::GroupConfig::default()`. Two spellings of one number is exactly the drift the
    /// rest of this port was about, so it is asserted rather than commented.
    #[test]
    fn clap_defaults_match_the_engines() {
        let parsed = parse(&["data.big"]).expect("a path is the only required argument");
        let expected = Options { path: "data.big".to_string(), ..Options::default() };

        assert_eq!(parsed.addr, expected.addr);
        assert_eq!(parsed.write_coalesce, expected.write_coalesce);
        assert_eq!(parsed.write_group_jobs, expected.write_group_jobs);
        assert_eq!(parsed.write_group_facts, expected.write_group_facts);
        assert_eq!(parsed.write_async, expected.write_async);
        assert_eq!(parsed.write_linger, expected.write_linger);
        assert_eq!(parsed.write_queue_bytes, expected.write_queue_bytes);
        assert_eq!(parsed.write_when_full, expected.write_when_full);
        assert_eq!(parsed.watch_max, expected.watch_max);
        assert_eq!(parsed.mapsize, expected.mapsize);
    }

    /// `0` spells "no limit" on the two flags that have one, rather than "a limit of nothing".
    #[test]
    fn zero_means_no_limit_rather_than_a_limit_of_zero() {
        let o = parse(&["f", "--query-timeout", "0", "--max-row-keys", "0"]).unwrap();
        let o = o.normalize().unwrap();
        assert_eq!(o.query_timeout, None);
        assert_eq!(o.max_row_keys, None);

        let o = parse(&["f", "--query-timeout", "30", "--max-row-keys", "5"]).unwrap();
        let o = o.normalize().unwrap();
        assert_eq!(o.query_timeout, Some(Duration::from_secs(30)));
        assert_eq!(o.max_row_keys, Some(5));
    }

    /// The flags that need other flags are refused together, not silently half-applied.
    #[test]
    fn a_flag_that_needs_another_is_refused_without_it() {
        for args in [
            vec!["f", "--write-async"],
            vec!["f", "--tls-cert", "c"],
            vec!["f", "--tls-key", "k"],
            vec!["f", "--peer-cert", "c"],
            vec!["f", "--peer-key", "k"],
            vec!["f", "--join", "a:1"],
            vec!["f", "--join", "a:1", "--cluster-id", "c"],
            // `--cluster` and `--join` are two ways to learn the same thing, so naming both is a
            // question about which one wins that has no good answer.
            vec!["f", "--cluster", "c.toml", "--join", "a:1"],
        ] {
            assert!(parse(&args).is_err(), "`{}` should be refused", args.join(" "));
        }
    }

    /// The removed flag says what happened rather than "unknown option".
    #[test]
    fn the_removed_tokens_flag_explains_itself() {
        let o = parse(&["f", "--tokens", "t"]).expect("still recognised, so it can explain itself");
        let e = o.normalize().expect_err("but refused");
        assert!(e.contains("--tokens is gone"), "{e}");
    }

    /// Sizes carry the suffixes an operator types, in powers of 1024.
    #[test]
    fn a_size_takes_the_suffixes_an_operator_types() {
        assert_eq!(size_u64("64M").unwrap(), 64 << 20);
        assert_eq!(size_u64("1t").unwrap(), 1 << 40);
        assert_eq!(size_u64("4096").unwrap(), 4096);
        assert!(size_u64("0").is_err());
        assert!(size_u64("").is_err());
        assert!(size_u64("64X").is_err());
        assert!(size_u64("99999999T").is_err(), "overflow is refused, not wrapped");
    }
}
