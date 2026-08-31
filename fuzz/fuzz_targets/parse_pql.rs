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

#![no_main]

//! The query parser, which is the one entry point in this engine a stranger reaches directly.
//!
//! `bigd` hands `parse` whatever arrived in a request body. So the contract is the same as the
//! page parser's - return `Result`, never panic - with one addition that a byte parser does not
//! have to worry about: **this is recursive descent, so deeply nested input is a stack overflow
//! rather than an error.** `Row(((((((...` is four bytes of grammar and a thousand bytes of
//! depth, which a fuzzer finds in seconds and a `Result` cannot express.
//!
//! Both are checked here. Depth is bounded by the parser itself; if that bound is ever removed,
//! this target stops returning and starts crashing, which is the point.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Invalid UTF-8 never reaches the parser: the HTTP layer decodes the body first, so feeding
    // it here would spend the fuzzer's budget on a case production cannot produce.
    let Ok(text) = core::str::from_utf8(data) else { return };

    if let Ok(call) = big_plan::parse(text) {
        // A successful parse has to survive being looked at. Formatting walks the same tree the
        // planner will, so a structure the parser can build but nothing else can traverse fails
        // here rather than in a query.
        let printed = format!("{call:?}");
        assert!(!printed.is_empty());
    }
});
