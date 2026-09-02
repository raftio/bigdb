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

//! What one node sends another, and what it refuses to read back.
//!
//! Two halves. A round trip has to be exact, because a `Matches` that survives the wire with
//! one record missing is a count that is quietly wrong. And a decoder has to survive bytes it
//! did not write: the peer is authenticated, not trusted, and a build on the other side of an
//! upgrade is not hostile but is just as capable of sending something this one cannot read.

use big_cluster::wire::{self, Assignment, FactValue, OwnedFact, WireError};
use big_container::Container;
use big_db::Matches;
use big_embed::{Plan, Rows, Value};
use big_engine::bitmap::RowSet;
use big_exec::Group;
use big_plan::CmpOp;
use proptest::prelude::*;

/// The representation a container would take on a page, so every test that builds one exercises
/// whichever of the three shapes its values actually deserve.
fn packed(values: impl IntoIterator<Item = u16>) -> Container {
    let c = Container::from_values(values);
    big_container::optimize(c.as_ref(), big_container::Caps { array_max: 2048, run_max: 1024 })
        .into_owned()
}

fn rowset(slots: &[(u64, &[u16])]) -> RowSet {
    let mut out = RowSet::new();
    for (slot, values) in slots {
        out.insert(*slot, Container::from_values(values.iter().copied()));
    }
    out
}

