//! Units, in the type (§6 C10's law).
//!
//! C9 shipped three assertions that were wrong about their own units — a fraction compared against `25.0`
//! and printed as a percentage, twice, and a slope that compared a one-quarter span against a two-quarter
//! span. All three *passed* while being wrong, which is the failure mode a green tick cannot show you. A
//! sprint whose entire output is numbers cannot afford a fourth.
//!
//! So a number here carries its unit in its type, and the types do not mix: [`Nanos`] plus [`Nanos`] is
//! [`Nanos`], [`Nanos`] plus [`Bytes`] does not compile, and a bare `f64` never crosses this crate's API.
//! A [`Ratio`] is the one dimensionless quantity, it can only be built by dividing two values of the *same*
//! unit, and it knows whether it came from a division or from a percentage — because "1.05" and "105%" are
//! the same number and not the same claim.

use std::fmt;
use std::ops::{Add, Div, Mul, Sub};

/// Elapsed time, in nanoseconds.
///
/// Nanoseconds rather than `Duration` because every arithmetic this crate does — medians, ratios, bands —
/// is integer arithmetic on a scalar, and `Duration`'s API invites the seconds/millis confusion this type
/// exists to prevent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Nanos(pub u64);

/// A size or a footprint, in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bytes(pub u64);

/// A count of things: rows, entries, epochs, queries, operator steps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Count(pub u64);

macro_rules! scalar_unit {
    ($name:ident, $suffix:literal) => {
        impl $name {
            #[must_use]
            pub fn get(self) -> u64 {
                self.0
            }

            #[must_use]
            pub fn is_zero(self) -> bool {
                self.0 == 0
            }

            /// The ratio of two values of this unit. `None` when the denominator is zero, because a
            /// benchmark that divides by nothing has measured nothing and should say so rather than
            /// return an infinity that formats as a plausible number.
            #[must_use]
            pub fn ratio_to(self, other: $name) -> Option<Ratio> {
                if other.0 == 0 {
                    return None;
                }
                Some(Ratio(self.0 as f64 / other.0 as f64))
            }

            /// Per-unit cost: this quantity spread over `n` things. `None` when `n` is zero.
            #[must_use]
            pub fn per(self, n: Count) -> Option<PerCount<$name>> {
                if n.0 == 0 {
                    return None;
                }
                Some(PerCount {
                    total: self,
                    each: self.0 as f64 / n.0 as f64,
                    over: n,
                })
            }
        }

        impl Add for $name {
            type Output = $name;
            fn add(self, other: $name) -> $name {
                $name(self.0.saturating_add(other.0))
            }
        }

        impl Sub for $name {
            type Output = $name;
            /// Saturating, because a negative duration or a negative footprint is a measurement error and
            /// wrapping it into a very large positive number is how such an error becomes a headline.
            fn sub(self, other: $name) -> $name {
                $name(self.0.saturating_sub(other.0))
            }
        }

        impl Mul<u64> for $name {
            type Output = $name;
            fn mul(self, factor: u64) -> $name {
                $name(self.0.saturating_mul(factor))
            }
        }

        impl Div<u64> for $name {
            type Output = $name;
            fn div(self, divisor: u64) -> $name {
                $name(self.0 / divisor.max(1))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}{}", self.0, $suffix)
            }
        }
    };
}

scalar_unit!(Nanos, "ns");
scalar_unit!(Bytes, "B");
scalar_unit!(Count, "");

impl Nanos {
    /// For a human-readable line. The value is still nanoseconds; this is presentation only, and it says
    /// so by returning a `String` rather than a number anything could go on to compute with.
    #[must_use]
    pub fn describe(self) -> String {
        match self.0 {
            n if n < 10_000 => format!("{n} ns"),
            n if n < 10_000_000 => format!("{:.1} µs", n as f64 / 1_000.0),
            n if n < 10_000_000_000 => format!("{:.1} ms", n as f64 / 1_000_000.0),
            n => format!("{:.2} s", n as f64 / 1_000_000_000.0),
        }
    }
}

