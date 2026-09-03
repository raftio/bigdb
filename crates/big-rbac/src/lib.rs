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

//! Who may do what, and nothing about who anybody is.
//!
//! **Three things live here and they are deliberately three.** A [`Demand`] is what a statement
//! needs - `big-sql` produces those, next to the statements they are about. A [`Grants`] store is
//! what roles have been given. [`Grants::allows`] is the decision, and it is a lookup over
//! integers with no I/O in it, because it runs once per statement on a path that must never grow
//! a second password hash.
//!
//! **What this crate has never heard of is a user.** A credential is a line in a file the server
//! reads, checked at the edge, and the only thing that crosses the gap is a role *name*. That is
//! what lets the same catalog serve a deployment whose accounts live somewhere no part of this
//! engine can see - and it is why [`Grants`] can be a pure data structure with no clock, no
//! socket and no notion of authentication at all.
//!
//! The store is held by `big-db`'s catalog and encoded into the same record chain the schema
//! uses, the way `big-keys` holds row keys there. Where they are kept is that crate's; what they
//! mean is this one's.

use std::collections::BTreeMap;

/// A named set of privileges. Not a person: see the module note.
pub type RoleId = u32;
/// Mirrors `big_db::catalog::DatabaseId`. Spelled again rather than imported, because importing
/// it would mean this crate depending on storage to name a `u32`.
pub type DatabaseId = u32;
/// Mirrors `big_db::catalog::TableId`, for the reason [`DatabaseId`] is spelled here.
pub type TableId = u32;

/// A grant's database word when the grant is about every database.
///
/// The top of the range rather than a flag byte: a sentinel the id allocator can never reach
/// costs no space and cannot be mistaken for a real id. `big-db`'s `intern_database` is held off
/// it, and that guard is what this constant depends on.
pub const ANY_DATABASE: DatabaseId = u32::MAX;
/// A grant's table word when the grant is about every table in its database.
pub const ANY_TABLE: TableId = u32::MAX;

/// The role that holds everything, exists without being created, and cannot be dropped.
///
/// **Reserved by name and never stored.** Privileges come from grants, and a grant has to be made
/// by somebody who already holds the privilege to make one - so a database whose catalog is empty
/// has nobody who can write the first `GRANT`, and no amount of correct code inside the engine
/// breaks that circle. A role that is true before anything is written breaks it from outside.
///
/// Spelled the same in the users file and in a refusal, so a grep for a role in the log finds the
/// line in the file that granted it.
pub const SUPERUSER: &str = "superuser";

/// How many roles a store may hold.
///
/// Bounded for the reason [`MAX_GRANTS`] is. Sixty-four is more distinct answers to "who is this"
/// than a deployment with one users file has people to give them to.
pub const MAX_ROLES: usize = 64;

/// How many grants a store may hold, across every role.
///
/// **Bounded because this store rides in the catalog, and the catalog is re-encoded on every
/// commit that dirties it - including ones that only wrote data.** Fragment metadata lives in the
/// same chain and moves whenever a bit depth widens or a shard's min/max shifts, so an import
/// dirties the catalog and rewrites every grant record with it. That is the cost this ceiling
/// exists to bound, and it is not the one a reader expects.
///
/// 256 records is 32 KiB. For scale, one table of twenty fields across ten shards is about two
/// hundred fragment records already, so a realistic policy of a few dozen grants is noise beside
/// what the chain carries anyway - it is the ceiling, not the typical case, that had to be kept
/// off the same order of magnitude.
///
/// Grants are dense, which is what makes 256 generous: a role with the run of a database is one
/// entry, not one per table in it. If it ever binds, the fix is not a bigger number - it is to
/// give this store its own page chain and its own dirty flag, which the meta page has room for
/// and which would take data commits out of the picture entirely.
pub const MAX_GRANTS: usize = 256;

