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

//! Substituting a stored `SELECT` for the name it is kept under.
//!
//! # Why this is here and not in `big-sql`
//!
//! `big-sql` is schema-free on purpose: `translate` takes no catalog, which is why every test in
//! that crate runs at the speed of a parser test. "This name is a view and not a table" is a
//! question only a catalog can answer, so the crate that holds one asks it - between
//! [`big_sql::qualify`] and [`big_sql::finish`], on the **parse tree**.
//!
//! On the parse tree rather than on the lowered calls because a view renames columns and narrows
//! a `WHERE`, and both of those are shapes in [`big_sql::ast`]. After lowering they are query
//! language calls with the names already resolved, and un-resolving them would be undoing work
//! the lowering had just done.
//!
//! # What an expansion is
//!
//! A view is a filter and a projection over one table - the parser guarantees that much, see
//! [`Refused::ViewBody`] - so substituting one is four edits to the [`Select`] that reads it:
//!
//! 1. the base table replaces the view's name, **keeping the view's name as an alias** so a
//!    qualifier like `v.amount` still resolves with nothing rewritten;
//! 2. every column the reader names is remapped through the view's select list, so an exposed
//!    `AS x` becomes the underlying column;
//! 3. a name the view does not expose is refused, which is the point of having a view at all;
//! 4. the two `WHERE`s are `AND`ed.
//!
//! Nothing here reaches storage. A view owns no pages and this module reads no fragment: it is a
//! source-to-source rewrite between two things `big-sql` produced.

use big_db::catalog::{Catalog, SavedQuery, TableRef};
use big_sql::ast::{
    Cond, Having, HavingAgg, HavingOperand, Item, Name, Order, OrderKey, Proj, Query, Select,
    Source,
};
use big_sql::{Parsed, Refused, SqlError};

use crate::error::{ApiError, Result};

/// Rewrites every view a statement reads into the table underneath it.
///
/// A statement naming no view is left byte-for-byte alone, which is the common case and costs
/// one catalog probe per source.
pub fn expand(parsed: &mut Parsed, catalog: &Catalog) -> Result<()> {
    // Matched exhaustively rather than skipped with a `let ... else`, because what was safe to
    // fall through has stopped being one kind of statement. A kind added later fails to compile
    // here rather than losing its views quietly - and the failure that would cause is not a
    // missing answer but a wrong one: a statement whose view was never substituted names a
    // table that is not there, and reports that instead of what is actually wrong.
    match parsed {
        // Only a query reads through a view. An `INSERT` into one is not expanded and is refused
        // where the table is resolved - writing through a view means deciding what the columns it
        // does not expose should hold, and there is no answer to that this engine could invent.
        Parsed::Query(query) => {
            for select in &mut query.branches {
                expand_select(select, catalog)?;
            }
            Ok(())
        }
        // An `EXPLAIN` reads through whatever the statement under it reads through: describing
        // `SELECT ... FROM v` means describing the statement `v` stands for. The parser refuses
        // a second `EXPLAIN`, so this recurses exactly once.
        Parsed::Explain { inner, .. } => expand(inner, catalog),
        // A schema change and a listing name no source to read through.
        Parsed::Insert(_) | Parsed::Show(_) | Parsed::Ddl(_) => Ok(()),
    }
}

/// One branch of a statement: its `FROM`, then each `JOIN`.
fn expand_select(select: &mut Select, catalog: &Catalog) -> Result<()> {
    // The `FROM` first. Its view's filter is unqualified - it is about the statement's own
    // table - so it merges into the `WHERE` as written.
    if let Some(view) = resolve(&select.from, catalog) {
        let body = body_of(&view, catalog, 0)?;
        let exposed = Exposed::of(&body, select.from.label());
        // The reader's names are remapped before the body's filter joins them, so the two sides
        // of the `AND` are already talking about the same columns.
        remap_select(select, &exposed)?;
        select.from = rebase(&select.from, &body.from);
        select.filter = and(body.filter, select.filter.take());
    }
    for i in 0..select.joins.len() {
        let Some(view) = resolve(&select.joins[i].source, catalog) else { continue };
        let body = body_of(&view, catalog, 0)?;
        let exposed = Exposed::of(&body, select.joins[i].source.label());
        // A join's source is named by a label everywhere else in the statement, so the remap
        // covers the whole `Select` again - but only names carrying *this* source's qualifier
        // are touched. See `Exposed::rename`.
        remap_select(select, &exposed)?;
        let label = select.joins[i].source.label().to_string();
        select.joins[i].source = rebase(&select.joins[i].source, &body.from);
        // The body's filter is about the joined table, and a bare name in the outer `WHERE`
        // means the `FROM` table - so every name in it is qualified with the label the join
        // answers to before it is merged. Without this a view joined in would silently filter
        // the wrong side.
        let filter = body.filter.map(|c| qualify_cond(c, &label));
        select.filter = and(filter, select.filter.take());
    }
    Ok(())
}

