//! Parquet snapshots of the input integrals — the ground truth (`ARCHITECTURE.md` §5.8, §6 C7).
//!
//! A snapshot directory is what makes the log finite. It holds the *accumulated contents* of every
//! table as of one epoch, one Parquet file each, plus the dedup ledger and a manifest:
//!
//! ```text
//!   snap-0000000004/
//!     t.parquet          the integral of table t as of epoch 4, consolidated
//!     u.parquet
//!     DEDUP              the acknowledged tokens (schweep-log's format — see why below)
//!     MANIFEST           epoch, per-file row count and checksum, dedup checksum
//! ```
//!
//! ## Why Parquet, and what "ground truth" means
//!
//! A snapshot is not a checkpoint. A checkpoint restores *one circuit* — its operator state, its
//! answer — and it is written in our own format because only we will ever read it. A snapshot is the
//! **data**, and the data outlives every circuit built over it: bootstrap reads it, one-shot queries
//! read it, C6's mid-history attach reads it after a compaction, and a human with `duckdb` or `pandas`
//! can read it too. That last one is not a nice-to-have — a database whose ground truth is only
//! readable by itself asks to be trusted, and this project does not ask.
//!
//! ## The weight column
//!
//! A Z-set is rows *with weights* (S-4), and Parquet has no notion of one. So each file carries the
//! table's own columns plus a trailing `__weight` `Int64`, and reading splits it back off. The name is
//! reserved rather than escaped: a table with a column called `__weight` is refused at snapshot time
//! instead of producing a file whose two `__weight` columns mean different things.
//!
//! ## Consolidated, and zero weights dropped
//!
//! What is written is the integral **consolidated** (S-5): each distinct row once, at its net weight,
//! and rows whose net weight is zero are not written at all. That is not a size optimisation — a row
//! at weight zero is a row that is *not present*, and writing it would make the snapshot claim
//! something the log does not (S-4). One of C7's canonical mutations breaks exactly this.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::{Int64Array, RecordBatch};
use arrow_schema::{DataType as ArrowType, Field as ArrowField, Schema as ArrowSchema};
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};
use parquet::arrow::ArrowWriter;
use schweep_zset::{EpochDeltas, Schema, ZSetBatch};

use crate::error::{BatchError, Result};

/// The reserved weight column.
pub const WEIGHT_COLUMN: &str = "__weight";
/// The manifest file inside a snapshot.
pub const MANIFEST: &str = "MANIFEST";

/// What a snapshot's manifest records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub epoch: u64,
    /// Table → (rows written, CRC-32 of the file's bytes).
    pub tables: BTreeMap<String, (usize, u32)>,
    pub dedup_crc: u32,
}

impl Manifest {
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut body = format!("current snapshot v1\nepoch={}\n", self.epoch);
        for (table, (rows, crc)) in &self.tables {
            body.push_str(&format!("table={table} rows={rows} crc={crc:08x}\n"));
        }
        body.push_str(&format!("dedup={:08x}\n", self.dedup_crc));
        format!(
            "{body}manifest={:08x}\n",
            schweep_log::record::crc32(body.as_bytes())
        )
        .into_bytes()
    }

    pub fn decode(bytes: &[u8]) -> Result<Manifest> {
        let text = std::str::from_utf8(bytes).map_err(|_| BatchError::CorruptSnapshot {
            what: "manifest is not UTF-8",
        })?;
        let (body, checksum) =
            text.rsplit_once("manifest=")
                .ok_or(BatchError::CorruptSnapshot {
                    what: "manifest has no checksum",
                })?;
        let expected =
            u32::from_str_radix(checksum.trim(), 16).map_err(|_| BatchError::CorruptSnapshot {
                what: "manifest checksum is not hex",
            })?;
        if schweep_log::record::crc32(body.as_bytes()) != expected {
            return Err(BatchError::CorruptSnapshot {
                what: "manifest failed its own checksum",
            });
        }

        let mut epoch = None;
        let mut tables = BTreeMap::new();
        let mut dedup_crc = None;
        for line in body.lines() {
            if let Some(value) = line.strip_prefix("epoch=") {
                epoch = value.parse().ok();
            } else if let Some(value) = line.strip_prefix("dedup=") {
                dedup_crc = u32::from_str_radix(value.trim(), 16).ok();
            } else if line.starts_with("table=") {
                let mut name = None;
                let mut rows = None;
                let mut crc = None;
                for part in line.split_whitespace() {
                    match part.split_once('=') {
                        Some(("table", value)) => name = Some(value.to_owned()),
                        Some(("rows", value)) => rows = value.parse::<usize>().ok(),
                        Some(("crc", value)) => crc = u32::from_str_radix(value, 16).ok(),
                        _ => {}
                    }
                }
                if let (Some(name), Some(rows), Some(crc)) = (name, rows, crc) {
                    tables.insert(name, (rows, crc));
                }
            }
        }
        Ok(Manifest {
            epoch: epoch.ok_or(BatchError::CorruptSnapshot {
                what: "manifest names no epoch",
            })?,
            tables,
            dedup_crc: dedup_crc.ok_or(BatchError::CorruptSnapshot {
                what: "manifest names no dedup checksum",
            })?,
        })
    }
}