/// One thing a role may be allowed to do.
///
/// **Verbs, not statements.** Several statements demand `Select`, and one statement can demand
/// more than one of these; the mapping from a statement to the set it needs is `big-sql`'s, where
/// the statements are. Two of these are never demanded by any statement - see [`Privilege::Roles`]
/// and [`Privilege::Operate`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Privilege {
    /// Read rows: `SELECT`, and the `SHOW`/`DESCRIBE` that name the same object.
    Select,
    /// Write rows.
    Insert,
    /// Remove rows.
    ///
    /// Separate from [`Privilege::Insert`] because deleting is reachable without inserting - the
    /// REST surface has a route for it - and a credential that may add facts is not obviously one
    /// that may remove them.
    Delete,
    /// Bring an object into being: a database, a table, a view, a column.
    Create,
    /// Remove one.
    ///
    /// Never folded into [`Privilege::Alter`]: dropping a table destroys the data in it, and
    /// altering one does not.
    Drop,
    /// Change the shape of an object that stays.
    Alter,
    /// Administer roles: `CREATE ROLE`, `DROP ROLE`, `GRANT`, `REVOKE`.
    ///
    /// Only ever held on [`Object::Server`]. A role that could hand out privileges within one
    /// database would still be handing out the privilege to hand them out, and the fence would
    /// not hold for more than one hop.
    Roles,
    /// Operate the server: metrics, repair, backup.
    ///
    /// **Demanded by no statement.** It guards routes rather than SQL, which is why it is not
    /// grantable in a `GRANT` today: there is no statement whose refusal would explain it.
    Operate,
}

impl Privilege {
    /// Every privilege, so a `GRANT ALL` and a listing cannot fall behind the enum.
    pub const ALL: [Self; 8] = [
        Self::Select,
        Self::Insert,
        Self::Delete,
        Self::Create,
        Self::Drop,
        Self::Alter,
        Self::Roles,
        Self::Operate,
    ];

    /// The bit this privilege occupies. Part of the on-disk format: a stored grant is these bits
    /// and nothing else, so the numbers may never be reordered.
    pub const fn bit(self) -> u32 {
        1 << (self as u32)
    }

    /// The name a `GRANT` spells it with, and the one a refusal prints.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Select => "SELECT",
            Self::Insert => "INSERT",
            Self::Delete => "DELETE",
            Self::Create => "CREATE",
            Self::Drop => "DROP",
            Self::Alter => "ALTER",
            Self::Roles => "ROLES",
            Self::Operate => "OPERATE",
        }
    }

    /// Parses a privilege by the name a `GRANT` spells it with. Case-insensitive, because the
    /// lexer that will hand these over does not fold keywords.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str().eq_ignore_ascii_case(s))
    }

    /// Whether this privilege means anything at the level of a whole table.
    ///
    /// [`Privilege::Create`] does not: creating a table that already exists is not a thing to be
    /// allowed. [`Privilege::Roles`] and [`Privilege::Operate`] are about the server and mean
    /// nothing inside a database either. Enforced when a `GRANT` is parsed rather than when it is
    /// resolved, so the decision below stays one uniform lookup.
    pub const fn grantable_on_table(self) -> bool {
        matches!(self, Self::Select | Self::Insert | Self::Delete | Self::Drop | Self::Alter)
    }

    /// Whether this privilege means anything at the level of a database.
    pub const fn grantable_on_database(self) -> bool {
        !matches!(self, Self::Roles | Self::Operate)
    }

    /// Whether a `GRANT` may name this privilege at all.
    ///
    /// [`Privilege::Operate`] is the one that may not, and only because nothing demands it: it
    /// guards routes rather than statements, so a `GRANT OPERATE` would be a privilege whose
    /// refusal no statement could ever explain. Adding it later is additive - the bit is already
    /// reserved and already stored.
    pub const fn grantable_on_server(self) -> bool {
        !matches!(self, Self::Operate)
    }

    /// Whether this privilege may be granted on `object`.
    pub fn grantable_on(self, object: &Object) -> bool {
        match object {
            Object::Server => self.grantable_on_server(),
            Object::Database(_) => self.grantable_on_database(),
            Object::Table { .. } => self.grantable_on_table(),
        }
    }
}

/// A set of privileges, as the `u32` a catalog record carries.
///
/// **Never masked to the bits this build knows.** A store read from a file a newer build wrote
/// carries bits with no name here; refusing to *act* on them is right, and dropping them is not,
/// because what is read is what gets written back on the next schema commit. A downgrade must not
/// silently revoke what it merely could not interpret. See [`Privileges::known`] for the mask
/// that is applied at the decision instead.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Privileges(pub u32);

impl Privileges {
    /// Every privilege this build has a name for. What `GRANT ALL` means, and the mask a
    /// decision is taken through.
    pub fn known() -> Self {
        Self(Privilege::ALL.iter().fold(0, |acc, p| acc | p.bit()))
    }

    /// What `GRANT ALL` means on one object, which is level-dependent.
    ///
    /// **`ALL` is "everything grantable here", not "every bit".** `GRANT ALL ON sales.*` must not
    /// hand out [`Privilege::Roles`] - that is held on the server or nowhere, or the fence it
    /// draws would not survive one hop - and `GRANT ALL ON sales.orders` must not hand out
    /// [`Privilege::Create`], because there is nothing left to create once the table is there.
    pub fn all_on(object: &Object) -> Self {
        Privilege::ALL.into_iter().filter(|p| p.grantable_on(object)).collect()
    }

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn contains(self, p: Privilege) -> bool {
        self.0 & p.bit() != 0
    }