/// The view a source names, if it names one.
///
/// `None` for an ordinary table, which is the answer for every source in almost every statement.
fn resolve(source: &Source, catalog: &Catalog) -> Option<SavedQuery> {
    let database = source.database.as_deref().unwrap_or(big_db::catalog::DEFAULT_DATABASE_NAME);
    catalog.saved_query(catalog.database(database)?, &source.table).cloned()
}

/// Parses a view's stored statement, expanding any view *it* reads, to a bounded depth.
///
/// **The body is qualified with the view's own database, not the request's.** A view created in
/// `sales` whose body says `FROM orders` means `sales.orders` however the request that reads it
/// was addressed - the view was written by somebody standing in `sales`, and where a reader
/// happens to stand later cannot change what it says.
fn body_of(view: &SavedQuery, catalog: &Catalog, depth: usize) -> Result<Select> {
    if depth >= big_sql::MAX_VIEW_DEPTH {
        return Err(refused(Refused::ViewDepth));
    }
    let mut parsed = big_sql::parse(&view.text)?;
    let database = catalog
        .database_name(view.database)
        .unwrap_or(big_db::catalog::DEFAULT_DATABASE_NAME)
        .to_string();
    big_sql::qualify(&mut parsed, &database);
    // The parser accepted this text at `CREATE VIEW`, so anything else here is a file written
    // by something other than this build. Refused as a body rather than trusted.
    let Parsed::Query(Query { branches }) = parsed else { return Err(refused(Refused::ViewBody)) };
    let [mut body] = <[Select; 1]>::try_from(branches).map_err(|_| refused(Refused::ViewBody))?;
    // A view over a view: the same substitution, one level down, and the depth is what stops it
    // running away on a file this build did not write.
    if let Some(inner) = resolve(&body.from, catalog) {
        let under = body_of(&inner, catalog, depth + 1)?;
        let exposed = Exposed::of(&under, body.from.label());
        remap_select(&mut body, &exposed)?;
        body.from = rebase(&body.from, &under.from);
        body.filter = and(under.filter, body.filter.take());
    }
    Ok(body)
}

/// The base source, wearing the view's name as an alias.
///
/// **This is what makes qualifier rewriting unnecessary.** A statement that wrote `FROM v` may
/// say `v.amount` anywhere, and [`Source::label`] already says an alias replaces the table name.
/// So leaving `v` on as the alias means every one of those qualifiers still resolves, with the
/// substitution invisible to them. An alias the reader wrote themselves wins, because that is
/// the name they went on to use.
fn rebase(outer: &Source, base: &Source) -> Source {
    Source {
        database: base.database.clone(),
        table: base.table.clone(),
        alias: Some(outer.label().to_string()),
    }
}

/// What a view exposes: the name a reader may write, and the column underneath it.
struct Exposed {
    /// Exposed name → underlying column, in the order the body declared them.
    columns: Vec<(String, Name)>,
    /// What a qualifier has to say to mean this view.
    ///
    /// **The reader's label, not the view's name.** A statement writing `FROM big_gb AS g` then
    /// says `g.amount`, so comparing against `big_gb` would decide that name belongs to some
    /// other table and leave it unmapped - which is [`Source::label`]'s rule, that an alias
    /// replaces the name, applied here rather than restated.
    label: String,
}

impl Exposed {
    fn of(body: &Select, label: &str) -> Self {
        let columns = body
            .items
            .iter()
            .filter_map(|item| match &item.proj {
                // The only shape the parser lets through; anything else was refused at `CREATE`.
                Proj::Column(under) => Some((item.column(), under.clone())),
                _ => None,
            })
            .collect();
        Self { columns, label: label.to_string() }
    }