/// Write one table's integral as Parquet: its own columns, then `__weight`.
pub fn write_table(path: &Path, integral: &ZSetBatch) -> Result<usize> {
    let schema = integral.schema();
    if schema.index_of(WEIGHT_COLUMN).is_some() {
        return Err(BatchError::ReservedColumn {
            column: WEIGHT_COLUMN,
        });
    }
    // Consolidate here rather than trusting the caller: a snapshot is a statement about what is
    // *present*, and a duplicate or zero-weight entry would make it a different statement (S-5).
    let consolidated = integral.consolidate()?;

    let mut fields: Vec<ArrowField> = schema
        .to_arrow()
        .fields()
        .iter()
        .map(|field| field.as_ref().clone())
        .collect();
    fields.push(ArrowField::new(WEIGHT_COLUMN, ArrowType::Int64, false));
    let file_schema = Arc::new(ArrowSchema::new(fields));

    let mut columns: Vec<arrow_array::ArrayRef> = consolidated.record_batch().columns().to_vec();
    columns.push(Arc::new(consolidated.weights().clone()));
    let batch = RecordBatch::try_new(Arc::clone(&file_schema), columns)
        .map_err(|e| BatchError::Arrow(e.to_string()))?;

    let file = fs::File::create(path)?;
    let mut writer = ArrowWriter::try_new(file, file_schema, None)
        .map_err(|e| BatchError::Parquet(e.to_string()))?;
    writer
        .write(&batch)
        .map_err(|e| BatchError::Parquet(e.to_string()))?;
    writer
        .close()
        .map_err(|e| BatchError::Parquet(e.to_string()))?;
    Ok(consolidated.len())
}

/// Read one table's integral back, checking it against the schema the catalog declares.
pub fn read_table(path: &Path, schema: &Schema) -> Result<ZSetBatch> {
    let file = fs::File::open(path)?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| BatchError::Parquet(e.to_string()))?
        .build()
        .map_err(|e| BatchError::Parquet(e.to_string()))?;

    let mut entries: Vec<(schweep_zset::Row, i64)> = Vec::new();
    for batch in reader {
        let batch = batch.map_err(|e| BatchError::Arrow(e.to_string()))?;
        let rows = decode_batch(batch, schema)?;
        entries.extend(rows.entries()?);
    }
    Ok(ZSetBatch::from_entries(schema.clone(), entries)?)
}

fn decode_batch(batch: RecordBatch, schema: &Schema) -> Result<ZSetBatch> {
    let weight_index =
        batch
            .schema()
            .index_of(WEIGHT_COLUMN)
            .map_err(|_| BatchError::CorruptSnapshot {
                what: "a snapshot file has no __weight column",
            })?;
    let weights = batch
        .column(weight_index)
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or(BatchError::CorruptSnapshot {
            what: "__weight is not an Int64 column",
        })?;
    let data = batch
        .project(&(0..weight_index).collect::<Vec<_>>())
        .map_err(|e| BatchError::Arrow(e.to_string()))?;
    Ok(ZSetBatch::from_arrow(
        schema.clone(),
        data,
        weights.clone(),
    )?)
}

#[derive(Debug)]
struct TableDescriptor {
    table: String,
    path: PathBuf,
    schema: Schema,
    expected_rows: usize,
}

struct CurrentTable {
    descriptor: TableDescriptor,
    reader: ParquetRecordBatchReader,
    rows: usize,
}

/// Snapshot contents as bounded Arrow record-batch deltas.
///
/// Only one Parquet reader and one record batch are resident. This is the bootstrap half of C10's
/// server-compaction fix: a 500 GiB snapshot must not require a 500 GiB `EpochDeltas` allocation just
/// because a standing query is being rebuilt.
pub struct SnapshotChunks {
    pending: std::vec::IntoIter<TableDescriptor>,
    current: Option<CurrentTable>,
    failed: bool,
}

impl std::fmt::Debug for SnapshotChunks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotChunks")
            .field("tables_pending", &self.pending.len())
            .field("table_open", &self.current.is_some())
            .field("failed", &self.failed)
            .finish()
    }
}

