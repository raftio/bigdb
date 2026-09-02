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

//! Schema, over the cluster.
//!
//! Each of these is one DDL applied at the schema leader and then everywhere else, so a
//! partial success is the cluster's problem to report rather than this layer's to paper over.

use super::*;

pub(super) fn create_table<P: PagerMut + Sync>(
    ctx: &Ctx<'_, P>,
    req: &Request,
    table: &str,
) -> Response {
    // Absent means the default, not `bitmap`. A caller who says nothing wants the engine that
    // answers the widest range of questions well; a caller who wants the narrow one says so.
    let engine = match req.param("engine") {
        None => big_embed::TableEngine::default(),
        Some(s) => match big_embed::TableEngine::parse(&s) {
            Some(e) => e,
            None => {
                return Response::failure(
                    400,
                    "bad_parameter",
                    "engine must be bitmap, bitmap+columnar or columnar",
                )
            }
        },
    };
    match ctx.cluster.create_table_with(table, engine) {
        Ok(id) => Response::ok(format!("{{\"table\":{id}}}")),
        Err(e) => from_cluster(&e),
    }
}

pub(super) fn create_field<P: PagerMut + Sync>(
    ctx: &Ctx<'_, P>,
    req: &Request,
    table: &str,
    field: &str,
) -> Response {
    let Some(kind) = req.param("kind").as_deref().and_then(parse_kind) else {
        return Response::failure(
            400,
            "bad_parameter",
            "kind must be int, signed, decimal, set, mutex, bool, timequantum, float32, \
             float64, date or datetime",
        );
    };
    let bit_depth = match req.param("bit_depth").map(|v| v.parse::<u32>()) {
        Some(Ok(n)) => n,
        Some(Err(_)) => {
            return Response::failure(400, "bad_parameter", "bit_depth must be a number")
        }
        None => super::default_bit_depth(kind),
    };

    let result = match kind {
        FieldKind::Decimal => match req.param("scale").map(|v| v.parse::<i8>()) {
            Some(Ok(scale)) => ctx.cluster.create_decimal(table, field, bit_depth, scale),
            Some(Err(_)) => {
                return Response::failure(400, "bad_parameter", "scale must be a small number")
            }
            // Without a scale a decimal is only an integer wearing a different name, and the
            // difference matters to every comparison written against it.
            None => {
                return Response::failure(400, "bad_parameter", "a decimal field needs a scale")
            }
        },
        FieldKind::TimeQuantum => ctx.cluster.create_time_quantum(table, field, Vec::new()),
        _ => ctx.cluster.create_field(table, field, kind, bit_depth),
    };

    match result {
        Ok(id) => Response::ok(format!("{{\"field\":{id}}}")),
        Err(e) => from_cluster(&e),
    }
}

/// Dropping something that is not there answers 404 rather than pretending it worked: a
/// client that misspelled a name should hear about it.
pub(super) fn drop_table<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, table: &str) -> Response {
    match ctx.cluster.drop_table(table) {
        Ok(true) => Response::ok(format!("{{\"dropped\":\"{table}\"}}")),
        Ok(false) => Response::failure(404, "unknown_table", &format!("no table named `{table}`")),
        Err(e) => from_cluster(&e),
    }
}

/// `POST /database/{d}` - the namespace a table's name is unique within.
///
/// Idempotent, like `POST /table/{t}`: a database that is already there answers `200` with
/// nothing created, because a caller running a setup script twice asked for the same state
/// twice and got it.
pub(super) fn create_database<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, database: &str) -> Response {
    match ctx.cluster.create_database(database) {
        Ok(created) => {
            Response::ok(format!("{{\"database\":\"{database}\",\"created\":{created}}}"))
        }
        Err(e) => from_cluster(&e),
    }
}

/// `DELETE /database/{d}?cascade=true` - the database, and with `cascade` its tables.
///
/// **Without `cascade` a database that still holds tables is `409`, not a mass drop.** The
/// same default Postgres and BigQuery take, and for the same reason: this is one word away
/// from being the most expensive request on this surface.
pub(super) fn drop_database<P: PagerMut + Sync>(
    ctx: &Ctx<'_, P>,
    req: &Request,
    database: &str,
) -> Response {
    let cascade = req.param("cascade").is_some_and(|v| v == "true" || v == "1");
    match ctx.cluster.drop_database_if_empty(database, cascade) {
        Ok(true) => Response::ok(format!("{{\"dropped\":\"{database}\"}}")),
        Ok(false) => {
            Response::failure(404, "unknown_database", &format!("no database named `{database}`"))
        }
        Err(e) => from_cluster(&e),
    }
}

pub(super) fn drop_field<P: PagerMut + Sync>(
    ctx: &Ctx<'_, P>,
    table: &str,
    field: &str,
) -> Response {
    match ctx.cluster.drop_field(table, field) {
        Ok(true) => Response::ok(format!("{{\"dropped\":\"{field}\"}}")),
        Ok(false) => Response::failure(
            404,
            "unknown_field",
            &format!("table `{table}` has no field named `{field}`"),
        ),
        Err(e) => from_cluster(&e),
    }
}
