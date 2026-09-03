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

//! The statements that change who may do what.
//!
//! **Four statements, and deliberately no fifth about people.** `CREATE USER` is refused by name
//! with what to do instead: a credential is a line in a file the server reads off its own disk,
//! and a route that could write one would let an `admin` password rewrite the password file over
//! the network. So this surface administers *roles* - what may be done - and the users file says
//! who holds one.
//!
//! What a privilege means is `big-rbac`'s, in a crate that links no storage; what a statement
//! *demands* is [`crate::Sql::demands`], next to the variants it is about. This file is only the
//! shape of the four statements that change the grants.

use big_rbac::{Object, Privileges};

/// One statement that changes a role or what it holds.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Acl {
    /// `CREATE ROLE [IF NOT EXISTS] <name>`.
    CreateRole {
        name: String,
        /// A role already there is a request already satisfied, and its grants are untouched.
        if_not_exists: bool,
    },
    /// `DROP ROLE [IF EXISTS] <name>`.
    ///
    /// Takes every grant the role held with it. Whoever the users file gave that name now holds
    /// one that resolves to nothing, which is no privileges at all - the fail-closed direction,
    /// and the only one available, because this layer cannot see a users file.
    DropRole { name: String, if_exists: bool },
    /// `GRANT <privileges> ON <object> TO <role>`.
    Grant { privileges: Privileges, on: AclObject, role: String },
    /// `REVOKE <privileges> ON <object> FROM <role>`.
    ///
    /// Removes a grant rather than adding a denial. There is no negative grant here: privileges
    /// at the three levels are unioned, so a rule that subtracted would make the answer depend on
    /// the order the levels were consulted in.
    Revoke { privileges: Privileges, on: AclObject, role: String },
}

/// What a `GRANT` is about, before the request's database has been filled in.
///
/// The `Option` is the whole difference from [`big_rbac::Object`], and it is the same shape
/// [`crate::Ddl`] uses: a name the statement did not qualify is not yet decided, and what decides
/// it is the request rather than the text. [`AclObject::resolved`] is the conversion, once
/// [`AclObject::fill_database`] has run.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum AclObject {
    /// `ON *.*` - the server itself.
    ///
    /// Never filled in, the carve-out `CREATE DATABASE` gets for the same reason: it does not
    /// name something *inside* a database, so a request's database has nothing to say about it.
    Server,
    /// `ON <database>.*`, or a bare `ON *` meaning the request's database.
    Database(Option<String>),
    /// `ON <database>.<table>`, or a bare `ON <table>`.
    Table { database: Option<String>, table: String },
}

impl AclObject {
    /// Fills in the database this object did not name. See [`crate::translate_in`].
    pub fn fill_database(&mut self, database: &str) {
        match self {
            Self::Database(d) | Self::Table { database: d, .. } => {
                d.get_or_insert_with(|| database.to_string());
            }
            Self::Server => {}
        }
    }

    /// The resolved object, with [`crate::DEFAULT_DATABASE`] standing in for a name that was
    /// never filled.
    ///
    /// The fallback rather than a panic: `fill_database` returns early when the request is
    /// already against the default database, so an unfilled name here means exactly that.
    pub fn resolved(&self) -> Object {
        let or_default =
            |d: &Option<String>| d.clone().unwrap_or_else(|| crate::DEFAULT_DATABASE.to_string());
        match self {
            Self::Server => Object::Server,
            Self::Database(d) => Object::Database(or_default(d)),
            Self::Table { database, table } => {
                Object::Table { database: or_default(database), table: table.clone() }
            }
        }
    }

    /// How it was written, for an `EXPLAIN` and for the sentence in a refusal.
    pub fn written(&self) -> String {
        match self {
            Self::Server => "*.*".to_string(),
            Self::Database(d) => format!("{}.*", d.as_deref().unwrap_or("*")),
            Self::Table { database, table } => match database {
                Some(d) => format!("{d}.{table}"),
                None => table.clone(),
            },
        }
    }
}

impl Acl {
    /// Fills in the database a `GRANT` did not name.
    ///
    /// A role is not in a database, so `CREATE ROLE` and `DROP ROLE` are untouched: roles are
    /// server-wide, which is what lets one grant reach across two databases.
    pub fn fill_database(&mut self, database: &str) {
        match self {
            Self::Grant { on, .. } | Self::Revoke { on, .. } => on.fill_database(database),
            Self::CreateRole { .. } | Self::DropRole { .. } => {}
        }
    }

    /// The role this statement is about, which is the one name every form of it carries.
    pub fn role(&self) -> &str {
        match self {
            Self::CreateRole { name, .. } | Self::DropRole { name, .. } => name,
            Self::Grant { role, .. } | Self::Revoke { role, .. } => role,
        }
    }
}