impl Iterator for SnapshotChunks {
    type Item = Result<EpochDeltas>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        loop {
            if let Some(current) = self.current.as_mut() {
                match current.reader.next() {
                    Some(Ok(batch)) => {
                        let rows = match decode_batch(batch, &current.descriptor.schema) {
                            Ok(rows) => rows,
                            Err(error) => {
                                self.failed = true;
                                return Some(Err(error));
                            }
                        };
                        current.rows += rows.len();
                        if current.rows > current.descriptor.expected_rows {
                            self.failed = true;
                            return Some(Err(BatchError::CorruptSnapshot {
                                what: "a snapshot file holds more rows than its manifest claims",
                            }));
                        }
                        let entries = match rows.entries() {
                            Ok(entries) => entries,
                            Err(error) => {
                                self.failed = true;
                                return Some(Err(error.into()));
                            }
                        };
                        let mut deltas = EpochDeltas::new();
                        deltas.extend(current.descriptor.table.clone(), entries);
                        return Some(Ok(deltas));
                    }
                    Some(Err(error)) => {
                        self.failed = true;
                        return Some(Err(BatchError::Arrow(error.to_string())));
                    }
                    None => {
                        if current.rows != current.descriptor.expected_rows {
                            self.failed = true;
                            return Some(Err(BatchError::CorruptSnapshot {
                                what: "a snapshot file holds a different number of rows than its manifest claims",
                            }));
                        }
                        self.current = None;
                    }
                }
            } else {
                let descriptor = self.pending.next()?;
                let file = match fs::File::open(&descriptor.path) {
                    Ok(file) => file,
                    Err(error) => {
                        self.failed = true;
                        return Some(Err(error.into()));
                    }
                };
                let reader = match ParquetRecordBatchReaderBuilder::try_new(file)
                    .and_then(|builder| builder.with_batch_size(1_024).build())
                {
                    Ok(reader) => reader,
                    Err(error) => {
                        self.failed = true;
                        return Some(Err(BatchError::Parquet(error.to_string())));
                    }
                };
                self.current = Some(CurrentTable {
                    descriptor,
                    reader,
                    rows: 0,
                });
            }
        }
    }
}

fn file_crc32(path: &Path) -> Result<u32> {
    let mut file = fs::File::open(path)?;
    let mut checksum = schweep_log::record::Crc32::new();
    let mut buffer = [0_u8; 64 * 1_024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let bytes = buffer.get(..read).ok_or(BatchError::CorruptSnapshot {
            what: "a checksum read exceeded its fixed buffer",
        })?;
        checksum.update(bytes);
    }
    Ok(checksum.finish())
}

/// Verify a snapshot, then stream it one bounded Parquet record batch at a time.
pub fn chunks(dir: &Path, catalog: &BTreeMap<String, Schema>) -> Result<SnapshotChunks> {
    let manifest = Manifest::decode(&fs::read(dir.join(MANIFEST))?)?;
    let mut pending = Vec::with_capacity(manifest.tables.len());
    for (table, (rows, crc)) in manifest.tables {
        let schema = catalog
            .get(&table)
            .cloned()
            .ok_or_else(|| BatchError::UnknownTable {
                table: table.clone(),
            })?;
        let path = table_path(dir, &table);
        if file_crc32(&path)? != crc {
            return Err(BatchError::CorruptSnapshot {
                what: "a snapshot file failed its manifest checksum",
            });
        }
        pending.push(TableDescriptor {
            table,
            path,
            schema,
            expected_rows: rows,
        });
    }
    Ok(SnapshotChunks {
        pending: pending.into_iter(),
        current: None,
        failed: false,
    })
}

/// The path of one table's file inside a snapshot directory.
#[must_use]
pub fn table_path(dir: &Path, table: &str) -> PathBuf {
    dir.join(format!("{table}.parquet"))
}

/// Load and verify a whole snapshot: every table's integral, checked against the manifest.
///
/// Verification is not optional politeness. A snapshot is authoritative for the data the log no longer
/// holds, so a torn one that read as merely *small* would silently lose committed rows — which is
/// exactly the failure a checksum turns into a refusal.
pub fn load(dir: &Path, catalog: &BTreeMap<String, Schema>) -> Result<BTreeMap<String, ZSetBatch>> {
    let manifest = Manifest::decode(&fs::read(dir.join(MANIFEST))?)?;
    let mut out = BTreeMap::new();
    for (table, (rows, crc)) in &manifest.tables {
        let schema = catalog.get(table).ok_or_else(|| BatchError::UnknownTable {
            table: table.clone(),
        })?;
        let path = table_path(dir, table);
        let bytes = fs::read(&path)?;
        if schweep_log::record::crc32(&bytes) != *crc {
            return Err(BatchError::CorruptSnapshot {
                what: "a snapshot file failed its manifest checksum",
            });
        }
        let integral = read_table(&path, schema)?;
        if integral.len() != *rows {
            return Err(BatchError::CorruptSnapshot {
                what: "a snapshot file holds a different number of rows than the manifest claims",
            });
        }
        out.insert(table.clone(), integral);
    }
    Ok(out)
}