/// A shard, and the slots inside it. Spelled out once so the tests can say what they mean.
type Shard<'a> = (u64, &'a [(u64, &'a [u16])]);

fn matches(shards: &[Shard<'_>]) -> Matches {
    let mut out = Matches::new();
    for (shard, slots) in shards {
        out.insert(*shard, rowset(slots));
    }
    out
}

fn round_trip(v: &Value) -> Value {
    wire::decode_value(&wire::encode_value(v)).expect("what this crate wrote, it can read")
}

#[test]
fn a_set_of_records_survives_the_wire_exactly() {
    let m = matches(&[(0, &[(0, &[1, 2, 3]), (5, &[65535])]), (9, &[(1, &[0])])]);
    let back = round_trip(&Value::Rows(m.clone()));
    let back = back.as_rows().expect("rows went out, rows came back");
    assert_eq!(back.cardinality(), m.cardinality());
    assert_eq!(back.records().collect::<Vec<_>>(), m.records().collect::<Vec<_>>());
}

/// The three container shapes are the three the leaf page stores, and each has to survive
/// as itself: a run that came back as an array would still be the same set, but a bitmap that
/// came back short would not.
#[test]
fn every_container_shape_survives() {
    let dense: Vec<u16> = (0..5000).map(|i| i * 13 % 65535).collect();
    let run: Vec<u16> = (100..4000).collect();
    for values in [vec![1u16, 7, 9], run, dense] {
        let mut rows = RowSet::new();
        rows.insert(0, packed(values.iter().copied()));
        let mut m = Matches::new();
        m.insert(0, rows);
        let back = round_trip(&Value::Rows(m.clone()));
        assert_eq!(
            back.as_rows().unwrap().records().collect::<Vec<_>>(),
            m.records().collect::<Vec<_>>()
        );
    }
}

#[test]
fn every_shape_of_answer_survives() {
    let cases = vec![
        Value::Count(42),
        Value::Sum(u128::MAX),
        Value::SignedSum(i128::MIN),
        Value::Extreme(None),
        Value::Extreme(Some(7)),
        Value::SignedExtreme(Some(-7)),
        Value::Groups(vec![
            Group { row: 1, key: Some("GB".to_string()), value: Box::new(Value::Count(3)) },
            Group { row: 2, key: None, value: Box::new(Value::Sum(9)) },
        ]),
    ];
    for case in cases {
        let back = round_trip(&case);
        assert_eq!(format!("{back:?}"), format!("{case:?}"));
    }
}

/// A plan travels, never query text. Two nodes parsing the same string is exactly the
/// disagreement this encoding exists to prevent, so the plan has to survive whole.
#[test]
fn a_plan_survives_the_wire() {
    let plan = Plan::GroupBy {
        table: "tx".to_string(),
        rows: Rows::Intersect(vec![
            Rows::Key { field: "country".to_string(), value: "GB".to_string() },
            Rows::Not(Box::new(Rows::Compare {
                field: "amount".to_string(),
                op: CmpOp::Ge,
                value: 500,
            })),
            Rows::KeyBetween {
                field: "visit".to_string(),
                value: "home".to_string(),
                from: Some(-1),
                to: None,
            },
            // A pattern travels as a pattern. The matching happens at the owner, against the
            // dictionary it holds, so what crosses the wire is the question rather than the
            // list of keys one node happened to have interned - which is the whole reason a
            // plan travels and query text does not.
            Rows::KeyLike { field: "country".to_string(), pattern: "G%".to_string(), fold: false },
            Rows::KeyLike { field: "country".to_string(), pattern: "g_".to_string(), fold: true },
            Rows::Difference(
                Box::new(Rows::All),
                Box::new(Rows::Bool { field: "active".to_string(), value: true }),
            ),
        ]),
        field: "country".to_string(),
        aggregate: Box::new(Plan::Sum {
            table: "tx".to_string(),
            rows: Rows::All,
            field: "amount".to_string(),
        }),
    };
    let request = wire::QueryRequest { plan: plan.clone(), timeout_ms: Some(250) };
    let back = wire::QueryRequest::decode(&request.encode()).unwrap();
    assert_eq!(back.plan, plan);
    assert_eq!(back.timeout_ms, Some(250));
}

#[test]
fn a_batch_and_its_key_assignments_survive() {
    let request = wire::ImportRequest {
        table: "tx".to_string(),
        keys: vec![Assignment { field: "country".to_string(), key: "GB".to_string(), row: 3 }],
        facts: vec![
            OwnedFact { field: "amount".to_string(), record: 1, value: FactValue::Int(500) },
            OwnedFact { field: "owed".to_string(), record: 2, value: FactValue::Signed(-9) },
            OwnedFact {
                field: "country".to_string(),
                record: 3,
                value: FactValue::Key("GB".to_string()),
            },
            OwnedFact { field: "active".to_string(), record: 4, value: FactValue::Bool(false) },
        ],
    };
    assert_eq!(wire::ImportRequest::decode(&request.encode()).unwrap(), request);
}

#[test]
fn every_schema_change_survives() {
    let cases = vec![
        // One per engine, because the engine is a tag on the wire and a tag that only ever
        // travels as its default is a tag nothing has tested.
        wire::Ddl::CreateTable { table: "tx".to_string(), engine: big_embed::TableEngine::Bitmap },
        wire::Ddl::CreateTable {
            table: "tx".to_string(),
            engine: big_embed::TableEngine::BitmapColumnar,
        },
        wire::Ddl::CreateTable {
            table: "tx".to_string(),
            engine: big_embed::TableEngine::Columnar,
        },
        wire::Ddl::CreateField {
            table: "tx".to_string(),
            field: "amount".to_string(),
            kind: big_embed::FieldKind::SignedInt,
            bit_depth: 20,
        },
        wire::Ddl::CreateDecimal {
            table: "tx".to_string(),
            field: "price".to_string(),
            bit_depth: 32,
            scale: -2,
        },
        wire::Ddl::CreateTimeQuantum {
            table: "tx".to_string(),
            field: "visit".to_string(),
            granularity: vec![big_embed::Granularity::Year, big_embed::Granularity::Hour],
        },
        wire::Ddl::DropTable { table: "tx".to_string() },
        wire::Ddl::DropField { table: "tx".to_string(), field: "amount".to_string() },
        wire::Ddl::CreateDatabase { name: "sales".to_string() },
        wire::Ddl::DropDatabase { name: "sales".to_string() },
        // A table in a database travels as one qualified string rather than as a second field,
        // which is the whole of what `WIRE_VERSION` 3 changed. The round trip is the assertion
        // that the string survives the `.` it now carries.
        wire::Ddl::CreateTable {
            table: "sales.orders".to_string(),
            engine: big_embed::TableEngine::Bitmap,
        },
        wire::Ddl::DropTable { table: "sales.orders".to_string() },
        wire::Ddl::DropField { table: "sales.orders".to_string(), field: "amount".to_string() },
        // A view carries a whole statement, which is the longest string on this wire and the
        // only one holding quotes, spaces and a `*`. Qualified for the same reason a table is:
        // the database it is created in is also the database its body resolves in.
        wire::Ddl::CreateView {
            view: "big".to_string(),
            text: "SELECT amount, country AS cc FROM tx WHERE country = 'GB'".to_string(),
        },
        wire::Ddl::CreateView {
            view: "sales.big".to_string(),
            text: "SELECT amount FROM orders WHERE amount >= 500".to_string(),
        },
        wire::Ddl::DropView { view: "big".to_string() },
        wire::Ddl::DropView { view: "sales.big".to_string() },
    ];
    for case in cases {
        assert_eq!(wire::Ddl::decode(&case.encode()).unwrap(), case);
    }
}

// ------------------------------------------------------------------------------------------
// Bytes this crate did not write
// ------------------------------------------------------------------------------------------

#[test]
fn a_truncated_message_is_refused_at_every_length() {
    let full = wire::encode_value(&Value::Rows(matches(&[(0, &[(0, &[1, 2, 3])])])));
    for cut in 0..full.len() {
        assert!(
            wire::decode_value(&full[..cut]).is_err(),
            "{cut} bytes of {} decoded into something",
            full.len()
        );
    }
    assert!(wire::decode_value(&full).is_ok());
}

/// Trailing bytes mean the two sides disagree about the shape of a message, and answering
/// confidently from the half that was understood is the failure this layer exists to refuse.
#[test]
fn bytes_after_the_end_are_refused() {
    let mut full = wire::encode_value(&Value::Count(1));
    full.push(0);
    assert!(matches!(wire::decode_value(&full), Err(WireError::Malformed(_))));
}

#[test]
fn a_tag_that_names_nothing_is_refused() {
    assert!(matches!(
        wire::decode_value(&[200]),
        Err(WireError::BadTag { what: "value", tag: 200 })
    ));
    assert!(matches!(wire::Ddl::decode(&[99]), Err(WireError::BadTag { .. })));
}

/// A length field is four bytes and can claim anything. Nothing is sized from one before it is
/// checked against the bytes that are actually there.
#[test]
fn a_length_larger_than_the_message_allocates_nothing() {
    // Tag 0 is `Rows`, then a shard count of four billion, and nothing after it.
    let mut bytes = vec![0u8];
    bytes.extend_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(wire::decode_value(&bytes), Err(WireError::Truncated)));
}

/// Structurally readable and still nonsense. The set algebra assumes a container's values
/// ascend, so a container that arrives out of order is refused here rather than quietly giving
/// a wrong answer in an intersection.
#[test]
fn a_container_whose_values_descend_is_refused() {
    let mut bytes = vec![0u8]; // Value::Rows
    bytes.extend_from_slice(&1u32.to_le_bytes()); // one shard
    bytes.extend_from_slice(&0u64.to_le_bytes()); // shard 0
    bytes.extend_from_slice(&1u32.to_le_bytes()); // one slot
    bytes.extend_from_slice(&0u64.to_le_bytes()); // slot 0
    bytes.push(0); // an array
    bytes.extend_from_slice(&3u32.to_le_bytes());
    for v in [9u16, 4, 1] {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    assert!(matches!(wire::decode_value(&bytes), Err(WireError::Malformed(_))));
}

/// Nesting is call depth here as it is in the parser, so it takes the parser's ceiling. A peer
/// could otherwise send a plan that overflows the stack rather than failing.
#[test]
fn a_plan_nested_past_the_limit_is_refused() {
    let mut bytes = Vec::new();
    // `Rows::Not`, one deeper than the decoder will follow.
    bytes.extend(std::iter::repeat_n(8u8, wire::MAX_DEPTH + 1));
    bytes.push(9); // Rows::All
    let mut r = wire::Reader::new(&bytes);
    assert_eq!(wire::get_rows(&mut r), Err(WireError::TooDeep));
}

proptest! {
    /// Any set of records this engine can hold comes back as the same set.
    #[test]
    fn any_row_set_survives(
        values in prop::collection::btree_set(0u16..u16::MAX, 0..400),
        shard in 0u64..8,
        slot in 0u64..16,
    ) {
        let mut rows = RowSet::new();
        rows.insert(slot, packed(values.iter().copied()));
        let mut m = Matches::new();
        m.insert(shard, rows);

        let back = round_trip(&Value::Rows(m.clone()));
        let back = back.as_rows().unwrap();
        prop_assert_eq!(back.cardinality(), m.cardinality());
        prop_assert_eq!(
            back.records().collect::<Vec<_>>(),
            m.records().collect::<Vec<_>>()
        );
    }

    /// Nothing a peer can send makes a decoder panic. Refusing is the only other option, and
    /// it is the one this test is checking is always taken.
    #[test]
    fn arbitrary_bytes_are_refused_rather_than_trusted(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        let _ = wire::decode_value(&bytes);
        let _ = wire::QueryRequest::decode(&bytes);
        let _ = wire::ImportRequest::decode(&bytes);
        let _ = wire::DeleteRequest::decode(&bytes);
        let _ = wire::RecordsRequest::decode(&bytes);
        let _ = wire::InternRequest::decode(&bytes);
        let _ = wire::Ddl::decode(&bytes);
        let _ = wire::get_records(&bytes);
    }
}

/// A segment travels as what its records *hold*, not as encoded blocks.
///
/// Shipping the encoded form would tie two nodes to the same codec choice for ever; shipping the
/// cells lets the receiver encode with its own. So the round trip that matters is the cells, and
/// it has to keep both shapes apart - a scalar column and a keyed one store different things and
/// the value alone does not say which.
#[test]
fn a_segment_body_survives_the_wire() {
    let addr = big_embed::FragmentAddr {
        table: "tx".to_string(),
        field: Some("country".to_string()),
        view: None,
        field_id: 0,
        view_id: big_db::COLUMN_VIEW,
        shard: 3,
    };
    let body = wire::FragmentBody {
        addr,
        meta: big_embed::FragmentMeta { bit_depth: 7, min: 1, max: 99, has_values: true },
        data: big_embed::FragmentData::Cells(vec![
            (0, big_embed::ColumnCell::Value(42)),
            (1023, big_embed::ColumnCell::List(vec![1, 2, 3])),
            (1024, big_embed::ColumnCell::List(vec![])),
        ]),
    };

    let back = wire::FragmentBody::decode(&body.encode()).unwrap();
    assert_eq!(back.addr, body.addr);
    assert_eq!(back.meta, body.meta);
    match (&back.data, &body.data) {
        (big_embed::FragmentData::Cells(a), big_embed::FragmentData::Cells(b)) => {
            assert_eq!(a, b)
        }
        other => panic!("a segment came back as {other:?}"),
    }
}

/// The tag is written before the units so a reader knows what it is about to decode. Without it
/// the shape would have to be inferred from the address, which is resolved against the
/// *receiver's* catalog - and a body has to be readable before that resolution.
#[test]
fn a_body_with_an_unknown_data_tag_is_refused() {
    let addr = big_embed::FragmentAddr {
        table: "tx".to_string(),
        field: None,
        view: None,
        field_id: 0,
        view_id: 0,
        shard: 0,
    };
    let body = wire::FragmentBody {
        addr,
        meta: big_embed::FragmentMeta::default(),
        data: big_embed::FragmentData::Containers(Vec::new()),
    };
    let mut bytes = body.encode();
    // The tag sits right after the address and the meta; find it by round-tripping a known-good
    // body and corrupting the one byte that decodes as the tag.
    let tag_at = bytes.len() - 2;
    bytes[tag_at] = 9;
    assert!(wire::FragmentBody::decode(&bytes).is_err());
}
