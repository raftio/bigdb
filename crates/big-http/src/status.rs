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

//! Which HTTP status an engine error deserves, decided once.
//!
//! Every route used to answer `400` for everything it could not do. That told a client the
//! one thing that is almost never true - "you sent something malformed" - when the real answer
//! was as often "that table does not exist", "the engine is busy", or "this build has a bug".
//! A status is the only part of an error most clients ever look at, so getting it wrong makes
//! every retry policy above this server wrong too.
//!
//! **This is the one place in the tree that knows both HTTP and the whole error tree.** The
//! mapping cannot live lower down: `big-db` would have to learn about status codes to express
//! it, and the layering exists precisely so that it does not.
//!
//! **Anything 5xx is redacted.** A `StoreError::Io` prints the path it failed on, and a
//! filesystem layout is not something a client is owed. The full message goes to the log
//! against the request id, which is in the response, so an operator can still join the two.

use crate::Response;
use big_db::DbError;
use big_embed::ApiError;
use big_engine::bitmap::field::FieldError;
use big_exec::ExecError;
use big_pager::StoreError;
use big_plan::PlanError;
use big_sql::SqlError;

/// An error, resolved into the three things a response needs.
pub struct Failure {
    /// The HTTP status.
    pub status: u16,
    /// The stable identifier a client can branch on. Never changes when a failure moves
    /// between layers.
    pub code: &'static str,
    /// What the client is told. Identical to the log message below 500, generic above it.
    pub message: String,
    /// What the log is told. Always the full message.
    pub detail: String,
}

impl Failure {
    /// Resolves one error into a status, a code, and the two messages.
    pub fn new(e: &ApiError) -> Self {
        let status = status_of(e);
        let detail = e.to_string();
        let message = if status >= 500 {
            // Deliberately says nothing except that the fault is not the caller's, and points
            // at the one thing that lets an operator find the rest.
            "the server could not complete this request; see the server log".to_string()
        } else {
            detail.clone()
        };
        Self { status, code: e.code(), message, detail }
    }

    /// True when the failure is the server's fault and belongs in the log at `error`.
    pub fn is_internal(&self) -> bool {
        self.status >= 500
    }
}

/// Which status an error deserves.
///
/// The distinction that matters is 4xx from 5xx: a client that got the request wrong can fix
/// it, and one that hit a server fault cannot, so retrying is only ever right on one of them.
pub fn status_of(e: &ApiError) -> u16 {
    match e {
        ApiError::Query(ExecError::Plan(p)) => plan(p),
        ApiError::Sql(s) => sql(s),
        ApiError::Query(ExecError::Db(d)) | ApiError::Db(d) => db(d),
        // The statement is well formed and describes real columns; what it asks for costs more
        // than the plan allowed. `422` for the same reason `query_too_large` is: the body is
        // fine and what it describes is not answerable as written.
        ApiError::Query(ExecError::TooManyGroups { .. } | ExecError::TooManyBuckets { .. }) => 422,
        // The client wrote a value its field cannot hold, which is the same 400 an import line
        // with the same mistake gets.
        ApiError::Value(_) => 400,
        // **`403`, and it shadows the `404` a missing table would have got.** The privilege is
        // checked before anything is planned, so a caller who may not read a database and typos
        // a table name in it is told they may not read it rather than that it is not there.
        // That is correct: answering `404` would make this surface a way to ask which tables
        // exist, for somebody with no privilege to know.
        ApiError::Denied(_) => 403,
    }
}

/// A planning failure is always the query the client wrote.
///
/// **`404` for a table and `422` for a field, and the difference is not arbitrary.** `404`
/// is about the thing the URI names, and for a query the URI names the table:
/// `POST /table/tx/query` against a table that is not there really is a missing resource. A
/// field is named in the *body*, and a body that is well formed but describes something that
/// does not exist is what `422` is for. Answering `404` there would tell a client the endpoint
/// was gone when the endpoint was fine.
fn plan(e: &PlanError) -> u16 {
    match e {
        PlanError::UnknownTable(_) => 404,
        PlanError::UnknownField { .. } => 422,
        PlanError::Unexpected { .. }
        | PlanError::UnterminatedString { .. }
        | PlanError::NumberTooLarge { .. }
        | PlanError::NegativeDecimal { .. }
        | PlanError::TrailingInput { .. }
        | PlanError::UnknownCall(_)
        | PlanError::Arity { .. }
        | PlanError::BadArgument { .. }
        | PlanError::OperatorNotAllowed { .. }
        | PlanError::TooPrecise { .. }
        | PlanError::BadDate { .. }
        | PlanError::BadRounding { .. } => 400,
        // 400 rather than 413: the body is not too large - `MAX_BODY` let it through and would
        // let something ten times deeper through too. What is refused is its shape.
        PlanError::TooDeep { .. } => 400,
    }
}

