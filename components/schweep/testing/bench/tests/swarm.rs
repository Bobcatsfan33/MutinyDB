//! The non-timing tooth behind C10's swarm benchmark.
//!
//! A benchmark can get faster because it stopped doing work. This gate proves that its 10,000-query
//! population is distinct and that sharing materially changes held circuitry while answers stay equal.

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::collections::BTreeSet;

use schweep_bench::near_duplicate_queries;
use schweep_memo::{Memo, Sharing};
use schweep_plan::bind::Catalog;
use schweep_zset::{DataType, Field, Schema};

const SWARM_SIZE: usize = 10_000;

fn catalog() -> Catalog {
    let schema = Schema::new(vec![
        Field::new("k", DataType::Int64, false),
        Field::new("n", DataType::Int64, false),
    ])
    .unwrap();
    Catalog::from([("t".to_owned(), schema)])
}

#[test]
fn ten_thousand_queries_are_distinct_and_share_their_common_work() {
    let queries = near_duplicate_queries(SWARM_SIZE);
    assert_eq!(queries.len(), SWARM_SIZE);
    assert_eq!(queries.iter().collect::<BTreeSet<_>>().len(), SWARM_SIZE);

    let mut shared = Memo::with_sharing(catalog(), Sharing::On).unwrap();
    let mut private = Memo::with_sharing(catalog(), Sharing::Off).unwrap();
    for sql in &queries {
        let shared_handle = shared.register_sql(sql).unwrap();
        let private_handle = private.register_sql(sql).unwrap();
        assert_eq!(
            shared.read(shared_handle).unwrap(),
            private.read(private_handle).unwrap()
        );
    }
    shared.audit().unwrap();
    private.audit().unwrap();

    let shared_nodes = shared.accounting().live_nodes;
    let private_nodes = private.accounting().live_nodes;
    assert!(
        shared_nodes * 2 < private_nodes,
        "a sharing regression must move this tooth: {shared_nodes} shared vs {private_nodes} private"
    );
}
