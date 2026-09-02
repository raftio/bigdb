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

//! Bearer tokens from a file, and three roles.
//!
//! **Why not a user database.** There is one database per process and no notion of an owner
//! anywhere in the catalog, so "who" has nothing to attach to. What an operator actually needs
//! is to stop the port being world-writable, and a file of tokens does that without inventing
//! a subject model the engine cannot honour. When multi-tenancy is decided, this is the layer
//! that grows a subject - not the one that has to be unwound.
//!
//! **Why the tokens are not hashed.** Hashing them would need a password hash, which would be
//! a new dependency, and it protects against exactly one thing: an attacker who can read the
//! token file. Anyone who can read that file can also read the database file next to it, so
//! the hash would be guarding the smaller of the two secrets. What is enforced instead is the
//! thing that actually keeps the file unread: [`Auth::from_file`] refuses a token file that
//! anyone but its owner can open.
//!
//! **Comparison is constant time.** A token is compared byte for byte with no early exit, so
//! the time taken does not reveal how much of a guess was right.

use big_api::Authority;
use std::io;
use std::path::Path;

/// What a credential is allowed to do. Ordered: each role contains the ones below it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Role {
    /// Read the schema and run queries.
    Read = 0,
    /// The above, plus writing and deleting records.
    Write = 1,
    /// The above, plus creating and dropping schema, and reading metrics.
    Admin = 2,
}

impl Role {
    /// The name used in the token file and in log lines. The same spelling in both, so a
    /// grep for a role in the log finds the line in the file that granted it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Admin => "admin",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "read" => Self::Read,
            "write" => Self::Write,
            "admin" => Self::Admin,
            _ => return None,
        })
    }
}

/// What a statement demands, as what a credential has to hold.
///
/// **The whole of what this crate knows about SQL.** Which statements write and which only read
/// is [`big_api::Sql::authority`]'s to say, next to the variants it is about; this is the one
/// line that turns that answer into the vocabulary of a token file. An edge deciding it by
/// matching on the AST itself is how the rule ends up written twice and enforced once.
impl From<Authority> for Role {
    fn from(authority: Authority) -> Self {
        match authority {
            Authority::Read => Self::Read,
            Authority::Write => Self::Write,
            Authority::Admin => Self::Admin,
        }
    }
}

/// The verdict on one request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// Allowed, carrying the role it was allowed under - which is what the log records.
    Allowed(Option<Role>),
    /// No credential, or one this server does not know. `401`.
    Unauthenticated,
    /// A known credential that does not reach far enough. `403`.
    Forbidden {
        /// What the presented credential grants.
        held: Role,
        /// What the route required.
        needed: Role,
    },
}

/// The token table, and the decision of who may do what.
///
/// `Default` is disabled, which is safe only because binding anywhere but loopback without a
/// token file is refused by `bigd`. See its `--insecure-no-auth` flag for the deliberate
/// override.
#[derive(Clone, Default)]
pub struct Auth {
    /// `None` means authentication is switched off and every request is allowed. That is the
    /// default only because a loopback-only server is the default; binding anywhere else
    /// without a token file is refused by `bigd`, not here.
    tokens: Option<Vec<(String, Role)>>,
}

/// Redacted on purpose. A derived `Debug` would put every token into whatever printed it -
/// a panic message, a test failure, an `{:?}` someone left in - and a secret that reaches a
/// log is no longer a secret. The count is the part that is ever useful to see.
impl core::fmt::Debug for Auth {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match &self.tokens {
            None => write!(f, "Auth(disabled)"),
            Some(t) => write!(f, "Auth({} tokens, redacted)", t.len()),
        }
    }
}

impl Auth {
    /// Authentication off: every request is allowed, and every log line records no role.
    pub fn disabled() -> Self {
        Self { tokens: None }
    }

    /// Whether a token file was loaded at all. Distinct from [`Auth::is_empty`]: a file that
    /// exists and grants nobody anything is enabled and empty, and refuses every request.
    pub fn is_enabled(&self) -> bool {
        self.tokens.is_some()
    }

    /// How many credentials are loaded. For the line the daemon prints on the way up.
    pub fn len(&self) -> usize {
        self.tokens.as_ref().map_or(0, Vec::len)
    }

    /// Whether no credentials are loaded, whether or not authentication is enabled.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Reads `token<space>role` lines. `#` starts a comment; blank lines are skipped.
    ///
    /// Refuses a file that is readable by anyone but its owner, and refuses an empty one: a
    /// token file with no tokens locks the operator out of their own database, which is never
    /// what they meant by writing one.
    pub fn from_file(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        check_permissions(path)?;

        let text = std::fs::read_to_string(path)?;
        let mut tokens = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut parts = line.split_whitespace();
            let (Some(token), Some(role)) = (parts.next(), parts.next()) else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: line {}: expected `token role`", path.display(), n + 1),
                ));
            };
            let Some(role) = Role::parse(role) else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{}: line {}: `{role}` is not a role; use read, write or admin",
                        path.display(),
                        n + 1
                    ),
                ));
            };
            tokens.push((token.to_string(), role));
        }

        if tokens.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: no tokens; this would refuse every request", path.display()),
            ));
        }
        Ok(Self { tokens: Some(tokens) })
    }

    /// Whether `presented` may do something that needs `needed`.
    pub fn authorize(&self, presented: Option<&str>, needed: Role) -> Outcome {
        let Some(tokens) = &self.tokens else { return Outcome::Allowed(None) };
        let Some(presented) = presented else { return Outcome::Unauthenticated };

        // Every entry is compared, and the whole of every entry: returning as soon as one
        // matches would leak, through timing, how far down the file a token sits.
        let mut held: Option<Role> = None;
        for (token, role) in tokens {
            if constant_time_eq(token.as_bytes(), presented.as_bytes()) {
                held = Some(*role);
            }
        }

        match held {
            None => Outcome::Unauthenticated,
            Some(held) if held >= needed => Outcome::Allowed(Some(held)),
            Some(held) => Outcome::Forbidden { held, needed },
        }
    }
}

