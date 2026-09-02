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

//! The shortest thing that produces into a table.
//!
//! ```text
//! bigctl sql "CREATE TABLE tx (amount INT, country TEXT)"
//! cargo run -p big-message --example produce -- 127.0.0.1:7654 tx 1000
//! bigctl sql "SELECT count(*) FROM tx"
//! ```
//!
//! Note what is not here: no record id, and nowhere one could be put.
//!
//! `examples/producer/` in the repository root runs the same thing under `docker compose`, with
//! the server and the table set up for you.

use big_message::{Config, Error, Producer, Value};

fn main() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let addr = args.first().map_or("127.0.0.1:7654", String::as_str);
    let table = args.get(1).map_or("tx", String::as_str);
    let count: u64 = args.get(2).and_then(|n| n.parse().ok()).unwrap_or(1000);

    // The token, if the server wants one. Read from the environment rather than a flag so it
    // does not end up in a shell history or a process listing.
    //
    // **`BIG_MESSAGE_TOKEN`, not `BIG_TOKEN`**, and the difference is the point: `bigctl` reads
    // `BIG_TOKEN` as the *path* to a mode-600 file, because a long-lived client should not carry
    // a secret in its environment. This is an example that has to run in a container beside the
    // server, where the mode of a mounted file is whatever the runtime decided - so it takes the
    // token itself. Two spellings because they are two things; one name for both is how a demo
    // teaches somebody to leak a credential.
    let token = std::env::var("BIG_MESSAGE_TOKEN").ok();

    let mut producer =
        Producer::open(addr, table, &["amount", "country"], token.as_deref(), Config::default())?;

    let countries = ["GB", "US", "VN", "JP"];
    for i in 0..count {
        producer.send(&[
            Value::Int(i * 10),
            Value::Text(countries[(i % countries.len() as u64) as usize]),
        ])?;
    }

    let flushed = producer.close()?;
    println!("{} rows", flushed.inserted);
    Ok(())
}
