//! Metrics and structured tracing (docs/M6-SURFACE.md's observability section).
//!
//! Counters and gauges in ordered maps rendered as Prometheus text — ordered so two scrapes of
//! the same state are byte-identical. Traces are one JSON object per line on stderr; wall-clock
//! timestamps are permitted here and only here, because logs are operational output and never an
//! engine input (Schweep D-6 holds inside the boundary).

use std::collections::BTreeMap;
use std::sync::Mutex;

#[derive(Debug, Default)]
pub struct Metrics {
    counters: Mutex<BTreeMap<String, u64>>,
    gauges: Mutex<BTreeMap<String, i64>>,
}

impl Metrics {
    pub fn inc(&self, series: &str) {
        self.add(series, 1);
    }

    pub fn add(&self, series: &str, by: u64) {
        if let Ok(mut counters) = self.counters.lock() {
            *counters.entry(series.to_owned()).or_insert(0) += by;
        }
    }

    pub fn gauge(&self, series: &str, value: i64) {
        if let Ok(mut gauges) = self.gauges.lock() {
            gauges.insert(series.to_owned(), value);
        }
    }

    /// One counter's current value, for the gates that assert the admission ledger.
    #[must_use]
    pub fn counter(&self, series: &str) -> u64 {
        self.counters
            .lock()
            .ok()
            .and_then(|counters| counters.get(series).copied())
            .unwrap_or(0)
    }

    /// Prometheus text exposition, deterministically ordered.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        if let Ok(counters) = self.counters.lock() {
            for (series, value) in counters.iter() {
                out.push_str(series);
                out.push(' ');
                out.push_str(&value.to_string());
                out.push('\n');
            }
        }
        if let Ok(gauges) = self.gauges.lock() {
            for (series, value) in gauges.iter() {
                out.push_str(series);
                out.push(' ');
                out.push_str(&value.to_string());
                out.push('\n');
            }
        }
        out
    }
}

/// One structured trace line to stderr. Never load-bearing; never read back by the engine.
pub fn trace(event: &str, fields: &[(&str, String)]) {
    let mut object = serde_json::Map::new();
    object.insert(
        "ts_ms".to_owned(),
        serde_json::Value::from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        ),
    );
    object.insert("event".to_owned(), serde_json::Value::from(event));
    for (name, value) in fields {
        object.insert((*name).to_owned(), serde_json::Value::from(value.clone()));
    }
    eprintln!("{}", serde_json::Value::Object(object));
}
