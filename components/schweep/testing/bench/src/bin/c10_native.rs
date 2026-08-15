//! Reproducible native half of the C10 performance evidence.
//!
//! Run with `cargo run --release -p schweep-bench --bin c10_native`. The DuckDB comparison is paired
//! by `scripts/run_c10_benchmarks.py`, which also combines this JSON into the committed artifact.

#![allow(clippy::print_stdout)]

use std::error::Error;
use std::hint::black_box;

use schweep_bench::{
    interleaved, marginal_query, near_duplicate_queries, Benchmark, Count, Machine,
};
use schweep_memo::{Admission, Memo, Sharing};
use schweep_plan::bind::Catalog;
use schweep_zset::{DataType, EpochDeltas, Field, Row, Schema, Value};

const ROUNDS: usize = 11;
const READS_PER_ROUND: usize = 1_000;
const SWARM_SIZE: usize = 10_000;

fn catalog() -> Result<Catalog, Box<dyn Error>> {
    let schema = Schema::new(vec![
        Field::new("k", DataType::Int64, false),
        Field::new("n", DataType::Int64, false),
    ])?;
    Ok(Catalog::from([("t".to_owned(), schema)]))
}

fn changes(rows: usize, weight: i64) -> EpochDeltas {
    let mut delta = EpochDeltas::new();
    delta.extend(
        "t",
        (0..rows).map(|index| {
            (
                Row::new(vec![
                    Value::Int((index % 128) as i64),
                    Value::Int(index as i64),
                ]),
                weight,
            )
        }),
    );
    delta
}

fn maintenance_sample(rows: usize) -> Result<impl FnMut() -> Count, Box<dyn Error>> {
    let mut memo = Memo::with_sharing(catalog()?, Sharing::On)?;
    memo.register_sql("SELECT t.n AS n FROM t WHERE t.k > 1")?;
    let insert = changes(rows, 1);
    let retract = changes(rows, -1);
    let mut adding = true;
    Ok(move || {
        let delta = if adding { &insert } else { &retract };
        adding = !adding;
        if memo.seal_epoch(delta).is_err() {
            return Count(0);
        }
        Count(rows as u64)
    })
}

fn main() -> Result<(), Box<dyn Error>> {
    let machine = Machine::detect();
    if !machine.is_a_performance_build() {
        return Err("C10 evidence requires a --release build".into());
    }

    let mut small = maintenance_sample(100)?;
    let mut medium = maintenance_sample(1_000)?;
    let mut large = maintenance_sample(10_000)?;
    let mut maintenance_workloads: [&mut dyn FnMut() -> Count; 3] =
        [&mut small, &mut medium, &mut large];
    let maintenance = interleaved(
        &["maintenance-100", "maintenance-1000", "maintenance-10000"],
        ROUNDS,
        &mut maintenance_workloads,
    );

    let mut read_memo = Memo::with_sharing(catalog()?, Sharing::On)?;
    let seed = changes(100_000, 1);
    read_memo.seal_epoch(&seed)?;
    let read_plan = schweep_sql::compile(
        "SELECT t.k AS k, COUNT(*) AS c FROM t GROUP BY t.k",
        &catalog()?,
    )?;
    let read_handle = read_memo.register(
        &read_plan,
        Admission::with_unbounded_state("C10 controlled benchmark population"),
    )?;
    let read_latency = schweep_bench::sample("standing-answer-read", ROUNDS, || {
        for _ in 0..READS_PER_ROUND {
            black_box(read_memo.read(read_handle)).ok();
        }
        (Count(READS_PER_ROUND as u64), ())
    });

    let mut swarm = Memo::with_sharing(catalog()?, Sharing::On)?;
    for sql in near_duplicate_queries(SWARM_SIZE) {
        swarm.register_sql(&sql)?;
    }
    let mut next_query = SWARM_SIZE;
    let marginal = schweep_bench::sample("swarm-marginal-registration", ROUNDS, || {
        let sql = marginal_query(next_query);
        next_query += 1;
        let performed = match swarm.register_sql(&sql) {
            Ok(handle) => {
                black_box(swarm.read(handle)).ok();
                if swarm.deregister(handle).is_ok() {
                    1
                } else {
                    0
                }
            }
            Err(_) => 0,
        };
        (Count(performed), ())
    });
    swarm.audit()?;

    let mut reports = Vec::new();
    for (index, sample) in maintenance.into_iter().enumerate() {
        let rows = [100, 1_000, 10_000].get(index).copied().unwrap_or(0);
        reports.push(
            Benchmark::new(
                format!("maintenance change volume {rows}"),
                sample,
                "changed row",
            )
            .with_note("alternating insert/retract keeps retained state bounded")
            .to_json(),
        );
    }
    reports.push(
        Benchmark::new("standing answer read latency", read_latency, "read")
            .with_note(
                "100,000-row retained input; 128-row aggregate answer; 1,000 reads per round",
            )
            .to_json(),
    );
    reports.push(
        Benchmark::new(
            "10,000-query swarm marginal registration",
            marginal,
            "marginal query",
        )
        .with_note("10,000 distinct queries share one scan/filter prefix")
        .to_json(),
    );

    println!(
        "{{\n  \"schema_version\": 1,\n  \"suite\": \"schweep-c10-native\",\n  \"benchmarks\": [\n{}\n  ]\n}}",
        reports.join(",\n")
    );
    Ok(())
}