    /// The column a name means, or the refusal.
    ///
    /// A qualifier that is not this view's is left alone: in `FROM tx JOIN v ...` a name written
    /// `tx.amount` is about the other table, and remapping it through this view's select list
    /// would rewrite a column that has nothing to do with it.
    fn rename(&self, name: &mut Name) -> Result<()> {
        if name.qualifier.as_deref().is_some_and(|q| q != self.label) {
            return Ok(());
        }
        let Some((_, under)) = self.columns.iter().find(|(exposed, _)| *exposed == name.column)
        else {
            return Err(refused(Refused::ViewColumn));
        };
        name.column = under.column.clone();
        Ok(())
    }
}

/// Remaps every column a `Select` names through one view's select list.
///
/// Every clause, because a name the view does not expose has to be refused wherever it was
/// written - a `GROUP BY` on a hidden column is the same leak as a `SELECT` of one.
fn remap_select(select: &mut Select, exposed: &Exposed) -> Result<()> {
    // Taken before the items are rewritten: an `ORDER BY` may name a column *or* an alias the
    // select list introduced, and telling those apart needs the names as the reader wrote them.
    let aliases: Vec<String> = select.items.iter().map(|item| item.column()).collect();
    for item in &mut select.items {
        remap_item(item, exposed)?;
    }
    if let Some(cond) = &mut select.filter {
        remap_cond(cond, exposed)?;
    }
    for name in &mut select.group_by {
        exposed.rename(name)?;
    }
    if let Some(having) = &mut select.having {
        remap_having(having, exposed)?;
    }
    if let Some(order) = &mut select.order_by {
        remap_order(order, exposed, &aliases)?;
    }
    // A join's `ON` names a column on each side. Only the one this view is about is remapped,
    // which `Exposed::rename` decides by the qualifier.
    for join in &mut select.joins {
        exposed.rename(&mut join.left)?;
        exposed.rename(&mut join.right)?;
    }
    Ok(())
}

fn remap_item(item: &mut Item, exposed: &Exposed) -> Result<()> {
    match &mut item.proj {
        // A bare column is the one entry whose *output name* is the column's, so renaming it
        // would rename the answer's header too - `SELECT cc FROM v` would come back as
        // `country`, which is the name the view exists to keep out of sight. The name the
        // reader wrote becomes an explicit alias, so the substitution stays invisible in the
        // result as well as in the plan.
        Proj::Column(name) => {
            let wrote = name.column.clone();
            exposed.rename(name)?;
            if item.alias.is_none() && name.column != wrote {
                item.alias = Some(wrote);
            }
        }
        // The rest name a column but are not named *by* it - `sum(x)` comes back as `sum`
        // whichever column it measured - so there is no header to preserve.
        Proj::CountDistinct(name)
        | Proj::Agg { field: name, .. }
        | Proj::Avg(name)
        | Proj::Quantile { field: name, .. }
        | Proj::TopKeys { field: name, .. }
        // A rounded column names a column and is not named by it: `toDate(seen)` comes back as
        // `toDate` whatever the view calls the column underneath.
        | Proj::TimeOf { field: name, .. } => exposed.rename(name)?,
        // `count(*)` names no column, so a view exposing none of them still answers it; and
        // `now()` names nothing at all, so a view with no columns exposed still answers that.
        Proj::Star | Proj::Count | Proj::Now { .. } => {}
    }
    if let Some(cond) = &mut item.filter {
        remap_cond(cond, exposed)?;
    }
    Ok(())
}

/// Puts every column a `HAVING` names through the view, wherever in the tree it sits.
///
/// A walk rather than one match, for the reason the clause is a tree at all: a column the view
/// hides has to be refused in every branch, and a branch that skipped the substitution would
/// read the base table's column through a view that exists to keep it out of sight.
fn remap_having(having: &mut Having, exposed: &Exposed) -> Result<()> {
    match having {
        Having::And(a, b) | Having::Or(a, b) => {
            remap_having(a, exposed)?;
            remap_having(b, exposed)
        }
        Having::Not(a) => remap_having(a, exposed),
        Having::Cmp { left, right, .. } => {
            for side in [left, right] {
                if let HavingOperand::Agg(agg) = side {
                    match agg {
                        HavingAgg::Agg { field, .. } | HavingAgg::Avg(field) => {
                            exposed.rename(field)?
                        }
                        HavingAgg::Count => {}
                    }
                }
            }
            Ok(())
        }
    }
}

