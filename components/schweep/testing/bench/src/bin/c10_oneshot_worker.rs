//! Persistent one-shot worker for the paired DuckDB comparison.

#![allow(clippy::print_stdout)]

use std::error::Error;
use std::io::{self, BufRead, Write};

use schweep_plan::bind::Catalog;
use schweep_zset::{DataType, EpochDeltas, Field, Row, Schema, Value};

fn main() -> Result<(), Box<dyn Error>> {
    let input_path = std::env::args()
        .nth(1)
        .ok_or("usage: c10_oneshot_worker TPC_H_LINEITEM_PIPE_FILE")?;
    let schema = Schema::new(vec![
        Field::new("k", DataType::Utf8, false),
        Field::new("n", DataType::Int64, false),
    ])?;
    let catalog = Catalog::from([("t".to_owned(), schema)]);
    let mut input = EpochDeltas::new();
    let file = std::fs::File::open(input_path)?;
    let mut rows = Vec::new();
    for line in io::BufReader::new(file).lines() {
        let line = line?;
        let (key, number) = line.split_once('|').ok_or("invalid TPC-H projection row")?;
        rows.push((
            Row::new(vec![
                Value::Str(key.to_owned()),
                Value::Int(number.parse()?),
            ]),
            1,
        ));
    }
    input.extend("t", rows);
    let sql = "SELECT t.k AS k, SUM(t.n) AS s FROM t GROUP BY t.k";

    let stdout = io::stdout();
    let mut output = stdout.lock();
    writeln!(output, "READY")?;
    output.flush()?;
    for line in io::stdin().lock().lines() {
        if line?.trim() == "STOP" {
            break;
        }
        let answer = schweep_batch::answer_sql(&catalog, sql, &input)?;
        writeln!(output, "{}", answer.len())?;
        output.flush()?;
    }
    Ok(())
}