impl Bytes {
    #[must_use]
    pub fn describe(self) -> String {
        match self.0 {
            b if b < 10_000 => format!("{b} B"),
            b if b < 10_000_000 => format!("{:.1} KiB", b as f64 / 1024.0),
            b if b < 10_000_000_000 => format!("{:.1} MiB", b as f64 / (1024.0 * 1024.0)),
            b => format!("{:.2} GiB", b as f64 / (1024.0 * 1024.0 * 1024.0)),
        }
    }
}

/// A quantity spread over a count — nanoseconds per row, bytes per query.
///
/// It keeps the total and the count it was divided by, so a report can print "12.4 µs per row over 10,000
/// rows" rather than "12.4", and a reader can tell the difference between a per-row cost measured over ten
/// rows and one measured over ten million.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PerCount<T> {
    pub total: T,
    pub each: f64,
    pub over: Count,
}

impl<T: fmt::Display> fmt::Display for PerCount<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:.2} each over {} ({})",
            self.each, self.over.0, self.total
        )
    }
}

/// A dimensionless ratio, buildable only from two quantities of the same unit.
///
/// **It is not a percentage and it will not print as one.** C9's growth assertions compared a fraction
/// against a threshold written as though it were a percentage; the value could not tell the two apart, and
/// neither could the print. Here `Ratio(1.05)` displays as `1.05×` and `as_percent_change()` is a separate,
/// explicitly named call that returns a [`PercentChange`].
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct Ratio(pub f64);

impl Ratio {
    #[must_use]
    pub fn get(self) -> f64 {
        self.0
    }

    /// This ratio expressed as a change from 1.0, in percent. `Ratio(1.05)` → `PercentChange(5.0)`.
    #[must_use]
    pub fn as_percent_change(self) -> PercentChange {
        PercentChange((self.0 - 1.0) * 100.0)
    }

    /// Is this ratio inside `[low, high]`? Named for what it answers, so a caller cannot read the
    /// comparison backwards.
    #[must_use]
    pub fn is_within(self, low: f64, high: f64) -> bool {
        self.0 >= low && self.0 <= high
    }
}

impl fmt::Display for Ratio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.3}×", self.0)
    }
}

/// A change expressed in percent, which prints with a sign and a `%` and cannot be confused with a ratio.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct PercentChange(pub f64);

impl fmt::Display for PercentChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:+.1}%", self.0)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::float_cmp)]

    use super::*;

    #[test]
    fn a_ratio_and_a_percentage_are_different_things_and_print_differently() {
        let ratio = Nanos(105).ratio_to(Nanos(100)).unwrap();
        assert_eq!(ratio.to_string(), "1.050×");
        assert_eq!(ratio.as_percent_change().to_string(), "+5.0%");
        // The C9 bug, in one line: a fraction compared against a threshold meant as a percentage. The
        // types now make the two spellings visibly different at the call site.
        assert!(ratio.is_within(1.0, 1.1));
        assert!(!ratio.is_within(1.0, 1.01));
    }

    #[test]
    fn dividing_by_nothing_returns_nothing_rather_than_an_infinity() {
        assert!(Nanos(10).ratio_to(Nanos(0)).is_none());
        assert!(Bytes(10).per(Count(0)).is_none());
    }

    #[test]
    fn a_negative_measurement_saturates_rather_than_wrapping() {
        // A clock that appears to go backwards must not become 18 quintillion nanoseconds.
        assert_eq!(Nanos(5) - Nanos(9), Nanos(0));
        assert_eq!(Bytes(0) - Bytes(1), Bytes(0));
    }

    #[test]
    fn a_per_count_remembers_what_it_was_divided_by() {
        let per = Nanos(10_000).per(Count(100)).unwrap();
        assert_eq!(per.each, 100.0);
        assert_eq!(per.over, Count(100));
        assert!(per.to_string().contains("over 100"));
    }
}
