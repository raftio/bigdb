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

const USAGE: &str = "\
usage: big serve <file> [addr] [options]

  addr                        defaults to 127.0.0.1:7654

  --users <file>              one `username role hash` per line, made by `big passwd`
                              a role is a name the catalog holds - made with CREATE ROLE,
                              given privileges with GRANT - and a name it does not hold is
                              no privileges at all. `superuser` is reserved and holds
                              everything, which is how a new database gets its first GRANT.
                              The file must be mode 600
  --tls-cert <file>           PEM certificate chain this server presents
  --tls-key <file>            PEM private key for it; file must be mode 600
                              both need a build with the `tls` feature
  --peer-cert <file>          PEM chain this node presents to its peers
  --peer-key <file>           PEM private key for it; file must be mode 600
                              a cluster whose file names a peer CA needs both. Nodes
                              prove themselves to each other with a certificate, not a
                              shared secret, so one leaked key is one node
  --join <addr>               take the cluster from a node already in it, instead of a file.
                              Needs --cluster-id and --node, and needs this node to have been
                              added already: run `bigctl cluster add-node <name> <addr>` against
                              a node that is in the cluster first, or this one is refused by
                              name at startup. Add --peer-ca where the peers speak TLS
  --cluster-id <name>         what names the cluster, as in the cluster file. Required with
                              --join: every peer request carries a stamp derived from it, so a
                              joining node cannot ask for what it has not been told
  --peer-ca <file>            PEM CA the peers' certificates chain to, for --join. The cluster
                              file's `peer_ca_file` says the same thing for everybody else
  --cluster <file>            who owns which shards; see docs/clustering.md
                              without it, this node owns every shard and has no peers
                              a node owns a range (shards) or copies one (replica)
                              a cluster with any replica needs three nodes or more
  --node <name>               which node in the cluster file this daemon is
                              defaults to the one whose addr is the addr above
  --insecure-no-auth          allow a non-loopback bind with no users. Says what it is.
  --insecure-no-tls           allow a non-loopback bind in the clear. Says what it is.
                              what you want when a reverse proxy terminates TLS in front
  --workers <n>               requests handled at once
  --queue <n>                 connections allowed to wait; past this, 503
  --read-timeout <seconds>    how long a client may take to send a request
  --query-timeout <seconds>   wall-clock budget for one query; 0 means none
  --reclaim                   give trailing free pages back to the filesystem while serving.
                              Copy-on-write leaves holes and the freelist reuses them, so a
                              file that has churned is mostly free space it never hands back;
                              without this only an offline `big compact` shrinks it. Acts once
                              a quarter of the file is reclaimable, needs a moment with no
                              query in flight, and takes the write lock for one truncation.
                              Watch big_pages_reclaimed_total and big_reclaim_blocked_total
  --elect-schema-leader       let the agreement give the row-key namespace to another node
                              when the one holding it has been silent for fifteen seconds.
                              Without it, `bigctl cluster schema-leader` is the only way it
                              moves, and a dead leader means no *new* row key can be assigned
                              until it is back. With it, a failover copies every row key of
                              every table to the successor - and can burn row ids the dead
                              leader assigned to writes that never landed, so the deposed node
                              must be repaired before it is trusted again. See
                              docs/clustering.md. Needs --cluster
  --balance                   let the cluster reshape itself: a node that is draining has its
                              ranges moved away, a node with nothing is given the tail, and a
                              node much fuller than another gives a range up. One step at a
                              time, decided by the agreement's leader. Off unless passed - a
                              cluster that reshapes itself unasked is one whose shape an
                              operator cannot predict. Needs --cluster to mean anything
  --max-row-keys <n>          refuse to invent more than n row keys; 0 means no limit
                              every row key is resident in memory in both directions, so
                              this is the ceiling on the one allocation that grows with
                              cardinality rather than with size. Watch big_row_keys
  --backup-dir <dir>          where POST /admin/backup may write; without it that
                              route is not configured and says so. The request names
                              a file inside this directory and cannot name one outside
  --mapsize <size>            address space reserved for the file, default 1T
                              suffixes K, M, G, T; reserved once and never remapped,
                              so it is this file's ceiling until the daemon restarts
  --durability full|barrier|none
                              how hard a commit flushes, default full
                              full    survives power loss
                              barrier survives the OS dying, not the drive's cache
                              none    survives this process dying, nothing more
  --write-coalesce            let concurrent writes share a commit. The store allows one
                              writer, so writes arriving together are serialised anyway;
                              this makes them share one transaction and therefore one pair
                              of fsyncs instead of one pair each. The group is collected
                              while the first writer waits for the write lock, so a write
                              with no company waits for none. A batch the engine refuses
                              still fails alone. Off unless passed; watch
                              big_write_commit_jobs_total / big_write_commits_total
  --write-group-jobs <n>      batches one commit may carry, default 64
  --write-group-facts <n>     facts one commit may carry, default 1048576. Bounds the wait
                              a small batch inherits from a large one it arrived behind;
                              a batch larger than this still goes on its own
  --write-async               allow `?ack=queued` on an import or a delete: answered when the
                              facts are held rather than when they are committed. **A different
                              promise, not a faster one** - what has been acknowledged and not
                              committed is lost if this process dies, and a batch the engine
                              then refuses has nobody left to tell, so it is counted in
                              big_write_acknowledged_lost_total instead. Single node only; a
                              node with peers refuses ?ack=queued and says so. Needs
                              --write-coalesce. Watch big_write_queue_oldest_seconds
  --write-linger <ms>         the longest an acknowledged write may wait to be committed,
                              default 200. A ceiling, not a delay: any writer arriving sooner
                              carries it along
  --write-queue-bytes <size>  acknowledged writes this may hold, default 64M. Past it, see
                              --write-when-full. Suffixes K, M, G
  --watch-max <n>             subscriptions GET /watch may hold at once, default 0 (off).
                              A client subscribes with a SELECT and is pushed the answer
                              again whenever it changes - a live query, not a change feed:
                              the engine keeps no log of logical changes, and an answer here
                              is a number rather than a row. **Each subscriber holds a worker
                              for as long as it stays connected**, so this is the one route
                              that can take the pool away from everything else; keep it well
                              under --workers. On a node writing alone a push follows a commit
                              at once; with peers it follows ?interval= instead, because a
                              commit elsewhere notifies nothing here
  --write-when-full block|refuse
                              what a full buffer does, default block. block answers the write
                              the durable way instead, which is backpressure aimed at whoever
                              filled it; refuse answers 503 server_busy

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

