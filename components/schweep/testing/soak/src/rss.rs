//! Sampling resident memory, and finding out whether there is a ceiling at all (§6 C8).
//!
//! The C8 gate is about the *shape* of the memory curve, not its endpoints: a run that starts at 40 MiB
//! and ends at 40 MiB may have leaked steadily and been reclaimed by luck, and a run that ends higher
//! than it started may simply have grown a cache to its bound. So RSS is sampled throughout and the
//! samples are compared as a series.
//!
//! ## Why the ceiling is read rather than assumed
//!
//! > the gate runs in CI at a fixed memory ceiling (cgroup), not on whatever the runner has free
//! > — §6 C8's pitfall
//!
//! A test that merely *hopes* it is running under a ceiling is a test that passes on a 64 GiB machine
//! having proved nothing. So [`ceiling`] reads the cgroup's own limit, and the gate **refuses to claim
//! anything** when there is none: it runs a reduced shape, prints that it is not a gate, and the CI job
//! is what supplies the ceiling. The job's only responsibility is to apply it; the test's is to check.

use std::path::PathBuf;

/// Where a memory ceiling came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ceiling {
    /// A cgroup v2 limit, in bytes, read from the cgroup this process is in.
    Cgroup { bytes: u64, path: PathBuf },
    /// No limit is in force. The gate must not claim to have proven anything.
    Unlimited,
}

impl Ceiling {
    #[must_use]
    pub fn bytes(&self) -> Option<u64> {
        match self {
            Ceiling::Cgroup { bytes, .. } => Some(*bytes),
            Ceiling::Unlimited => None,
        }
    }

    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Ceiling::Cgroup { bytes, path } => {
                format!("{bytes} bytes, from {}", path.display())
            }
            Ceiling::Unlimited => "no ceiling in force".to_owned(),
        }
    }
}

/// The memory ceiling this process is running under, if any.
///
/// cgroup v2 only. v1's hierarchy is a maze and the CI runner is v2; a v1 machine reads as
/// `Unlimited`, which makes the gate decline to claim rather than claim wrongly.
#[must_use]
pub fn ceiling() -> Ceiling {
    let Ok(own) = std::fs::read_to_string("/proc/self/cgroup") else {
        return Ceiling::Unlimited;
    };
    // v2 format: `0::/the/path`
    let relative = own
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .unwrap_or("/")
        .trim();

    // Walk from the process's own cgroup up to the root: a limit set on an ancestor applies here too,
    // and `systemd-run --scope -p MemoryMax=` sets it on a scope that may be an ancestor.
    let mut candidate = PathBuf::from("/sys/fs/cgroup").join(relative.trim_start_matches('/'));
    loop {
        let file = candidate.join("memory.max");
        if let Ok(text) = std::fs::read_to_string(&file) {
            let text = text.trim();
            if text != "max" {
                if let Ok(bytes) = text.parse::<u64>() {
                    return Ceiling::Cgroup { bytes, path: file };
                }
            }
        }
        if !candidate.pop() || !candidate.starts_with("/sys/fs/cgroup") {
            return Ceiling::Unlimited;
        }
    }
}

/// Resident set size, in bytes, right now.
///
/// `/proc/self/statm` on Linux; `ps` elsewhere. No `unsafe`, no FFI: the `mach` call that would give
/// this directly on macOS needs `unsafe`, which is forbidden until C10 and under an inventory
/// discipline when it arrives — and a test harness is not the place to spend that budget.
#[must_use]
pub fn rss_bytes() -> Option<u64> {
    if let Ok(statm) = std::fs::read_to_string("/proc/self/statm") {
        // Fields are in pages: size, resident, shared, …
        let resident: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        return Some(resident * STATM_PAGE_BYTES);
    }
    let pid = std::process::id();
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    // `ps` reports kibibytes.
    text.trim().parse::<u64>().ok().map(|kib| kib * 1024)
}

/// The page size `statm`'s fields are counted in.
///
/// 4 KiB on every platform that has `/proc/self/statm`, which is the only platform this is used on: the
/// `ps` path below reports kibibytes directly and never reaches here. Hard-coded rather than read,
/// because reading it means `libc::sysconf` — an FFI call, and therefore `unsafe`, which is forbidden
/// until C10 and under an inventory discipline when it arrives. A test harness is not where that budget
/// gets spent.
const STATM_PAGE_BYTES: u64 = 4096;

