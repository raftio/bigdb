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

//! `GRANT`, `REVOKE`, and the two role statements.
//!
//! The lexer has no keyword list, so none of these words is reserved: a column may still be
//! called `grant` or `role`, and only the leading token position is special. That is the same
//! property `EXPLAIN` relies on, and it is why this grammar cost the lexer nothing.

use super::Parser;
use crate::acl::{Acl, AclObject};
use crate::error::{Refused, Result};
use crate::lex::Tok;
use big_rbac::{Privilege, Privileges};

impl Parser<'_> {
    /// `GRANT <privileges> ON <object> TO <role>`.
    pub(super) fn grant(&mut self) -> Result<Acl> {
        let (privileges, on) = self.privileges_on()?;
        self.expect_word("TO", "TO after the object")?;
        let role = self.grantee()?;
        self.end_of_statement()?;
        Ok(Acl::Grant { privileges, on, role })
    }

    /// `REVOKE <privileges> ON <object> FROM <role>`.
    pub(super) fn revoke(&mut self) -> Result<Acl> {
        let (privileges, on) = self.privileges_on()?;
        self.expect_word("FROM", "FROM after the object")?;
        let role = self.grantee()?;
        self.end_of_statement()?;
        Ok(Acl::Revoke { privileges, on, role })
    }

    /// The half `GRANT` and `REVOKE` share: a privilege list, `ON`, and an object.
    ///
    /// The object is parsed before the list is checked against it, because which privileges are
    /// legal *depends* on the level - `CREATE` means nothing on a table that already exists, and
    /// `ROLES` means nothing anywhere but the server.
    fn privileges_on(&mut self) -> Result<(Privileges, AclObject)> {
        let all = self.word_is("ALL");
        let named = if all {
            self.i += 1;
            // `PRIVILEGES` is the standard's noise word. Accepted and ignored, because it is
            // what somebody writes who learned this surface from the standard.
            self.eat_word("PRIVILEGES");
            Vec::new()
        } else {
            self.privilege_list()?
        };
        self.expect_word("ON", "ON after the privileges")?;
        let on = self.acl_object()?;

        if all {
            return Ok((Privileges::all_on(&on.resolved()), on));
        }
        // Checked here rather than at the resolver, so the rule is a property of the grammar and
        // the decision below stays one uniform lookup with no unrepresentable states in it.
        let object = on.resolved();
        for p in &named {
            if !p.grantable_on(&object) {
                return Err(self.refuse(Refused::AclObject));
            }
        }
        Ok((named.into_iter().collect(), on))
    }

    /// `SELECT, INSERT, ...`, at least one.
    fn privilege_list(&mut self) -> Result<Vec<Privilege>> {
        let mut out = Vec::new();
        loop {
            let Some(word) = self.word() else {
                return Err(self.syntax("a privilege"));
            };
            // A column list after a privilege is the one refusal here that is about a decision
            // rather than a typo: grants are per table, and a fence finer than the answers this
            // surface gives is a fence somebody walks around by asking a different question.
            let word = word.to_string();
            self.i += 1;
            if self.peek() == Some(&Tok::LParen) {
                return Err(self.refuse(Refused::AclColumns));
            }
            match Privilege::parse(&word) {
                Some(p) => out.push(p),
                // `GRANT analyst TO senior` is a role hierarchy, which this surface does not
                // have: the first word is not a privilege, and saying only "unknown privilege"
                // would leave somebody trying to spell it differently.
                None if self.word_is("TO") || self.word_is("FROM") => {
                    return Err(self.refuse(Refused::RoleGrant))
                }
                None => return Err(self.refuse(Refused::AclPrivilege)),
            }
            if !self.eat(&Tok::Comma) {
                return Ok(out);
            }
        }
    }

    /// `*.*`, `<database>.*`, `*`, `<database>.<table>`, or `<table>`.
    fn acl_object(&mut self) -> Result<AclObject> {
        // `*` on its own, or `*.*`. A leading star can only be one of those two.
        if self.eat(&Tok::Star) {
            if !self.eat(&Tok::Dot) {
                // `ON *` is every table in the request's database, which is what a bare name
                // means one level down. Left unfilled here and decided by `fill_database`.
                return Ok(AclObject::Database(None));
            }
            if self.eat(&Tok::Star) {
                return Ok(AclObject::Server);
            }
            // `ON *.orders` has no meaning: a table is only nameable inside a database, so
            // there is nothing for the name to be relative to.
            return Err(self.refuse(Refused::AclObject));
        }
        let first = self.bare_ident("an object to grant on")?;
        if !self.eat(&Tok::Dot) {
            return Ok(AclObject::Table { database: None, table: first });
        }
        if self.eat(&Tok::Star) {
            return Ok(AclObject::Database(Some(first)));
        }
        let table = self.bare_ident("a table name")?;
        Ok(AclObject::Table { database: Some(first), table })
    }

    /// The role a `GRANT` names, and the refusals for the two things that are not one.
    fn grantee(&mut self) -> Result<String> {
        // `TO PUBLIC` is the standard's name for a role everybody holds. There is none here, and
        // inventing one silently would be a grant nobody could see in a listing.
        if self.word_is("PUBLIC") || self.word_is("ALL") {
            return Err(self.refuse(Refused::AclPublic));
        }
        let role = self.bare_ident("a role name")?;
        // `WITH GRANT OPTION` delegates the power to delegate. Refused rather than ignored:
        // accepting it silently would leave somebody believing a grant can be passed on.
        if self.word_is("WITH") {
            return Err(self.refuse(Refused::GrantOption));
        }
        Ok(role)
    }

    /// `CREATE ROLE [IF NOT EXISTS] <name>`, entered on the second word of a `CREATE`.
    pub(super) fn create_role(&mut self) -> Result<Acl> {
        let if_not_exists = self.if_exists(true)?;
        let name = self.role_name()?;
        self.end_of_statement()?;
        Ok(Acl::CreateRole { name, if_not_exists })
    }

    /// `DROP ROLE [IF EXISTS] <name>`, entered on the second word of a `DROP`.
    pub(super) fn drop_role(&mut self) -> Result<Acl> {
        let if_exists = self.if_exists(false)?;
        let name = self.role_name()?;
        self.end_of_statement()?;
        Ok(Acl::DropRole { name, if_exists })
    }

    /// A role name, refusing the one that is reserved.
    ///
    /// Caught at the parser rather than at the store, so `CREATE ROLE superuser` is refused for
    /// the reason it is refused - the name means something already - rather than arriving as a
    /// storage error about a row.
    fn role_name(&mut self) -> Result<String> {
        let name = self.bare_ident("a role name")?;
        if name.eq_ignore_ascii_case(big_rbac::SUPERUSER) {
            return Err(self.refuse(Refused::ReservedRole));
        }
        Ok(name)
    }
}
