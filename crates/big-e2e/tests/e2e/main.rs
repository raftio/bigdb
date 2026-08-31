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

//! The four binaries, run as processes, against files on disk.
//!
//! **What this layer is for.** Everything below it stops at a library entry point: `big-cli`'s
//! tests call `big_cli::run`, `big-http`'s call `Server::bind`, and the cluster tests build an
//! `Api::in_memory()`. All of that is the right shape for what it claims - but it means the
//! `main` of every shipped binary, and every decision `main` makes before anything else runs,
//! had no test at all. `bigd`'s argument parser had exactly one caller and zero tests.
//!
//! So the rule here is: **nothing is linked, everything is spawned.** A test starts a real
//! `bigd` on a real file, talks to it with a real `bigc`, and reads the exit code a shell would
//! read. What it costs is process startup; what it buys is the only coverage of the surface an
//! operator actually touches.
//!
//! The cluster module is `#[ignore]` by default - see its own header for why.

mod common;

mod cluster;
mod daemon;
mod offline;
mod together;