    #[must_use]
    pub const fn with(self, p: Privilege) -> Self {
        Self(self.0 | p.bit())
    }

    #[must_use]
    pub const fn without(self, p: Privilege) -> Self {
        Self(self.0 & !p.bit())
    }

    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    #[must_use]
    pub const fn minus(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// The privileges in this set that this build can name, in enum order. What `SHOW GRANTS`
    /// prints - a bit with no name is not printable and is not this build's to describe.
    pub fn named(self) -> impl Iterator<Item = Privilege> {
        Privilege::ALL.into_iter().filter(move |p| self.contains(*p))
    }
}

impl FromIterator<Privilege> for Privileges {
    fn from_iter<I: IntoIterator<Item = Privilege>>(iter: I) -> Self {
        Self(iter.into_iter().fold(0, |acc, p| acc | p.bit()))
    }
}

/// What a grant or a demand is *about*, owned.
///
/// Three levels and no fourth. There is no column here and no row predicate, and that is a
/// decision rather than an omission: this surface answers questions over whole tables, and a
/// privilege finer than the answer would be a fence somebody could walk around by asking a
/// slightly different question.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Object {
    /// The server itself - what `ON *.*` names. Where [`Privilege::Roles`] and
    /// [`Privilege::Operate`] are held, and where `CREATE DATABASE` is demanded, since the thing
    /// being created is not in a database yet.
    Server,
    /// Every table in one database: `ON db.*`.
    Database(String),
    /// One table: `ON db.tbl`.
    Table { database: String, table: String },
}

/// [`Object`] as it is produced by a statement, borrowing the names in the statement's own AST.
///
/// Borrowed because a demand is made and answered within one statement and never stored; owning
/// the names would allocate twice per table on a path that runs once per query.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObjectRef<'a> {
    Server,
    Database(&'a str),
    Table { database: &'a str, table: &'a str },
}

impl<'a> ObjectRef<'a> {
    pub fn to_owned(self) -> Object {
        match self {
            Self::Server => Object::Server,
            Self::Database(d) => Object::Database(d.to_string()),
            Self::Table { database, table } => {
                Object::Table { database: database.to_string(), table: table.to_string() }
            }
        }
    }

    /// The database this object is in, if it is in one.
    pub fn database(self) -> Option<&'a str> {
        match self {
            Self::Server => None,
            Self::Database(d) | Self::Table { database: d, .. } => Some(d),
        }
    }
}

/// One privilege on one object, which is the unit a statement asks in.
///
/// A statement produces a list of these and every one of them has to be held - they are an `AND`,
/// never an `OR`. A join needs `Select` on both tables, and holding it on one is not most of the
/// way to being allowed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Demand<'a> {
    pub privilege: Privilege,
    pub on: ObjectRef<'a>,
}

impl<'a> Demand<'a> {
    pub fn new(privilege: Privilege, on: ObjectRef<'a>) -> Self {
        Self { privilege, on }
    }

    pub fn server(privilege: Privilege) -> Self {
        Self { privilege, on: ObjectRef::Server }
    }

    pub fn database(privilege: Privilege, database: &'a str) -> Self {
        Self { privilege, on: ObjectRef::Database(database) }
    }

    pub fn table(privilege: Privilege, database: &'a str, table: &'a str) -> Self {
        Self { privilege, on: ObjectRef::Table { database, table } }
    }
}

/// A role, as it is listed.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct RoleDef {
    pub id: RoleId,
    pub name: String,
}

/// Which role, on which object. The key a stored grant is filed under.
///
/// Ids rather than names, and that is what makes a grant vanish with the object it is about: ids
/// are never reissued, so a table dropped and recreated under the same name gets a new one and
/// cannot inherit what the old one carried.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct GrantKey {
    pub role: RoleId,
    pub database: DatabaseId,
    pub table: TableId,
}

impl GrantKey {
    /// The key for `ON *.*`.
    pub const fn server(role: RoleId) -> Self {
        Self { role, database: ANY_DATABASE, table: ANY_TABLE }
    }

    /// The key for `ON db.*`.
    pub const fn database(role: RoleId, database: DatabaseId) -> Self {
        Self { role, database, table: ANY_TABLE }
    }

