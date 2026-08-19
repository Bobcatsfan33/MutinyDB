//! The frozen incident corpus, parsed. The committed TSV is authoritative; this module only
//! translates its lines into typed values and refuses anything it does not recognize, because a
//! loader that guesses is a second, unfrozen corpus.

use loom_core::SourceRef;
use mutiny_taint::{KeyType, TaintConfig, TaintTableSpec};
use schweep_zset::{DataType, Field, Row, Schema, Value};
use std::collections::{BTreeMap, BTreeSet};

/// The exact committed corpus bytes. The gate pins their BLAKE3 checksum.
pub const CORPUS: &str = include_str!("../fixtures/incident-corpus.tsv");

pub const TENANT: &str = "acme";
pub const CLAIMS: &str = "claims";
pub const TELEMETRY: &str = "telemetry";

/// One corpus write: exactly one storage commit, one row, one envelope.
#[derive(Clone, Debug)]
pub struct CorpusCommit {
    pub session: String,
    pub branch: String,
    pub actor: String,
    pub table: String,
    pub sources: Vec<SourceRef>,
    pub key: String,
    pub row: Row,
}

/// One executed external action, driven through Loom's real gateway by the host.
#[derive(Clone, Debug)]
pub struct CorpusAction {
    pub branch: String,
    pub actor: String,
    pub action_type: String,
    pub target: String,
    pub idempotency_key: String,
    pub justified_by: Vec<String>,
}

/// The parsed corpus.
#[derive(Clone, Debug, Default)]
pub struct Corpus {
    pub sessions: Vec<String>,
    pub forks: Vec<(String, String)>,
    pub commits: Vec<CorpusCommit>,
    pub actions: Vec<CorpusAction>,
    /// `system:record` → the (branch, table, key) rows the corpus declares downstream of it.
    pub downstream: BTreeMap<String, BTreeSet<(String, String, String)>>,
}

#[derive(Debug, thiserror::Error)]
#[error("corpus line {line}: {reason}")]
pub struct CorpusError {
    pub line: usize,
    pub reason: String,
}

/// The compute catalog the corpus runs against: both payload tables, the bridge's fixed
/// derivation relation, and the taint ledger.
pub fn catalog() -> Result<BTreeMap<String, Schema>, String> {
    let claims = Schema::new_table(vec![
        Field::not_null("claim_id", DataType::Utf8),
        Field::not_null("branch", DataType::Utf8),
        Field::not_null("subject", DataType::Utf8),
        Field::not_null("asserts", DataType::Utf8),
        Field::not_null("confidence_bp", DataType::Int64),
    ])
    .map_err(|error| error.to_string())?;
    let telemetry = Schema::new_table(vec![
        Field::not_null("event_id", DataType::Utf8),
        Field::not_null("branch", DataType::Utf8),
        Field::not_null("body", DataType::Utf8),
        Field::not_null("cost_micros", DataType::Int64),
        Field::not_null("error", DataType::Boolean),
        Field::not_null("event_time", DataType::Int64),
    ])
    .map_err(|error| error.to_string())?;
    Ok(BTreeMap::from([
        (CLAIMS.to_owned(), claims),
        (TELEMETRY.to_owned(), telemetry),
        (
            mutiny_bridge::DERIVATION_TABLE.to_owned(),
            mutiny_bridge::derivation_schema().map_err(|error| error.to_string())?,
        ),
        (
            mutiny_taint::LEDGER_TABLE.to_owned(),
            TaintConfig::ledger_schema().map_err(|error| error.to_string())?,
        ),
    ]))
}

/// The plane each payload table ingests through (MD-2 R6's channel naming).
#[must_use]
pub fn plane_of(table: &str) -> &'static str {
    match table {
        CLAIMS => "memory",
        _ => "events",
    }
}

/// The taint core's view of this corpus.
#[must_use]
pub fn taint_config() -> TaintConfig {
    TaintConfig {
        tenant: TENANT.to_owned(),
        tables: BTreeMap::from([
            (
                CLAIMS.to_owned(),
                TaintTableSpec {
                    plane: plane_of(CLAIMS).to_owned(),
                    key_column: "claim_id".to_owned(),
                    branch_column: "branch".to_owned(),
                    key_type: KeyType::Utf8,
                },
            ),
            (
                TELEMETRY.to_owned(),
                TaintTableSpec {
                    plane: plane_of(TELEMETRY).to_owned(),
                    key_column: "event_id".to_owned(),
                    branch_column: "branch".to_owned(),
                    key_type: KeyType::Utf8,
                },
            ),
        ]),
    }
}