/// A series of RSS samples, and the questions the gate asks of it.
#[derive(Clone, Debug, Default)]
pub struct Curve {
    pub samples: Vec<u64>,
}

impl Curve {
    pub fn sample(&mut self) {
        if let Some(bytes) = rss_bytes() {
            self.samples.push(bytes);
        }
    }

    #[must_use]
    pub fn peak(&self) -> u64 {
        self.samples.iter().copied().max().unwrap_or(0)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// The samples after the warm-up prefix.
    ///
    /// **Warm-up is real and it is not a leak.** A process that has just started has not yet grown its
    /// allocator arenas, filled its caches, or reached the working set the workload needs; measured on
    /// this workload the first fifth of the run climbs steeply and then plateaus. Asking a run to be flat
    /// including its warm-up would be asking it to be born at steady state.
    ///
    /// So a **stated** prefix is excluded — [`Curve::WARM_UP_SAMPLES`] — and everything after it must be
    /// flat. What it costs is that a leak confined entirely to those first samples would be missed; what
    /// it buys is a gate that fails for leaks rather than for starting up.
    #[must_use]
    pub fn steady_state(&self) -> &[u64] {
        self.samples.get(Curve::WARM_UP_SAMPLES..).unwrap_or(&[])
    }

    /// Samples treated as warm-up: a fixed **count**, not a fraction.
    ///
    /// Measured on the C8 ceiling shape: resident memory climbs from 6 MiB to ~34 MiB over roughly the
    /// first forty epochs — redb's page cache filling toward its bound — and is then flat to the end of a
    /// 313-epoch run that took operator state to 1.08 GB. That climb is a fixed *duration*, so excluding a
    /// fixed *fraction* would exclude too little on a short run and far too much on a long one. Forty-eight
    /// is above the measured forty with room to spare, and is a seventh of the run the CI gate performs.
    pub const WARM_UP_SAMPLES: usize = 48;

    /// The mean of the first quarter of the **steady state**, and of the last quarter.
    ///
    /// **The shape, not the endpoints.** Two numbers taken at the ends can agree while everything
    /// between them climbs; quartile means over a long run cannot.
    #[must_use]
    pub fn quartile_means(&self) -> Option<(f64, f64)> {
        let samples = self.steady_state();
        if samples.len() < 8 {
            return None;
        }
        let quarter = samples.len() / 4;
        let first: u64 = samples.get(..quarter)?.iter().sum();
        let last: u64 = samples.get(samples.len() - quarter..)?.iter().sum();
        Some((first as f64 / quarter as f64, last as f64 / quarter as f64))
    }

    /// How much the last quarter exceeds the first, as a fraction. Negative means it shrank.
    #[must_use]
    pub fn growth(&self) -> Option<f64> {
        let (first, last) = self.quartile_means()?;
        if first <= 0.0 {
            return None;
        }
        Some((last - first) / first)
    }

    /// Is the curve monotonically climbing across quarters? A leak's signature.
    ///
    /// Checked in addition to `growth`, because a leak that is small relative to the baseline can hide
    /// inside a generous growth allowance while still climbing at every step.
    #[must_use]
    pub fn climbs_every_quarter(&self) -> bool {
        let samples = self.steady_state();
        if samples.len() < 8 {
            return false;
        }
        let quarter = samples.len() / 4;
        let mut means = Vec::new();
        for index in 0..4 {
            let from = index * quarter;
            let slice = match samples.get(from..from + quarter) {
                Some(slice) => slice,
                None => return false,
            };
            means.push(slice.iter().sum::<u64>() as f64 / quarter as f64);
        }
        means.windows(2).all(|pair| match pair {
            [a, b] => b > a,
            _ => false,
        })
    }

    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!(
            "{} samples ({} after warm-up) · peak {} bytes",
            self.samples.len(),
            self.steady_state().len(),
            self.peak()
        );
        if let Some((first, last)) = self.quartile_means() {
            out.push_str(&format!(
                " · first quarter mean {first:.0} · last quarter mean {last:.0}"
            ));
        }
        if let Some(growth) = self.growth() {
            out.push_str(&format!(" · growth {:+.1}%", growth * 100.0));
        }
        out
    }
}
