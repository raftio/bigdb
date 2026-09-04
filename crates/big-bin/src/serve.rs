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
use std::time::Duration;

const DEFAULT_ADDR: &str = "127.0.0.1:7654";

const USAGE: &str = "\
usage: big serve <file> [addr] [options]

  addr                        defaults to 127.0.0.1:7654

  --users <file>              one `username role hash` per line, made by `big passwd`
                              roles: read, write, admin; file must be mode 600
  --tls-cert <file>           PEM certificate chain this server presents
  --tls-key <file>            PEM private key for it; file must be mode 600
                              both need a build with the `tls` feature
  --peer-cert <file>          PEM chain this node presents to its peers
  --peer-key <file>           PEM private key for it; file must be mode 600
                              a cluster whose file names a peer CA needs both. Nodes
                              prove themselves to each other with a certificate, not a
                              shared secret, so one leaked key is one node
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

  BIG_LOG=off|error|warn|info|debug   log level, default info

Listing:
  GET /table/{t}/records?after=<id>&limit=<n>   records in order, a page at a time
                              `next` in the response is the id to send back as `after`

Replication:
  GET  /verify   do the copies of every range still hold the same facts
                 a scan, not a probe; needs a `read` user
  POST /repair   catch up every copy the agreement has marked behind
                 a scan and a copy; needs an `admin` user

Backup:
  POST /admin/backup?name=<f>  an online, compact copy of this node's file
                 needs --backup-dir and an `admin` user; one at a time
                 a cluster is backed up one node at a time, and the copies
                 are not one snapshot - see docs/clustering.md

Probes and metrics:
  GET /health    liveness, never authenticated
  GET /ready     readiness, never authenticated
  GET /metrics   Prometheus text; needs a `read` user when --users is configured
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

    let server = Server::bind_cluster(cluster, opts.addr.as_str(), serving(auth, tls, &opts))?;
    let bound = server.local_addr()?;
    refuse_an_open_port(&server, bound, &opts);
    announce(&server, bound, &opts);

    server.serve()
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

fn assemble(
    api: Api<big_embed::MmapPager>,
    opts: &Options,
    file: Option<ClusterFile>,
) -> std::io::Result<Cluster<big_embed::MmapPager>> {
    let (Some(path), Some(file)) = (&opts.cluster, file) else { return Ok(Cluster::solo(api)) };
    let peer_ca = file.peer_ca_file().map(str::to_string);
    let config = file
        .for_node(opts.node.as_deref(), &opts.addr)
        .map_err(|e| std::io::Error::other(format!("{path}: {e}")))?;
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
    Cluster::new(api, config, tls, Box::new(big_cluster::raft::FileStore::new(state)))
}

/// The listener's settings: the defaults, with whatever the operator overrode.
fn serving(auth: Auth, tls: Option<TlsConfig>, opts: &Options) -> ServerConfig {
    let base = ServerConfig::default();
    ServerConfig {
        auth,
        tls,
        backup_dir: opts.backup_dir.clone(),
        query_timeout: opts.query_timeout,
        workers: opts.workers.unwrap_or(base.workers),
        queue_depth: opts.queue.unwrap_or(base.queue_depth),
        read_timeout: opts.read_timeout.unwrap_or(base.read_timeout),
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
    durability: Option<big_db::Durability>,
    cluster: Option<String>,
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
            tls_key: None,
            insecure: false,
            insecure_no_tls: false,
            workers: None,
            queue: None,
            read_timeout: None,
            query_timeout: None,
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
