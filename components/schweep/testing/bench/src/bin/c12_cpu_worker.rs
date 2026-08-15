//! C12's current CPU one-shot candidate and deterministic input generator.

#![allow(clippy::print_stdout)]

use std::error::Error;
use std::io::{self, BufRead, Read, Write};
use std::path::Path;

use schweep_plan::bind::Catalog;
use schweep_zset::{DataType, EpochDeltas, Field, Row, Schema, Value};

const SIZES: [usize; 3] = [100_000, 1_000_000, 10_000_000];
const SQL: &str = "SELECT SUM(t.n) AS total FROM t WHERE t.k > 0";

fn pair(index: usize) -> (i64, i64) {
    (
        i64::try_from(index % 2_048).unwrap_or(0) - 1_024,
        i64::try_from(index % 127).unwrap_or(0) - 63,
    )
}

fn generate(path: &Path, rows: usize) -> Result<(), Box<dyn Error>> {
    let file = std::fs::File::create(path)?;
    let mut output = io::BufWriter::new(file);
    for index in 0..rows {
        let (key, value) = pair(index);
        output.write_all(&key.to_le_bytes())?;
        output.write_all(&value.to_le_bytes())?;
    }
    output.flush()?;
    Ok(())
}

fn read_pairs(path: &Path) -> Result<Vec<(i64, i64)>, Box<dyn Error>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?.read_to_end(&mut bytes)?;
    if bytes.len() % 16 != 0 {
        return Err("C12 input length is not a whole number of Int64 pairs".into());
    }
    let mut pairs = Vec::with_capacity(bytes.len() / 16);
    for chunk in bytes.chunks_exact(16) {
        let (key, value) = chunk.split_at(8);
        pairs.push((
            i64::from_le_bytes(key.try_into()?),
            i64::from_le_bytes(value.try_into()?),
        ));
    }
    Ok(pairs)
}

fn catalog() -> Result<Catalog, Box<dyn Error>> {
    let schema = Schema::new(vec![
        Field::new("k", DataType::Int64, false),
        Field::new("n", DataType::Int64, false),
    ])?;
    Ok(Catalog::from([("t".to_owned(), schema)]))
}

fn input(pairs: &[(i64, i64)], rows: usize) -> Result<EpochDeltas, Box<dyn Error>> {
    if pairs.len() < rows {
        return Err(format!("C12 input has {} rows, needs {rows}", pairs.len()).into());
    }
    let mut deltas = EpochDeltas::new();
    deltas.extend(
        "t",
        pairs
            .iter()
            .take(rows)
            .map(|(key, value)| (Row::new(vec![Value::Int(*key), Value::Int(*value)]), 1)),
    );
    Ok(deltas)
}

fn answer(catalog: &Catalog, input: &EpochDeltas) -> Result<i64, Box<dyn Error>> {
    let answer = schweep_batch::answer_sql(catalog, SQL, input)?;
    let (row, weight) = answer
        .entries()
        .first()
        .ok_or("grand-total query returned no row")?;
    if answer.entries().len() != 1 || *weight != 1 {
        return Err("grand-total query returned a non-canonical singleton".into());
    }
    match row.get(0) {
        Some(Value::Int(value)) => Ok(*value),
        _ => Err("grand-total query did not return one Int64".into()),
    }
}

fn serve(path: &Path) -> Result<(), Box<dyn Error>> {
    let pairs = read_pairs(path)?;
    let catalog = catalog()?;
    let mut inputs = Vec::new();
    for rows in SIZES.into_iter().filter(|rows| *rows <= pairs.len()) {
        inputs.push((rows, input(&pairs, rows)?));
    }
    drop(pairs);

    let stdout = io::stdout();
    let mut output = stdout.lock();
    writeln!(output, "READY")?;
    output.flush()?;
    for line in io::stdin().lock().lines() {
        let line = line?;
        if line.trim() == "STOP" {
            break;
        }
        let rows: usize = line
            .strip_prefix("RUN ")
            .ok_or("expected RUN <rows>")?
            .trim()
            .parse()?;
        let selected = inputs
            .iter()
            .find(|(candidate, _)| *candidate == rows)
            .map(|(_, input)| input)
            .ok_or("requested a size outside the frozen C12 matrix")?;
        writeln!(output, "RESULT\t{}", answer(&catalog, selected)?)?;
        output.flush()?;
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.first().map(String::as_str) == Some("--generate") {
        let path = arguments.get(1).ok_or("--generate needs a path")?;
        let rows = arguments
            .get(2)
            .ok_or("--generate needs a row count")?
            .parse()?;
        return generate(Path::new(path), rows);
    }
    let path = arguments.first().ok_or("usage: c12_cpu_worker INPUT")?;
    serve(Path::new(path))
}