/// A SQL failure is the statement the client wrote, and there are two kinds.
///
/// **A refusal is a `400`, not a `501`.** `501` says the server has not implemented this yet,
/// which invites a client to try again after an upgrade. A join is not unimplemented here: there
/// is nothing to join, and there never will be. Saying `400` says what is true - this request
/// cannot be made of this server - and the code in the body says which construct.
///
/// The `Plan` arm delegates, so a misspelled field answers `422` whichever surface it arrived
/// through.
fn sql(e: &SqlError) -> u16 {
    match e {
        SqlError::Plan(p) => plan(p),
        SqlError::Syntax { .. }
        | SqlError::UnterminatedString { .. }
        | SqlError::NumberTooLarge { .. }
        | SqlError::NegativeDecimal { .. }
        | SqlError::TooDeep { .. }
        | SqlError::Refused { .. } => 400,
    }
}

fn db(e: &DbError) -> u16 {
    match e {
        // The table is what a URI names, so a missing one is a missing resource. A database
        // names one the same way and answers the same, which is also what makes the two
        // distinguishable to a client: same status, different code.
        // A view is a name a `FROM` resolves, so a missing one is a missing resource for the
        // same reason a missing table is - and distinguishable by its code, not its status.
        DbError::UnknownTable(_) | DbError::UnknownDatabase(_) | DbError::UnknownView(_) => 404,

        // Roles and grants. A role that resolves to nothing is `404` for the reason a table is,
        // and deliberately not `403`: this is a statement failing to find what it named, not a
        // caller being refused what they asked for. The rest are the "state has to change"
        // shape the `409` block below is about - the reserved role, and the two ceilings.
        DbError::Rbac(e) => match e {
            big_rbac::RbacError::UnknownRole(_) => 404,
            _ => 409,
        },
        // A field is named in a body far more often than in a path - `/import` and `/query`
        // both do - so the error-derived answer is `422`. The two routes that *do* put a
        // field in the URI (`POST` and `DELETE` on `/table/{t}/field/{f}`) answer `404`
        // themselves and never reach here.
        DbError::UnknownField { .. } => 422,

        // Something is already there under that name. `409` rather than `400` because the
        // request was well formed and would have succeeded a moment earlier - which is
        // exactly the distinction that decides whether a client should retry.
        DbError::NameTaken(_)
        | DbError::FieldRedefined { .. }
        // A table already there under another engine. The same shape as a redefined field:
        // well formed, and it would have worked before the table existed.
        | DbError::TableRedefined { .. }
        | DbError::BackupDestinationExists(_)
        | DbError::BackupDestinationNotEmpty
        // Same shape as the two above it: the request was well formed and would have worked
        // against an empty table, so what has to change is the state, not the request.
        | DbError::BulkLoadNotEmpty { .. }
        // A `DROP DATABASE` that would have worked while the database was empty. Same shape as
        // the three above: the request is well formed, and what has to change is the state -
        // either drop the tables, or say `CASCADE` and mean it.
        | DbError::DatabaseNotEmpty { .. }
        // Two more of the same shape: the statement was well formed and would have worked a
        // moment earlier, so what has to change is the state - or the caller says
        // `OR REPLACE` and means it.
        | DbError::ViewRedefined(_)
        | DbError::ViewNameTaken(_) => 409,

        DbError::WrongFieldKind { .. }
        | DbError::NameTooLong { .. }
        // Both are a name the caller typed that no name is allowed to be, which is the same
        // class as a name too long: the request has to change, not the state.
        | DbError::NameSeparator(_)
        // A body past the ceiling a catalog record can hold. The same class as a name too
        // long, and the same fix: the request has to change.
        | DbError::ViewTooLong { .. }
        | DbError::DropDefaultDatabase
        // A name the caller typed and no engine has. `400`, like every other malformed value
        // in a statement.
        | DbError::UnknownEngineName(_) => 400,
        // The statement is well formed and the table is real; what does not line up is the
        // question and the engine the table was created under. `422` for the same reason an
        // unknown field in a body gets one - the request has to change, not its syntax.
        DbError::EngineCannotAnswer { .. } => 422,
        // A value the field cannot hold is data the caller sent, and the request was otherwise
        // well formed - so `422` rather than `400`, the same answer a value too wide gets.
        DbError::SignedValueOutOfRange { .. } | DbError::FloatValueOutOfRange { .. } => 422,

        // The file was written by a newer big than this one. `500` because nothing the client
        // can change will help, and it is redacted like every other 5xx: the fix is an
        // operator's, and the log line against the request id is where they will find it.
        DbError::UnknownFieldKind { .. } | DbError::UnknownTableEngine { .. } => 500,
        // A segment that cannot be read is the file's problem, not the request's - damage, or a
        // build older than the data. `500` and redacted like every other 5xx: nothing the
        // client sends will change it, and the log line against the request id is where the
        // operator finds which segment.
        DbError::Column(_) => 500,
        // The only variant is a row key over the limit, which is data the caller sent.
        // A key ceiling is not a bad request: the same line would have been accepted a moment
        // earlier and will be accepted again once the ceiling moves or keys are dropped. That
        // is the same shape as a name already taken, so it gets the same status - and it stays
        // in the 4xx range on purpose, because a 5xx would be redacted and the one thing the
        // caller needs is the number they hit.
        DbError::Key(big_db::KeyError::TooManyKeys { .. }) => 409,
        DbError::Key(_) => 400,

        // Well formed, understood, and too big to answer. `413` would be wrong - that is
        // about the bytes of the request, and this request was small.
        DbError::QueryTooLarge { .. } => 422,

        // `504` rather than `503`, because the two say different things to a retry policy.
        // `503` means busy - come back and the same request will work. A query that passed
        // its deadline will pass it again; what has to change is the query or the limit.
        DbError::QueryTimeout { .. } => 504,
        // nginx's `499`, which is non-standard because the standard has no code for it: the
        // client is gone, so nothing is delivered and this status exists only to be counted
        // and logged as something other than a server fault.
        DbError::QueryCancelled => 499,

        DbError::Field(f) => field(f),
        // Every `BTreeError` that is not a wrapped store or page error means the tree on disk
        // is not the shape this build writes, which is damage rather than a mistake.
        DbError::Tree(_) => 500,
        DbError::Store(s) => store(s),
    }
}