/// One secret out of a file, with the same refusal a token file gets.
///
/// The outbound half of the same problem: a node presenting a bearer token to its peers needs
/// to read one from somewhere, and "somewhere" has to be as unreadable as the token file that
/// grants it. Comments and blank lines are skipped so that the file can say what it is for;
/// the first line left is the secret, whitespace trimmed.
///
/// Here rather than in `big-cluster` because this is where the policy lives - what makes a file
/// fit to hold a secret is one rule, and a second copy of it in another crate is a second rule
/// that can drift.
pub fn read_secret_file(path: impl AsRef<Path>) -> io::Result<String> {
    let path = path.as_ref();
    check_permissions(path)?;
    let text = std::fs::read_to_string(path)?;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if !line.is_empty() {
            return Ok(line.to_string());
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}: no token in this file", path.display()),
    ))
}

/// Equal-length, equal-content, in time that does not depend on where they differ.
///
/// The length check is not constant time and does not need to be: a token's length is not the
/// secret, and comparing different lengths byte-for-byte would need a decision about what to
/// compare the short one against.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// A token file anyone can read is not a secret.
///
/// Unix only, like the rest of the file-backed half of this tree: `MmapPager` is already
/// `#[cfg(unix)]`, so there is no platform where this check is skipped and a database is
/// nevertheless being served from a file.
#[cfg(unix)]
fn check_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is mode {mode:04o}; a token file must not be readable by anyone else - \
                 run `chmod 600 {}`",
                path.display(),
                path.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_permissions(_: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A token file at mode 600, which is the only mode `from_file` accepts.
    fn token_file(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        drop(f);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        (dir, path)
    }

    #[test]
    fn disabled_allows_everything() {
        let a = Auth::disabled();
        assert_eq!(a.authorize(None, Role::Admin), Outcome::Allowed(None));
        assert!(!a.is_enabled());
    }

    #[test]
    fn a_role_contains_the_ones_below_it() {
        let (_d, p) = token_file("secret admin\nro read\n");
        let a = Auth::from_file(&p).unwrap();
        assert_eq!(a.authorize(Some("secret"), Role::Read), Outcome::Allowed(Some(Role::Admin)));
        assert_eq!(a.authorize(Some("secret"), Role::Admin), Outcome::Allowed(Some(Role::Admin)));
        assert_eq!(a.authorize(Some("ro"), Role::Read), Outcome::Allowed(Some(Role::Read)));
    }

    #[test]
    fn a_short_role_is_forbidden_not_unauthenticated() {
        let (_d, p) = token_file("ro read\n");
        let a = Auth::from_file(&p).unwrap();
        assert_eq!(
            a.authorize(Some("ro"), Role::Write),
            Outcome::Forbidden { held: Role::Read, needed: Role::Write },
            "a known token that does not reach far enough is a 403, not a 401 - retrying with \
             the same credential will never work and the client should be told so"
        );
    }

    #[test]
    fn an_unknown_or_absent_token_is_unauthenticated() {
        let (_d, p) = token_file("ro read\n");
        let a = Auth::from_file(&p).unwrap();
        assert_eq!(a.authorize(None, Role::Read), Outcome::Unauthenticated);
        assert_eq!(a.authorize(Some("guess"), Role::Read), Outcome::Unauthenticated);
        // A prefix of a real token must not pass.
        assert_eq!(a.authorize(Some("r"), Role::Read), Outcome::Unauthenticated);
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let (_d, p) = token_file("# admins\n\n  secret admin  \n\nro read # read only\n");
        let a = Auth::from_file(&p).unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a.authorize(Some("ro"), Role::Read), Outcome::Allowed(Some(Role::Read)));
    }

    #[test]
    fn a_file_with_no_tokens_is_refused() {
        let (_d, p) = token_file("# nothing here\n");
        let e = Auth::from_file(&p).unwrap_err();
        assert!(e.to_string().contains("no tokens"), "{e}");
    }

    #[test]
    fn an_unknown_role_is_refused_by_name() {
        let (_d, p) = token_file("secret superuser\n");
        let e = Auth::from_file(&p).unwrap_err();
        assert!(e.to_string().contains("superuser"), "{e}");
    }

    #[test]
    #[cfg(unix)]
    fn a_world_readable_token_file_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, p) = token_file("secret admin\n");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = Auth::from_file(&p).unwrap_err();
        assert!(e.to_string().contains("chmod 600"), "{e}");
    }

    #[test]
    fn constant_time_eq_still_compares_correctly() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }
}