    /// The key for `ON db.tbl`.
    pub const fn table(role: RoleId, database: DatabaseId, table: TableId) -> Self {
        Self { role, database, table }
    }
}

/// Why a change to the store was refused.
///
/// Deliberately not an error about *authorization* - a refused decision is [`Denied`], and the
/// two are different enough that sharing a type would let a caller answer one with the other's
/// status code.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RbacError {
    /// A name no role has. Also what `GRANT ... TO alice` gets when `alice` is a person: this
    /// crate cannot tell, and deliberately does not try - consulting the edge's users file would
    /// turn `GRANT` into a way to ask whether an account exists.
    UnknownRole(String),
    /// [`SUPERUSER`], which cannot be made, dropped, or granted to.
    ReservedRole(String),
    TooManyRoles {
        max: usize,
    },
    TooManyGrants {
        max: usize,
    },
    /// A role id allocator that reached [`ANY_DATABASE`]'s value. Unreachable in practice at
    /// [`MAX_ROLES`], and refused rather than trusted to stay that way.
    RoleIdsExhausted,
}

impl core::fmt::Display for RbacError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnknownRole(n) => write!(
                f,
                "there is no role `{n}`; roles are made with `CREATE ROLE`, and a person is given \
                 one by the third column of the server's users file - `GRANT ... TO` never names \
                 a person"
            ),
            Self::ReservedRole(n) => write!(
                f,
                "`{n}` is the reserved role that holds everything; it exists without being \
                 created and cannot be changed, because it is what an empty catalog is recovered \
                 through"
            ),
            Self::TooManyRoles { max } => write!(f, "a catalog holds at most {max} roles"),
            Self::TooManyGrants { max } => write!(
                f,
                "a catalog holds at most {max} grants; grant on a whole database rather than on \
                 each table in it"
            ),
            Self::RoleIdsExhausted => write!(f, "no role id is left to hand out"),
        }
    }
}

impl std::error::Error for RbacError {}

impl RbacError {
    /// The stable string a client matches on, in the same shape the rest of the engine's codes
    /// take.
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownRole(_) => "unknown_role",
            Self::ReservedRole(_) => "reserved_role",
            Self::TooManyRoles { .. } => "too_many_roles",
            Self::TooManyGrants { .. } => "too_many_grants",
            Self::RoleIdsExhausted => "role_ids_exhausted",
        }
    }
}

/// A refused decision: what was needed, and who did not have it.
///
/// Carries the object as a name rather than as an id, because the only thing that ever reads this
/// is a person reading a `403`, and an id would mean nothing to them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Denied {
    /// The role that was refused. `None` when the caller held no role at all - a users file
    /// naming one the catalog does not have.
    pub role: Option<String>,
    pub privilege: Privilege,
    pub on: Object,
}

impl core::fmt::Display for Denied {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let what = self.privilege.as_str();
        let where_ = match &self.on {
            Object::Server => "the server".to_string(),
            Object::Database(d) => format!("`{d}`"),
            Object::Table { database, table } => format!("`{database}.{table}`"),
        };
        match &self.role {
            Some(role) => write!(f, "role `{role}` does not hold {what} on {where_}"),
            None => write!(
                f,
                "no role holds {what} on {where_}: this credential names a role the catalog does \
                 not have, which is every privilege withheld rather than any granted"
            ),
        }
    }
}

/// Who is asking, as far as this crate is concerned.
///
/// **Not `Default`, on purpose.** A default would have to be one of these, and either choice is
/// wrong in a way nothing would catch: defaulting to [`Who::Trusted`] makes every caller that
/// forgot to say fail *open*, and defaulting to a role makes internal machinery fail closed
/// against work it has already authorised. Every call site says which, and the compiler asks.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Who {
    /// The check is somebody else's, already done, or not wanted: a peer applying a change the
    /// leader ruled on, an embedder with no credentials at all, or a server started without a
    /// users file. Allows everything.
    Trusted,
    /// A person holding a role by name. [`SUPERUSER`] allows everything; a name the store does
    /// not have allows nothing.
    Role(String),
}

impl Who {
    /// Whether this is the reserved role.
    pub fn is_superuser(&self) -> bool {
        matches!(self, Self::Role(name) if name == SUPERUSER)
    }
}

