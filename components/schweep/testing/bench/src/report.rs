//! What a benchmark hands to the ledger, and what the ledger hands to a reader.
//!
//! Every field here exists because a number without it has been misread by somebody: which machine, how
//! many rounds, the full range and not only the median, and the **slowest** run given equal billing with
//! the median because that is the figure this project quotes when it quotes one (§6 C10, I-10).

use std::fmt;

use crate::timer::Sample;
use crate::units::{Bytes, Count, Nanos};

/// Where a measurement was taken. Machine-dependent numbers are the norm in this crate, so the machine
/// travels with them rather than being remembered.
#[derive(Clone, Debug)]
pub struct Machine {
    pub description: String,
    pub profile: &'static str,
}

impl Machine {
    /// The machine as far as a portable program can tell, which is not far — and the honest form of "not
    /// far" is to say what is known and stop, rather than to guess at a model name.
    #[must_use]
    pub fn detect() -> Machine {
        let profile = if cfg!(debug_assertions) {
            "debug (NOT a performance measurement — build with --release)"
        } else {
            "release"
        };
        Machine {
            description: format!(
                "{} {} · {} logical cpus",
                std::env::consts::OS,
                std::env::consts::ARCH,
                std::thread::available_parallelism().map_or(0, std::num::NonZero::get)
            ),
            profile,
        }
    }

    /// Is this a build whose timings mean anything? A debug build's numbers are not slow versions of the
    /// release numbers, they are a different shape, and quoting them would be worse than quoting nothing.
    #[must_use]
    pub fn is_a_performance_build(&self) -> bool {
        !cfg!(debug_assertions)
    }
}

impl fmt::Display for Machine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} · {}", self.description, self.profile)
    }
}

/// One benchmark's result, ready to be printed or written into an artifact.
#[derive(Clone, Debug)]
pub struct Benchmark {
    pub name: String,
    pub machine: Machine,
    pub sample: Sample,
    /// What one round did, in the workload's own unit — rows applied, queries registered, epochs sealed.
    pub work_unit: String,
    /// Resident memory at the end of the run, when the benchmark measured it.
    pub rss: Option<Bytes>,
    /// Lines a reader needs in order to know what was measured: the shape, the data size, the settings.
    pub notes: Vec<String>,
}

impl Benchmark {
    #[must_use]
    pub fn new(name: impl Into<String>, sample: Sample, work_unit: impl Into<String>) -> Benchmark {
        Benchmark {
            name: name.into(),
            machine: Machine::detect(),
            sample,
            work_unit: work_unit.into(),
            rss: None,
            notes: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_note(mut self, note: impl Into<String>) -> Benchmark {
        self.notes.push(note.into());
        self
    }

    #[must_use]
    pub fn with_rss(mut self, rss: Bytes) -> Benchmark {
        self.rss = Some(rss);
        self
    }

    /// Cost per unit of work, from the **slowest** round rather than the median.
    ///
    /// The worst-honest rule, applied where it is easiest to break: a per-unit cost from the fastest round
    /// is the number that ends up in a headline.
    #[must_use]
    pub fn slowest_per_work(&self) -> Option<f64> {
        let slowest = self.sample.slowest()?;
        let work = self
            .sample
            .rounds
            .iter()
            .map(|r| r.work.0)
            .max()
            .unwrap_or(0);
        slowest.per(Count(work)).map(|p| p.each)
    }

    #[must_use]
    pub fn median_per_work(&self) -> Option<f64> {
        let median = self.sample.median()?;
        let work = self.sample.rounds.first().map_or(Count(0), |r| r.work);
        median.per(work).map(|p| p.each)
    }

    /// The human-readable block, which leads with the caveat when there is one.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        if !self.machine.is_a_performance_build() {
            out.push_str(
                "!! DEBUG BUILD — these timings are not a performance measurement and must not be \
                 quoted.\n",
            );
        }
        out.push_str(&format!("{}\n", self.name));
        out.push_str(&format!("  machine: {}\n", self.machine));
        out.push_str(&format!("  {}\n", self.sample.render()));
        if let (Some(median), Some(slowest)) = (self.median_per_work(), self.slowest_per_work()) {
            out.push_str(&format!(
                "  per {}: {median:.1} ns median · {slowest:.1} ns at the slowest round\n",
                self.work_unit
            ));
        }
        if let Some(rss) = self.rss {
            out.push_str(&format!("  resident memory: {}\n", rss.describe()));
        }
        for note in &self.notes {
            out.push_str(&format!("  {note}\n"));
        }
        out
    }

    /// The artifact form. Hand-written JSON, because the workspace has no serde and adding one so a
    /// benchmark can write a file it also reads would be the tail wagging the dog.
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut out = format!("    {{\n      \"name\": {},\n", quote(&self.name));
        out.push_str(&format!(
            "      \"machine\": {},\n      \"profile\": {},\n",
            quote(&self.machine.description),
            quote(self.machine.profile)
        ));
        out.push_str(&format!("      \"rounds\": {},\n", self.sample.len()));
        out.push_str(&format!(
            "      \"median_nanos\": {},\n      \"fastest_nanos\": {},\n      \"slowest_nanos\": {},\n",
            self.sample.median().unwrap_or(Nanos(0)).0,
            self.sample.fastest().unwrap_or(Nanos(0)).0,
            self.sample.slowest().unwrap_or(Nanos(0)).0
        ));
        if let Some(spread) = self.sample.spread() {
            out.push_str(&format!("      \"spread\": {:.3},\n", spread.get()));
        }
        out.push_str(&format!(
            "      \"work_unit\": {},\n      \"work_per_round\": {},\n",
            quote(&self.work_unit),
            self.sample.rounds.first().map_or(0, |r| r.work.0)
        ));
        if let Some(per) = self.slowest_per_work() {
            out.push_str(&format!("      \"slowest_nanos_per_work\": {per:.2},\n"));
        }
        if let Some(rss) = self.rss {
            out.push_str(&format!("      \"rss_bytes\": {},\n", rss.0));
        }
        out.push_str("      \"notes\": [\n");
        for (index, note) in self.notes.iter().enumerate() {
            out.push_str(&format!(
                "        {}{}\n",
                quote(note),
                if index + 1 == self.notes.len() {
                    ""
                } else {
                    ","
                }
            ));
        }
        out.push_str("      ]\n    }");
        out
    }
}

fn quote(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}
