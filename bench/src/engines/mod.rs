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

//! One module per engine under comparison.

pub mod big;
/// `big` again, behind the analytical trait rather than the storage one.
pub mod big_olap;
// The analytical peers are behind features because of what they cost to build, not because they
// are optional to the argument. See `bench/Cargo.toml`.
#[cfg(feature = "http-peers")]
pub mod clickhouse;
#[cfg(feature = "datafusion-peer")]
pub mod datafusion;
#[cfg(feature = "duckdb-peer")]
pub mod duckdb;
pub mod fjall;
#[cfg(feature = "http-peers")]
pub mod http;
// LMDB is the one entrant whose API is unsafe to call. `heed` requires the caller to promise
// that no other process is mutating the environment, and there is no safe way to state that -
// so the crate-wide `deny(unsafe_code)` is lifted here and nowhere else, with the obligation
// discharged at each call site rather than by this attribute.
#[allow(unsafe_code)]
pub mod lmdb;
pub mod redb;
pub mod sqlite;