/// Every role, and what each has been granted.
///
/// A plain data structure: no clock, no lock, no I/O. It is held inside `big-db`'s catalog and
/// encoded into the same record chain, which is why it exposes its contents by iteration rather
/// than keeping them private to itself.
#[derive(Clone, Default, Debug)]
pub struct Grants {
    /// Named roles only. [`SUPERUSER`] is not in here: it is true before anything is written, so
    /// storing it would leave every reload deciding whether to put it back.
    roles: BTreeMap<RoleId, String>,
    role_ids: BTreeMap<String, RoleId>,
    /// The raw mask per object, exactly as it was read. See [`Privileges`] for why it is not
    /// narrowed on the way in.
    grants: BTreeMap<GrantKey, u32>,
    /// Ids only ever go up, which is what stops a remade role inheriting a dropped one's grants.
    next_role: RoleId,
}

impl Grants {
    pub fn new() -> Self {
        Self::default()
    }

    // ---- roles -------------------------------------------------------------------------

    /// The id of a role by name.
    ///
    /// [`SUPERUSER`] is deliberately not found here. It is not a row, so it has no id, and a
    /// caller that resolved it to one could grant *to* it - which must stay impossible, because a
    /// role nobody can narrow is what the bootstrap rests on.
    pub fn role(&self, name: &str) -> Option<RoleId> {
        self.role_ids.get(name).copied()
    }

    pub fn role_name(&self, id: RoleId) -> Option<&str> {
        self.roles.get(&id).map(String::as_str)
    }

    pub fn role_count(&self) -> usize {
        self.roles.len()
    }

    pub fn grant_count(&self) -> usize {
        self.grants.len()
    }

