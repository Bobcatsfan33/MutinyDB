//! Choosing a backend without naming one (`ARCHITECTURE.md` §5.5, C8).
//!
//! Until C8 there was one implementation, so every stateful operator wrote `MemBackend::new()` and the
//! choice was not a choice. With two implementations it has to be made somewhere, and the somewhere
//! must not be inside the operators: an operator that knew which store it had would be an operator that
//! could behave differently on each, and the backend-invariance gate exists precisely to assert that
//! none of them can.
//!
//! So a factory is threaded from the caller that builds a circuit down to the operators, and it hands
//! out backends by **label** — `n3-join-left`, `n5-aggregate`. The labels are what `EXPLAIN STATE`
//! reports against and what the reconciliation gate maps to files on disk; without them, per-operator
//! accounting would be a list of numbers with nothing to attach them to.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::backend::StateBackend;
use crate::error::Result;
use crate::mem::MemBackend;
use crate::redb_backend::RedbBackend;

/// Something that can hand a stateful operator its store.
pub trait BackendFactory: fmt::Debug + Send {
    /// A backend for the operator called `label`. Labels are unique within one circuit.
    fn create(&mut self, label: &str) -> Result<Box<dyn StateBackend>>;

    /// What this factory is, for fingerprints and for `EXPLAIN STATE`'s header.
    fn describe(&self) -> String;

    /// The labels handed out so far, and where each one's state lives if it lives anywhere.
    ///
    /// `None` for an in-memory backend: there is no file, and saying so is better than inventing a
    /// path. The reconciliation gate uses this to compare reported entries against real bytes.
    fn handed_out(&self) -> Vec<(String, Option<PathBuf>)>;
}

/// The in-memory factory: what every circuit got before C8, and what the oracle and most tests still
/// get.
#[derive(Debug, Default)]
pub struct MemFactory {
    labels: Vec<String>,
}

impl MemFactory {
    #[must_use]
    pub fn new() -> MemFactory {
        MemFactory::default()
    }
}

impl BackendFactory for MemFactory {
    fn create(&mut self, label: &str) -> Result<Box<dyn StateBackend>> {
        self.labels.push(label.to_owned());
        Ok(Box::new(MemBackend::new()))
    }

    fn describe(&self) -> String {
        "MemBackend (in memory)".to_owned()
    }

    fn handed_out(&self) -> Vec<(String, Option<PathBuf>)> {
        self.labels.iter().map(|l| (l.clone(), None)).collect()
    }
}

/// The durable factory: one redb file per operator, under one directory.
///
/// One file each rather than one file with a table each, because the trait has no notion of naming a
/// region inside a store — and inventing one here would be inventing an interface the frozen trait
/// deliberately does not have. One file per operator also makes the reconciliation gate's job
/// arithmetic rather than inference: this operator's state is that file's size.
#[derive(Debug)]
pub struct RedbFactory {
    root: PathBuf,
    handed: Vec<(String, PathBuf)>,
}

impl RedbFactory {
    pub fn new(root: impl AsRef<Path>) -> RedbFactory {
        RedbFactory {
            root: root.as_ref().to_path_buf(),
            handed: Vec::new(),
        }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Total bytes on disk across every backend this factory handed out.
    ///
    /// The "actual backend usage" side of `EXPLAIN STATE`'s reconciliation.
    pub fn bytes_on_disk(&self) -> Result<u64> {
        let mut total = 0u64;
        for (_, path) in &self.handed {
            if let Ok(meta) = std::fs::metadata(path) {
                total += meta.len();
            }
        }
        Ok(total)
    }
}

impl BackendFactory for RedbFactory {
    fn create(&mut self, label: &str) -> Result<Box<dyn StateBackend>> {
        // The label is in the filename, so a directory of spill files can be read by a person.
        let path = self.root.join(format!("{label}.redb"));
        let backend = RedbBackend::open(&path)?;
        self.handed.push((label.to_owned(), path));
        Ok(Box::new(backend))
    }

    fn describe(&self) -> String {
        format!("RedbBackend (spilled to {})", self.root.display())
    }

    fn handed_out(&self) -> Vec<(String, Option<PathBuf>)> {
        self.handed
            .iter()
            .map(|(label, path)| (label.clone(), Some(path.clone())))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::backend::WriteBatch;
    use schweep_zset::Value;

    #[test]
    fn a_mem_factory_hands_out_backends_with_no_files() {
        let mut factory = MemFactory::new();
        let mut backend = factory.create("n1-join-left").unwrap();
        let mut batch = WriteBatch::new();
        batch.add(vec![Value::Int(1)], 1);
        backend.write(&batch).unwrap();
        assert_eq!(backend.len(), 1);
        assert_eq!(
            factory.handed_out(),
            vec![("n1-join-left".to_owned(), None)],
            "there is no file, and the factory says so rather than inventing one"
        );
    }

    #[test]
    fn a_redb_factory_puts_each_label_in_its_own_file() {
        let root = std::env::temp_dir().join(format!("schweep-factory-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut factory = RedbFactory::new(&root);

        let mut left = factory.create("n1-join-left").unwrap();
        let _right = factory.create("n1-join-right").unwrap();
        let mut batch = WriteBatch::new();
        batch.add(vec![Value::Int(1), Value::Str("x".into())], 2);
        left.write(&batch).unwrap();

        let handed = factory.handed_out();
        assert_eq!(handed.len(), 2);
        for (label, path) in &handed {
            let path = path.as_ref().expect("a redb backend has a file");
            assert!(path.exists(), "{label} has no file at {path:?}");
            assert!(path.to_string_lossy().contains(label));
        }
        assert!(
            factory.bytes_on_disk().unwrap() > 0,
            "state on disk must occupy bytes on disk"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