fn remap_order(order: &mut Order, exposed: &Exposed, aliases: &[String]) -> Result<()> {
    match &mut order.key {
        OrderKey::Agg { field, .. } | OrderKey::Avg(field) => exposed.rename(field)?,
        // A bare name in `ORDER BY` may be a column or an alias the select list introduced, and
        // which one is the lowering's to decide. **An alias wins**, because that is the rule the
        // lowering applies: a name the statement introduced is not the view's to rename, and
        // renaming it would turn `ORDER BY total` into an order by a column called something
        // else. Anything that is not an alias is a column, so it goes through the view - and a
        // column the view hides is refused here rather than becoming a vaguer refusal later.
        OrderKey::Name(name) => {
            if !aliases.contains(&name.column) {
                exposed.rename(name)?;
            }
        }
        OrderKey::Count => {}
    }
    Ok(())
}

fn remap_cond(cond: &mut Cond, exposed: &Exposed) -> Result<()> {
    match cond {
        Cond::And(a, b) | Cond::Or(a, b) => {
            remap_cond(a, exposed)?;
            remap_cond(b, exposed)
        }
        Cond::Not(a) => remap_cond(a, exposed),
        Cond::Cmp { field, .. } | Cond::In { field, .. } | Cond::Between { field, .. } => {
            exposed.rename(field)
        }
    }
}

/// Puts a qualifier on every bare name in a condition.
///
/// For a view joined in: its stored `WHERE` was written about its own table with nothing
/// qualified, and it is about to be merged into a `WHERE` where a bare name means the `FROM`
/// table instead. A name that already carries a qualifier is left alone - it says which side it
/// is about, and that is still true after the merge.
fn qualify_cond(cond: Cond, label: &str) -> Cond {
    let mut cond = cond;
    fn walk(cond: &mut Cond, label: &str) {
        match cond {
            Cond::And(a, b) | Cond::Or(a, b) => {
                walk(a, label);
                walk(b, label);
            }
            Cond::Not(a) => walk(a, label),
            Cond::Cmp { field, .. } | Cond::In { field, .. } | Cond::Between { field, .. } => {
                field.qualifier.get_or_insert_with(|| label.to_string());
            }
        }
    }
    walk(&mut cond, label);
    cond
}

/// Two filters, either of which may be absent.
fn and(view: Option<Cond>, outer: Option<Cond>) -> Option<Cond> {
    match (view, outer) {
        (Some(v), Some(o)) => Some(Cond::And(Box::new(v), Box::new(o))),
        (Some(c), None) | (None, Some(c)) => Some(c),
        (None, None) => None,
    }
}

/// A refusal from this module, carrying no offset.
///
/// Zero rather than a byte position because the statement these are about is partly the view's
/// and partly the reader's, and pointing into one of the two would point at the wrong text as
/// often as the right one. The sentence says which view.
fn refused(what: Refused) -> ApiError {
    ApiError::Sql(SqlError::Refused { what, at: 0 })
}

/// A view's name, for the caller that resolves a `FROM`.
///
/// Here rather than in [`crate::schema`] because it is about the statement a view holds, and
/// [`crate::TableInfo`] is about the columns a table has.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ViewInfo {
    /// The database the name is unique within - and the one its body resolves in.
    pub database: String,
    /// The name, unqualified. [`ViewInfo::qualified`] is the two together.
    pub name: String,
    /// The `SELECT`, exactly as it was written.
    pub text: String,
}

impl ViewInfo {
    /// The qualified name, which is what a listing and an error message both say.
    pub fn qualified(&self) -> String {
        TableRef::new(&self.database, &self.name).to_string()
    }

    /// The table this view reads, qualified, and the columns it exposes as
    /// `(name the reader writes, column underneath)`.
    ///
    /// For `DESCRIBE v`, which answers with the view's own columns rather than the base table's
    /// - a `DESCRIBE` that listed the columns a view exists to hide would undo the view.
    ///
    /// Parses the stored statement, which is the same thing an expansion does and for the same
    /// reason: the text is what was stored, so the text is what is read back.
    pub fn shape(&self) -> Result<(String, Vec<(String, String)>)> {
        let mut parsed = big_sql::parse(&self.text)?;
        // The view's own database, not any request's - the rule `body_of` states.
        big_sql::qualify(&mut parsed, &self.database);
        let Parsed::Query(q) = &parsed else { return Err(refused(Refused::ViewBody)) };
        let Some(body) = q.branches.first() else { return Err(refused(Refused::ViewBody)) };
        let columns = body
            .items
            .iter()
            .filter_map(|item| match &item.proj {
                Proj::Column(under) => Some((item.column(), under.column.clone())),
                _ => None,
            })
            .collect();
        Ok((body.from.qualified(), columns))
    }
}