    /// Every role, in name order. [`SUPERUSER`] is not in it: a listing prepends what is true
    /// without being stored, and that is the lister's job rather than this one's.
    pub fn roles(&self) -> impl Iterator<Item = RoleDef> + '_ {
        self.role_ids.iter().map(|(name, id)| RoleDef { id: *id, name: name.clone() })
    }

    /// Makes a role, returning the existing id if it is already there.
    ///
    /// Idempotent on a repeat, which is what `IF NOT EXISTS` wants and which costs nothing: a
    /// role carries no declaration beyond its name, so there is no second one to contradict, and
    /// a repeat leaves its grants exactly as they were.
    pub fn intern_role(&mut self, name: &str) -> Result<RoleId, RbacError> {
        if name == SUPERUSER {
            return Err(RbacError::ReservedRole(name.to_string()));
        }
        if let Some(id) = self.role(name) {
            return Ok(id);
        }
        if self.roles.len() >= MAX_ROLES {
            return Err(RbacError::TooManyRoles { max: MAX_ROLES });
        }
        let id = self.next_role;
        // The sentinels are `u32::MAX` in the two words beside this one. A role id that could
        // collide with one is a trap laid for the next widening, so the allocator stops short.
        if id == u32::MAX {
            return Err(RbacError::RoleIdsExhausted);
        }
        self.next_role += 1;
        self.roles.insert(id, name.to_string());
        self.role_ids.insert(name.to_string(), id);
        Ok(id)
    }

    /// Removes a role and every grant it held. `false` means there was no such role.
    ///
    /// **A users file naming this role is not consulted, because it cannot be.** Credentials live
    /// at the edge, so dropping a role people hold leaves them holding a name that resolves to
    /// nothing - which is no privileges at all, the fail-closed direction, and the reason
    /// [`SUPERUSER`] refuses to be dropped.
    pub fn drop_role(&mut self, name: &str) -> Result<bool, RbacError> {
        if name == SUPERUSER {
            return Err(RbacError::ReservedRole(name.to_string()));
        }
        let Some(id) = self.role_ids.remove(name) else { return Ok(false) };
        self.roles.remove(&id);
        self.grants.retain(|k, _| k.role != id);
        Ok(true)
    }

    // ---- grants ------------------------------------------------------------------------

    /// Sets one role's privileges on one object to exactly `privileges`.
    ///
    /// **Absolute, not a delta, and that is what makes it safe to replicate.** `GRANT` and
    /// `REVOKE` are each a read of the current mask followed by one of these, computed once by
    /// whoever ruled the statement legal; what travels to another node is the answer rather than
    /// the arithmetic. A peer cannot reach a different result from its own state, and applying it
    /// twice is applying it once.
    ///
    /// An empty mask removes the record rather than storing a grant that grants nothing.
    pub fn set(&mut self, key: GrantKey, privileges: Privileges) -> Result<(), RbacError> {
        if privileges.is_empty() {
            self.grants.remove(&key);
            return Ok(());
        }
        if !self.grants.contains_key(&key) && self.grants.len() >= MAX_GRANTS {
            return Err(RbacError::TooManyGrants { max: MAX_GRANTS });
        }
        self.grants.insert(key, privileges.0);
        Ok(())
    }

    /// The raw mask stored for exactly this object, with no widening applied.
    pub fn get(&self, key: GrantKey) -> Privileges {
        Privileges(self.grants.get(&key).copied().unwrap_or(0))
    }

    /// Every grant a role holds, in object order. What `SHOW GRANTS` lists.
    pub fn of(&self, role: RoleId) -> impl Iterator<Item = (GrantKey, Privileges)> + '_ {
        self.grants
            .range(GrantKey { role, database: 0, table: 0 }..)
            .take_while(move |(k, _)| k.role == role)
            .map(|(k, v)| (*k, Privileges(*v)))
    }

    /// Every grant, for the encoder. In key order, so a file's records do not move about between
    /// commits that changed nothing.
    pub fn all(&self) -> impl Iterator<Item = (GrantKey, Privileges)> + '_ {
        self.grants.iter().map(|(k, v)| (*k, Privileges(*v)))
    }

    /// Every role as an id and a name, for the encoder.
    pub fn all_roles(&self) -> impl Iterator<Item = (RoleId, &str)> + '_ {
        self.roles.iter().map(|(id, name)| (*id, name.as_str()))
    }

    /// The next role id this store would hand out. Persisted, so a reload cannot reissue one.
    pub fn next_role_id(&self) -> RoleId {
        self.next_role
    }

    // ---- rebuilding from a file --------------------------------------------------------

    /// Puts back a role read from a record. For the decoder only.
    ///
    /// Skips [`SUPERUSER`]: it is never written, so a record claiming it came from somewhere
    /// else, and interning it would shadow the built-in that holds everything - the one name
    /// where being shadowed is a way in rather than a wrong answer.
    pub fn restore_role(&mut self, id: RoleId, name: &str) {
        if name == SUPERUSER {
            return;
        }
        self.role_ids.insert(name.to_string(), id);
        self.roles.insert(id, name.to_string());
    }

    /// Puts back a grant read from a record, if its role is one this store has.
    ///
    /// `false` means it was dropped for want of a role. Fail-closed: a privilege attached to
    /// nobody can never be exercised, and keeping it would leave the chain carrying a grant no
    /// listing could explain. The caller stages these and replays them once every role record has
    /// gone past, because nothing orders the chain.
    pub fn restore_grant(&mut self, key: GrantKey, privileges: Privileges) -> bool {
        if privileges.is_empty() || !self.roles.contains_key(&key.role) {
            return false;
        }
        self.grants.insert(key, privileges.0);
        true
    }

    /// Raises the id high-water mark to at least `value`. For the decoder.
    ///
    /// Taking the max rather than assigning means a file carrying both a counter record and roles
    /// is governed by whichever is further along, so a hand-edited counter can never hand out an
    /// id already in use.
    pub fn observe_role_id(&mut self, value: RoleId) {
        self.next_role = self.next_role.max(value);
    }

    /// Derives the counter from the roles present, the way a reader of a file written before the
    /// counter existed has to.
    pub fn seal(&mut self) {
        let derived = self.roles.keys().next_back().map_or(0, |m| m + 1);
        self.next_role = self.next_role.max(derived);
    }

    // ---- objects going away ------------------------------------------------------------

    /// Forgets every grant naming this table. Called when the table is dropped.
    pub fn forget_table(&mut self, table: TableId) {
        self.grants.retain(|k, _| k.table != table);
    }

    /// Forgets every grant naming this database, at either level.
    pub fn forget_database(&mut self, database: DatabaseId) {
        self.grants.retain(|k, _| k.database != database);
    }

    // ---- the decision ------------------------------------------------------------------

    /// What a role effectively holds on one object, which is the union of the three keys that
    /// cover it.
    ///
    /// Grants are additive, as in standard SQL: `ON *.*`, `ON db.*` and `ON db.tbl` all apply, and
    /// holding a privilege at any of those levels is holding it. There is no negative grant and
    /// nothing subtracts - `REVOKE` removes a grant rather than adding a denial - which is what
    /// keeps this a union and not an ordering problem.
    ///
    /// Masked to [`Privileges::known`] here and only here: a bit from a newer build is stored
    /// faithfully and never *acted* on.
    pub fn effective(&self, role: RoleId, database: DatabaseId, table: TableId) -> Privileges {
        let mut held = self.get(GrantKey::server(role));
        if database != ANY_DATABASE {
            held = held.union(self.get(GrantKey::database(role, database)));
            if table != ANY_TABLE {
                held = held.union(self.get(GrantKey::table(role, database, table)));
            }
        }
        Privileges(held.0 & Privileges::known().0)
    }

    /// Whether a role holds one privilege on one object.
    ///
    /// The whole decision, and it is a handful of `BTreeMap` lookups over integers. That matters:
    /// it runs once per demand per statement, on the path where re-authenticating would cost a
    /// second argon2 hash - so nothing here may grow into anything but a lookup.
    pub fn allows(
        &self,
        role: RoleId,
        privilege: Privilege,
        database: DatabaseId,
        table: TableId,
    ) -> bool {
        self.effective(role, database, table).contains(privilege)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_privilege_bit_is_stable_and_distinct() {
        let mut seen = 0u32;
        for p in Privilege::ALL {
            assert_eq!(seen & p.bit(), 0, "{p:?} collides with one before it");
            seen |= p.bit();
        }
        assert_eq!(Privileges::known().0, seen);
        // Pinned: these are the on-disk numbers, and reordering the enum would silently move
        // every grant already written.
        assert_eq!(Privilege::Select.bit(), 1);
        assert_eq!(Privilege::Insert.bit(), 2);
        assert_eq!(Privilege::Delete.bit(), 4);
        assert_eq!(Privilege::Operate.bit(), 128);
    }

    #[test]
    fn a_privilege_round_trips_through_its_name() {
        for p in Privilege::ALL {
            assert_eq!(Privilege::parse(p.as_str()), Some(p));
            assert_eq!(Privilege::parse(&p.as_str().to_lowercase()), Some(p));
        }
        assert_eq!(Privilege::parse("EXECUTE"), None);
    }

    /// Grants are additive, so a privilege held at any level covering an object is held on it.
    #[test]
    fn the_three_levels_union() {
        let mut g = Grants::new();
        let r = g.intern_role("analyst").unwrap();
        g.set(GrantKey::server(r), [Privilege::Select].into_iter().collect()).unwrap();
        g.set(GrantKey::database(r, 7), [Privilege::Insert].into_iter().collect()).unwrap();
        g.set(GrantKey::table(r, 7, 3), [Privilege::Drop].into_iter().collect()).unwrap();

        assert!(g.allows(r, Privilege::Select, 7, 3));
        assert!(g.allows(r, Privilege::Insert, 7, 3));
        assert!(g.allows(r, Privilege::Drop, 7, 3));
        // The table-level grant does not leak to a sibling.
        assert!(!g.allows(r, Privilege::Drop, 7, 4));
        // Nor the database-level one to another database.
        assert!(!g.allows(r, Privilege::Insert, 8, 3));
        // But the server-level one reaches everywhere.
        assert!(g.allows(r, Privilege::Select, 8, 4));
    }

    /// A bit this build cannot name is stored, and never acted on. Both halves matter: the first
    /// is what stops a downgrade destroying a grant, the second is what stops it honouring one.
    #[test]
    fn an_unknown_bit_is_kept_but_never_grants_anything() {
        let mut g = Grants::new();
        let r = g.intern_role("analyst").unwrap();
        let future = Privileges(Privilege::Select.bit() | 0x8000_0000);
        g.set(GrantKey::database(r, 7), future).unwrap();

        assert_eq!(g.get(GrantKey::database(r, 7)), future, "stored whole");
        assert_eq!(g.effective(r, 7, ANY_TABLE).0, Privilege::Select.bit(), "acted on narrowed");
    }

    #[test]
    fn a_role_that_was_never_made_holds_nothing() {
        let g = Grants::new();
        assert!(!g.allows(9, Privilege::Select, 7, 3));
        assert!(g.effective(9, 7, 3).is_empty());
    }

    #[test]
    fn the_reserved_role_is_not_a_row() {
        let mut g = Grants::new();
        assert_eq!(g.intern_role(SUPERUSER), Err(RbacError::ReservedRole(SUPERUSER.into())));
        assert_eq!(g.drop_role(SUPERUSER), Err(RbacError::ReservedRole(SUPERUSER.into())));
        assert!(g.role(SUPERUSER).is_none());
        // And a record claiming it is skipped rather than interned.
        g.restore_role(41, SUPERUSER);
        assert!(g.role(SUPERUSER).is_none());
        assert!(Who::Role(SUPERUSER.into()).is_superuser());
    }

    #[test]
    fn a_remade_role_never_reuses_the_dropped_ones_id() {
        let mut g = Grants::new();
        let first = g.intern_role("analyst").unwrap();
        g.set(GrantKey::database(first, 7), [Privilege::Drop].into_iter().collect()).unwrap();
        assert!(g.drop_role("analyst").unwrap());
        assert_eq!(g.grant_count(), 0, "its grants went with it");

        let again = g.intern_role("analyst").unwrap();
        assert_ne!(first, again);
        assert!(!g.allows(again, Privilege::Drop, 7, ANY_TABLE));
    }

    #[test]
    fn remaking_a_role_is_idempotent_and_keeps_its_grants() {
        let mut g = Grants::new();
        let first = g.intern_role("analyst").unwrap();
        g.set(GrantKey::database(first, 7), [Privilege::Select].into_iter().collect()).unwrap();
        assert_eq!(g.intern_role("analyst").unwrap(), first);
        assert!(g.allows(first, Privilege::Select, 7, ANY_TABLE));
    }

    #[test]
    fn an_empty_mask_removes_the_grant_rather_than_storing_nothing() {
        let mut g = Grants::new();
        let r = g.intern_role("analyst").unwrap();
        g.set(GrantKey::database(r, 7), [Privilege::Select].into_iter().collect()).unwrap();
        assert_eq!(g.grant_count(), 1);
        g.set(GrantKey::database(r, 7), Privileges::empty()).unwrap();
        assert_eq!(g.grant_count(), 0);
    }

    #[test]
    fn the_ceilings_hold_and_rewriting_an_existing_grant_is_not_a_new_one() {
        let mut g = Grants::new();
        for i in 0..MAX_ROLES {
            g.intern_role(&format!("r{i}")).unwrap();
        }
        assert_eq!(g.intern_role("more"), Err(RbacError::TooManyRoles { max: MAX_ROLES }));

        let r = g.role("r0").unwrap();
        let one = Privileges::from_iter([Privilege::Select]);
        for i in 0..MAX_GRANTS {
            g.set(GrantKey::database(r, i as DatabaseId), one).unwrap();
        }
        assert_eq!(
            g.set(GrantKey::database(r, 9999), one),
            Err(RbacError::TooManyGrants { max: MAX_GRANTS })
        );
        // A rewrite is not a new record, or `REVOKE` would be refused exactly when the catalog
        // most needed shrinking.
        assert!(g.set(GrantKey::database(r, 0), Privileges::from_iter([Privilege::Drop])).is_ok());
        assert!(g.set(GrantKey::database(r, 0), Privileges::empty()).is_ok());
    }

    #[test]
    fn an_object_going_away_takes_its_grants() {
        let mut g = Grants::new();
        let r = g.intern_role("analyst").unwrap();
        let one = Privileges::from_iter([Privilege::Select]);
        g.set(GrantKey::table(r, 7, 3), one).unwrap();
        g.set(GrantKey::database(r, 7), one).unwrap();
        g.set(GrantKey::database(r, 8), one).unwrap();

        g.forget_table(3);
        assert_eq!(g.grant_count(), 2);
        g.forget_database(7);
        assert_eq!(g.grant_count(), 1);
        assert!(g.allows(r, Privilege::Select, 8, ANY_TABLE));
    }

    /// A grant read before its role is an ordinary file, not a corrupt one, so the caller stages
    /// them - and one whose role never turns up is dropped rather than kept as an orphan.
    #[test]
    fn a_restored_grant_needs_its_role_to_be_there_first() {
        let mut g = Grants::new();
        let one = Privileges::from_iter([Privilege::Select]);
        assert!(!g.restore_grant(GrantKey::database(0, 7), one), "no role yet");
        g.restore_role(0, "analyst");
        assert!(g.restore_grant(GrantKey::database(0, 7), one));
        assert!(g.allows(0, Privilege::Select, 7, ANY_TABLE));
    }

    /// Ids only ever go up, across a reload as well as within one.
    #[test]
    fn the_counter_survives_being_derived_or_read() {
        let mut g = Grants::new();
        g.restore_role(4, "analyst");
        g.seal();
        assert_eq!(g.intern_role("another").unwrap(), 5);

        let mut g = Grants::new();
        g.restore_role(0, "analyst");
        g.observe_role_id(40);
        g.seal();
        assert_eq!(g.intern_role("another").unwrap(), 40, "the record wins when it is further on");
    }

    #[test]
    fn a_privilege_knows_where_it_may_be_granted() {
        assert!(Privilege::Select.grantable_on_table());
        assert!(Privilege::Select.grantable_on_database());
        // Creating a table that already exists is not a thing to be allowed.
        assert!(!Privilege::Create.grantable_on_table());
        assert!(Privilege::Create.grantable_on_database());
        // Administering roles is about the server and means nothing inside a database.
        assert!(!Privilege::Roles.grantable_on_database());
        assert!(!Privilege::Operate.grantable_on_database());
    }
}