/// Parse the committed corpus text.
pub fn parse(text: &str) -> Result<Corpus, CorpusError> {
    let mut corpus = Corpus::default();
    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let fail = |reason: String| CorpusError { line, reason };
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = trimmed.split('|').collect();
        match fields.first().copied() {
            Some("session") => {
                let [_, session] = fields[..] else {
                    return Err(fail("session takes exactly one field".to_owned()));
                };
                corpus.sessions.push(session.to_owned());
            }
            Some("fork") => {
                let [_, from, name] = fields[..] else {
                    return Err(fail("fork takes exactly two fields".to_owned()));
                };
                corpus.forks.push((from.to_owned(), name.to_owned()));
            }
            Some("commit") => {
                if fields.len() < 8 {
                    return Err(fail("commit is missing fields".to_owned()));
                }
                let (session, branch, actor, table, sources, key) = (
                    fields[1], fields[2], fields[3], fields[4], fields[5], fields[6],
                );
                let sources = sources
                    .split(',')
                    .map(parse_source)
                    .collect::<Result<Vec<_>, String>>()
                    .map_err(&fail)?;
                let payload = &fields[7..];
                let row =
                    match table {
                        TELEMETRY => {
                            let [body, cost, error, time] = payload else {
                                return Err(fail(
                                    "telemetry payload is body|cost|error|time".to_owned(),
                                ));
                            };
                            Row::new(vec![
                                Value::Str(key.to_owned()),
                                Value::Str(branch.to_owned()),
                                Value::Str((*body).to_owned()),
                                Value::Int(cost.parse().map_err(|_| {
                                    fail(format!("cost_micros {cost:?} is not an integer"))
                                })?),
                                Value::Bool(error.parse().map_err(|_| {
                                    fail(format!("error {error:?} is not a boolean"))
                                })?),
                                Value::Int(time.parse().map_err(|_| {
                                    fail(format!("event_time {time:?} is not an integer"))
                                })?),
                            ])
                        }
                        CLAIMS => {
                            let [subject, asserts, confidence] = payload else {
                                return Err(fail(
                                    "claims payload is subject|asserts|confidence_bp".to_owned(),
                                ));
                            };
                            Row::new(vec![
                                Value::Str(key.to_owned()),
                                Value::Str(branch.to_owned()),
                                Value::Str((*subject).to_owned()),
                                Value::Str((*asserts).to_owned()),
                                Value::Int(confidence.parse().map_err(|_| {
                                    fail(format!("confidence_bp {confidence:?} is not an integer"))
                                })?),
                            ])
                        }
                        other => return Err(fail(format!("unknown corpus table {other:?}"))),
                    };
                corpus.commits.push(CorpusCommit {
                    session: session.to_owned(),
                    branch: branch.to_owned(),
                    actor: actor.to_owned(),
                    table: table.to_owned(),
                    sources,
                    key: key.to_owned(),
                    row,
                });
            }
            Some("action") => {
                let [_, branch, actor, action_type, target, idempotency, justified] = fields[..]
                else {
                    return Err(fail("action is missing fields".to_owned()));
                };
                corpus.actions.push(CorpusAction {
                    branch: branch.to_owned(),
                    actor: actor.to_owned(),
                    action_type: action_type.to_owned(),
                    target: target.to_owned(),
                    idempotency_key: idempotency.to_owned(),
                    justified_by: justified.split(',').map(str::to_owned).collect(),
                });
            }
            Some("downstream") => {
                let [_, source, branch, table, key] = fields[..] else {
                    return Err(fail("downstream is missing fields".to_owned()));
                };
                corpus
                    .downstream
                    .entry(source.to_owned())
                    .or_default()
                    .insert((branch.to_owned(), table.to_owned(), key.to_owned()));
            }
            other => return Err(fail(format!("unknown corpus record {other:?}"))),
        }
    }
    Ok(corpus)
}

/// `system:record`, with the reserved internal form `mutiny:<table>/<key>` hex-encoding the key
/// per the M4 convention (`docs/M4-TAINT.md`).
fn parse_source(text: &str) -> Result<SourceRef, String> {
    let (system, record) = text
        .split_once(':')
        .ok_or_else(|| format!("source {text:?} is not system:record"))?;
    if system == mutiny_taint::INTERNAL_SOURCE_SYSTEM {
        let (table, key) = record
            .split_once('/')
            .ok_or_else(|| format!("internal source {text:?} is not mutiny:<table>/<key>"))?;
        return Ok(SourceRef::new(
            system,
            format!("{table}/{}", hex(key.as_bytes())),
        ));
    }
    Ok(SourceRef::new(system, record))
}

/// Lowercase hex, matching the bridge's derivation `row_key` encoding.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}