/// The manifest of a published snapshot, or `None` if the directory is not one.
pub fn manifest_of(dir: &Path) -> Result<Option<Manifest>> {
    match fs::read(dir.join(MANIFEST)) {
        Ok(bytes) => Ok(Some(Manifest::decode(&bytes)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use schweep_zset::{DataType, Field, Row, Value};

    fn schema() -> Schema {
        Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::nullable("s", DataType::Utf8),
            Field::nullable("b", DataType::Boolean),
        ])
        .unwrap()
    }

    fn row(id: i64, s: Option<&str>, b: Option<bool>) -> Row {
        Row::new(vec![
            Value::Int(id),
            s.map_or(Value::Null, |s| Value::Str(s.to_owned())),
            b.map_or(Value::Null, Value::Bool),
        ])
    }

    #[test]
    fn a_table_round_trips_through_parquet() {
        let dir = std::env::temp_dir().join(format!("schweep-snap-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = table_path(&dir, "t");

        let integral = ZSetBatch::from_entries(
            schema(),
            vec![
                (row(1, Some("a"), Some(true)), 2),
                (row(2, None, None), -3),
                (row(3, Some("with, comma"), Some(false)), 1),
            ],
        )
        .unwrap();
        let written = write_table(&path, &integral).unwrap();
        assert_eq!(written, 3);

        let read = read_table(&path, &schema()).unwrap();
        assert_eq!(
            read.canonical().unwrap().render(),
            integral.canonical().unwrap().render(),
            "the integral must come back byte for byte, negative weights and nulls included"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_snapshot_streams_in_bounded_record_batches_and_preserves_every_row() {
        let dir = std::env::temp_dir().join(format!("schweep-snap-stream-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = table_path(&dir, "t");
        let entries: Vec<_> = (0..2_050)
            .map(|id| (row(id, Some("streamed"), Some(true)), 1))
            .collect();
        let integral = ZSetBatch::from_entries(schema(), entries).unwrap();
        let rows = write_table(&path, &integral).unwrap();
        let bytes = fs::read(&path).unwrap();
        let manifest = Manifest {
            epoch: 7,
            tables: BTreeMap::from([("t".to_owned(), (rows, schweep_log::record::crc32(&bytes)))]),
            dedup_crc: 0,
        };
        fs::write(dir.join(MANIFEST), manifest.encode()).unwrap();

        let catalog = BTreeMap::from([("t".to_owned(), schema())]);
        let streamed: Vec<_> = chunks(&dir, &catalog)
            .unwrap()
            .map(|chunk| chunk.unwrap())
            .collect();
        assert!(
            streamed.len() >= 3,
            "2,050 rows at a 1,024-row bound must not be returned as one allocation"
        );
        let seen: usize = streamed
            .iter()
            .map(|chunk| chunk.tables().values().map(Vec::len).sum::<usize>())
            .sum();
        assert_eq!(seen, 2_050);
    }

    /// Zero-weight rows are not present, so they are not written (S-4, S-5).
    #[test]
    fn consolidation_drops_what_is_not_there() {
        let dir = std::env::temp_dir().join(format!("schweep-snap-zero-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = table_path(&dir, "t");

        let integral = ZSetBatch::from_entries(
            schema(),
            vec![
                (row(1, None, None), 1),
                (row(1, None, None), -1),
                (row(2, None, None), 3),
                (row(2, None, None), -1),
            ],
        )
        .unwrap();
        assert_eq!(
            write_table(&path, &integral).unwrap(),
            1,
            "one row survives"
        );
        let read = read_table(&path, &schema()).unwrap();
        assert_eq!(
            read.canonical().unwrap().render(),
            "(id: Int64 NOT NULL, s: Utf8, b: Boolean)\n(2, NULL, NULL) => 2\n"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_reserved_column_name_is_refused() {
        let dir = std::env::temp_dir().join(format!("schweep-snap-res-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let odd = Schema::new(vec![Field::new(WEIGHT_COLUMN, DataType::Int64, false)]).unwrap();
        let integral = ZSetBatch::from_entries(odd, vec![]).unwrap();
        assert!(matches!(
            write_table(&table_path(&dir, "t"), &integral),
            Err(BatchError::ReservedColumn { .. })
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_manifest_round_trips_and_detects_damage() {
        let manifest = Manifest {
            epoch: 4,
            tables: BTreeMap::from([("t".to_owned(), (3usize, 0xabcd_1234u32))]),
            dedup_crc: 7,
        };
        let bytes = manifest.encode();
        assert_eq!(Manifest::decode(&bytes).unwrap(), manifest);

        let mut damaged = bytes.clone();
        if let Some(byte) = damaged.get_mut(30) {
            *byte ^= 0xFF;
        }
        assert!(Manifest::decode(&damaged).is_err());
    }
}