fn field(e: &FieldError) -> u16 {
    match e {
        // The value the caller sent needs more bit planes than the field was declared with.
        FieldError::ValueTooWide { .. } => 400,
        // A record holding two values in a mutex field is a bug in the write path, and no
        // client could have caused it.
        FieldError::MutexConflict { .. } => 500,
        FieldError::Tree(_) => 500,
    }
}

/// The storage layer is where "try again later" and "this is broken" separate.
fn store(e: &StoreError) -> u16 {
    match e {
        // Transient by nature: another handle will let go, readers will finish, an operator
        // can reopen with a larger mapping. A `503` is what tells a proxy to retry.
        StoreError::Locked | StoreError::ReadersActive | StoreError::MapSizeExhausted { .. } => 503,
        StoreError::Unsupported(_) => 501,
        // Nothing a client did, and nothing that gets better on a retry: the operator pointed
        // this process at a file that is not a database. A `500` says so, and the message is
        // redacted like every other `5xx` because it names a path.
        StoreError::NotADatabase { .. } => 500,
        StoreError::Io(_)
        | StoreError::Page(_)
        | StoreError::OutOfBounds { .. }
        | StoreError::NoValidMeta
        | StoreError::ChainCycle { .. }
        | StoreError::SnapshotNotFound(_)
        | StoreError::UnallocatedPage(_) => 500,
    }
}

/// The response an engine error deserves.
///
/// This was `Response::from_error`, and it is here because it could not follow `Response` into
/// `big-wire`: classifying an error means matching on the engine's whole error tree, and a crate
/// that speaks only HTTP must not know that tree exists. `status.rs` is already the one place
/// that knows both, so it is the one place this belongs.
pub fn response_for(e: &ApiError) -> Response {
    let f = Failure::new(e);
    Response::failure(f.status, f.code, &f.message)
}