/// `big serve`, with the word already stripped by the dispatcher.
pub fn main(args: &[String]) -> std::io::Result<()> {
    let opts = match Options::parse(args) {
        Ok(o) => o,
        // `--help` is not a failure, so it prints the usage to stdout and exits zero.
        // Anything else is, and says what was wrong before repeating the usage.
        Err(e) if e.is_empty() => {
            print!("{USAGE}");
            return Ok(());
        }
        Err(e) => {
            eprintln!("big serve: {e}\n");
            eprint!("{USAGE}");
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
            let roster: Vec<String> = cluster
                .map(|c| c.nodes().iter().map(|n| n.name.clone()).collect())
                .unwrap_or_default();
            if peer_ca.is_some() && roster.is_empty() {
                return Err(std::io::Error::other(
                    "a peer CA is configured but the cluster file names no nodes",
                ));
            }
            let tls =
                TlsConfig::load(Path::new(cert), Path::new(key), peer_ca.map(Path::new), roster)
                    .map_err(|e| {
                        std::io::Error::new(e.kind(), format!("could not load {cert}: {e}"))
                    })?;
            eprintln!("big: tls certificate from {cert}");
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
             Anyone who can reach this port can read and delete everything in the database.\n\
             Either pass --users <file>, or bind to loopback and put a reverse proxy in\n\
             front of it, or pass --insecure-no-auth if the port really is private."
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

struct Options {
    path: String,
    addr: String,
    users: Option<String>,
    tls_cert: Option<String>,
    peer_cert: Option<String>,
    peer_key: Option<String>,
    tls_key: Option<String>,
    insecure: bool,
    insecure_no_tls: bool,
    workers: Option<usize>,
    queue: Option<usize>,
    read_timeout: Option<Duration>,
    query_timeout: Option<Duration>,
    /// Whether the agreement's leader may reshape the cluster on its own.
    balance: bool,
    /// Whether the agreement may give the row-key namespace to another node on its own.
    elect_schema_leader: bool,
    /// Whether this node may hand trailing free pages back to the filesystem while serving.
    reclaim: bool,
    /// Whether concurrent writers may share one commit.
    write_coalesce: bool,
    /// Batches one commit may carry. Not `Option`: the default belongs to the engine, and
    /// `big_embed::GroupConfig` is where it is written down.
    write_group_jobs: usize,
    /// Facts one commit may carry, across every batch in it.
    write_group_facts: usize,
    /// Whether `?ack=queued` is offered at all.
    write_async: bool,
    /// The ceiling on how stale an acknowledged write may be.
    write_linger: Duration,
    /// Acknowledged, uncommitted bytes this node may hold.
    write_queue_bytes: usize,
    /// What a full buffer does.
    write_when_full: big_embed::WhenFull,
    /// Subscriptions `GET /watch` may hold at once. `0` turns the route off.
    watch_max: usize,
    durability: Option<big_db::Durability>,
    cluster: Option<String>,
    /// The address of a node already in the cluster, for a daemon started with no cluster file.
    join: Option<String>,
    /// What names the cluster, needed with `--join`: the request that fetches the membership
    /// carries the stamp derived from it, so it cannot be learned by asking.
    cluster_id: Option<String>,
    /// The CA a joining node's peers are signed by. In the cluster file for everybody else.
    peer_ca: Option<String>,
    node: Option<String>,
    backup_dir: Option<String>,
    max_row_keys: Option<usize>,
    /// Address space reserved for the mapping, in bytes. Never `None`: a default that is
    /// applied here rather than deep in the pager is one an operator can read back in the
    /// startup line.
    mapsize: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            path: String::new(),
            addr: String::new(),
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
        }
    }
}

impl Options {
    /// Positional first, then flags. Hand-rolled for the same reason the HTTP parser is: eight
    /// options do not justify an argument-parsing dependency.
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut out = Self { addr: DEFAULT_ADDR.to_string(), ..Default::default() };
        let mut positional = Vec::new();
        let mut i = 0;

        while i < args.len() {
            let arg = args[i].as_str();
            // A flag's value is required, and saying which flag is missing one is the whole
            // difference between a usable error and "invalid arguments".
            let value = || args.get(i + 1).cloned().ok_or_else(|| format!("{arg} needs a value"));
            match arg {
                "--users" => {
                    out.users = Some(value()?);
                    i += 2;
                }
                // Recognised for one release so that it can say what happened. A flag that has
                // been removed and explains itself is worth more than a clean parser: falling
                // through to "unknown option" would send an operator to check their spelling.
                "--tokens" => {
                    return Err("--tokens is gone: bearer tokens were replaced by usernames and \
                         passwords. Make a users file with `big passwd <file> set <user>` and \
                         pass --users."
                        .to_string())
                }
                "--tls-cert" => {
                    out.tls_cert = Some(value()?);
                    i += 2;
                }
                "--tls-key" => {
                    out.tls_key = Some(value()?);
                    i += 2;
                }
                "--peer-cert" => {
                    out.peer_cert = Some(value()?);
                    i += 2;
                }
                "--peer-key" => {
                    out.peer_key = Some(value()?);
                    i += 2;
                }
                "--cluster" => {
                    out.cluster = Some(value()?);
                    i += 2;
                }
                "--join" => {
                    out.join = Some(value()?);
                    i += 2;
                }
                "--cluster-id" => {
                    out.cluster_id = Some(value()?);
                    i += 2;
                }
                "--peer-ca" => {
                    out.peer_ca = Some(value()?);
                    i += 2;
                }
                "--node" => {
                    out.node = Some(value()?);
                    i += 2;
                }
                "--workers" => {
                    out.workers = Some(parse_num(&value()?, arg)?);
                    i += 2;
                }
                "--queue" => {
                    out.queue = Some(parse_num(&value()?, arg)?);
                    i += 2;
                }
                "--read-timeout" => {
                    out.read_timeout = Some(Duration::from_secs(parse_num(&value()?, arg)? as u64));
                    i += 2;
                }
                "--query-timeout" => {
                    let secs = parse_num(&value()?, arg)? as u64;
                    // Zero means "no budget", not "a budget of nothing", which would refuse
                    // every query and be a confusing way to spell it.
                    out.query_timeout = (secs > 0).then(|| Duration::from_secs(secs));
                    i += 2;
                }
                "--balance" => {
                    out.balance = true;
                    i += 1;
                }
                "--elect-schema-leader" => {
                    out.elect_schema_leader = true;
                    i += 1;
                }
                "--reclaim" => {
                    out.reclaim = true;
                    i += 1;
                }
                "--write-coalesce" => {
                    out.write_coalesce = true;
                    i += 1;
                }
                "--write-group-jobs" => {
                    out.write_group_jobs = parse_num(&value()?, arg)?.max(1);
                    i += 2;
                }
                "--write-group-facts" => {
                    out.write_group_facts = parse_num(&value()?, arg)?.max(1);
                    i += 2;
                }
                "--watch-max" => {
                    out.watch_max = parse_num(&value()?, arg)?;
                    i += 2;
                }
                "--write-async" => {
                    out.write_async = true;
                    i += 1;
                }
                "--write-linger" => {
                    out.write_linger = Duration::from_millis(parse_num(&value()?, arg)? as u64);
                    i += 2;
                }
                "--write-queue-bytes" => {
                    out.write_queue_bytes = parse_size(&value()?, arg)? as usize;
                    i += 2;
                }
                "--write-when-full" => {
                    let v = value()?;
                    out.write_when_full = match v.as_str() {
                        "block" => big_embed::WhenFull::Block,
                        "refuse" => big_embed::WhenFull::Refuse,
                        _ => {
                            return Err(format!(
                                "--write-when-full takes block or refuse, got `{v}`"
                            ))
                        }
                    };
                    i += 2;
                }
                "--durability" => {
                    let v = value()?;
                    out.durability = Some(big_db::Durability::parse(&v).ok_or_else(|| {
                        format!("--durability takes full, barrier or none, got `{v}`")
                    })?);
                    i += 2;
                }
                "--max-row-keys" => {
                    let n = parse_num(&value()?, arg)?;
                    // Zero spells "no ceiling" rather than "a ceiling of nothing", which
                    // would refuse every keyed write and be a confusing way to say it. Same
                    // convention as --query-timeout.
                    out.max_row_keys = (n > 0).then_some(n);
                    i += 2;
                }
                "--backup-dir" => {
                    out.backup_dir = Some(value()?);
                    i += 2;
                }
                "--mapsize" => {
                    out.mapsize = parse_size(&value()?, arg)?;
                    i += 2;
                }
                "--insecure-no-auth" => {
                    out.insecure = true;
                    i += 1;
                }
                "--insecure-no-tls" => {
                    out.insecure_no_tls = true;
                    i += 1;
                }
                "-h" | "--help" => return Err("".to_string()),
                other if other.starts_with('-') => return Err(format!("unknown option {other}")),
                other => {
                    positional.push(other.to_string());
                    i += 1;
                }
            }
        }

        // Flags that need other flags, checked here so they exit 2 and reprint the usage the way
        // every other usage error does.
        match (&out.tls_cert, &out.tls_key) {
            (Some(_), Some(_)) | (None, None) => {}
            // Named individually rather than "both are required": the operator passed one of
            // them, so the useful sentence is which one is missing, not what the pair is called.
            (Some(_), None) => return Err("--tls-cert needs --tls-key".to_string()),
            (None, Some(_)) => return Err("--tls-key needs --tls-cert".to_string()),
        }
        match (&out.peer_cert, &out.peer_key) {
            (Some(_), Some(_)) | (None, None) => {}
            (Some(_), None) => return Err("--peer-cert needs --peer-key".to_string()),
            (None, Some(_)) => return Err("--peer-key needs --peer-cert".to_string()),
        }
        // Refused rather than quietly ignored. An operator who asked for early answers and got
        // durable ones would see the flag in the command line, the writes going through, and no
        // reason at all for the latency.
        if out.write_async && !out.write_coalesce {
            return Err("--write-async needs --write-coalesce".to_string());
        }

        match positional.as_slice() {
            [path] => out.path = path.clone(),
            [path, addr] => {
                out.path = path.clone();
                out.addr = addr.clone();
            }
            [] => return Err("a database file is required".to_string()),
            _ => return Err("too many positional arguments".to_string()),
        }
        Ok(out)
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

fn parse_num(s: &str, flag: &str) -> Result<usize, String> {
    s.parse().map_err(|_| format!("{flag} needs a number, got `{s}`"))
}

/// A byte count, with the suffixes an operator actually types.
///
/// Powers of 1024 rather than 1000: the number is a count of pages times 8 KiB, and a
/// "gigabyte" that did not divide by the page size would be a number the pager silently
/// rounds. Zero is refused here rather than in the pager, so the message names the flag.
fn parse_size(s: &str, flag: &str) -> Result<u64, String> {
    let (digits, scale) = match s.as_bytes().last() {
        Some(b'K' | b'k') => (&s[..s.len() - 1], 1u64 << 10),
        Some(b'M' | b'm') => (&s[..s.len() - 1], 1u64 << 20),
        Some(b'G' | b'g') => (&s[..s.len() - 1], 1u64 << 30),
        Some(b'T' | b't') => (&s[..s.len() - 1], 1u64 << 40),
        _ => (s, 1),
    };
    let n: u64 = digits.parse().map_err(|_| format!("{flag} needs a size like 64G, got `{s}`"))?;
    let bytes = n
        .checked_mul(scale)
        .ok_or_else(|| format!("{flag} is larger than this machine can address: `{s}`"))?;
    if bytes == 0 {
        return Err(format!("{flag} cannot be zero"));
    }
    Ok(bytes)
}
