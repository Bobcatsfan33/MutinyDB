//! Rendering an `f64` must be lossless, because **answer comparison happens on rendered strings**.
//!
//! This lands before `AVG` exists, which is the only thing in Current that produces a `Float64`
//! (S-3, S-31). The reason it has to land first is a chain of three facts:
//!
//! 1. I-1 requires the incremental answer to equal the oracle's recomputation **byte for byte**.
//! 2. The differential harness compares [`Canonical::render`] output — strings, not values.
//! 3. `AVG` is exempt from the no-floats rule (D-10) *only* because both implementations perform
//!    one identical IEEE-754 division of two exact integers and therefore produce identical bits.
//!
//! Fact 3 is worth nothing if fact 2 throws bits away. If two distinct `f64` values could render
//! identically, the harness would report agreement between two answers that differ — and it would
//! do so silently, on exactly the values where floating-point arithmetic is most likely to have gone
//! wrong. The whole argument for allowing `AVG` at all rests on this file.
//!
//! What is asserted:
//!
//! - **Injectivity:** distinct bit patterns never render to the same string.
//! - **Round-trip:** the rendered form parses back to the identical bit pattern.
//!
//! Both are asserted over the values that break naive formatting — subnormals, values needing all
//! 17 significant digits, adjacent representable neighbours, ±0.0 — and then over a large seeded
//! sweep of arbitrary bit patterns.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;

use schweep_zset::{DataType, Field, Row, Schema, Value, ZSetBatch};

/// Render a single float exactly as an answer would carry it: through a Z-set's canonical form.
///
/// Deliberately not `format!("{x:?}")` directly. The property that matters is about the path a real
/// answer takes, and testing a shortcut would leave the real path unproven.
fn render_through_an_answer(x: f64) -> String {
    let schema = Schema::new(vec![Field::nullable("avg", DataType::Float64)]).unwrap();
    let batch =
        ZSetBatch::from_entries(schema, vec![(Row::new(vec![Value::Float(x)]), 1)]).unwrap();
    batch.canonical().unwrap().render()
}

/// The float's own rendering, extracted from the row line of the canonical form.
fn rendered_value(x: f64) -> String {
    let full = render_through_an_answer(x);
    let line = full
        .lines()
        .nth(1)
        .expect("a canonical form with one entry has a schema line and a row line");
    line.trim_start_matches('(')
        .split(')')
        .next()
        .expect("the row line starts with (value)")
        .to_owned()
}

/// The values that break naive float formatting.
fn awkward_values() -> Vec<f64> {
    let mut values = vec![
        0.0,
        -0.0,
        1.0,
        -1.0,
        0.1,
        0.2,
        0.3,
        1.0 / 3.0,
        2.0 / 3.0,
        // Needs all 17 significant digits to round-trip. Built from bits rather than written as a
        // literal, because the literal is the *same* f64 as 0.1 and clippy rightly says so — the
        // value that actually differs is 0.1's neighbour.
        f64::from_bits(0.1_f64.to_bits() + 1),
        1.234_567_890_123_456_7e300,
        f64::MIN,
        f64::MAX,
        f64::MIN_POSITIVE,
        // Subnormals: the smallest positive value, and its neighbour.
        f64::from_bits(1),
        f64::from_bits(2),
        f64::INFINITY,
        f64::NEG_INFINITY,
        // Values AVG actually produces: sums over counts.
        1.0 / 7.0,
        -22.0 / 7.0,
        i64::MAX as f64 / 3.0,
        i64::MIN as f64 / 7.0,
    ];
    // Adjacent representable neighbours around a handful of anchors — the pairs a lossy renderer
    // would collapse.
    for anchor in [1.0_f64, 0.1, 1e16, 1e-16, 12345.678] {
        let bits = anchor.to_bits();
        for delta in [0_u64, 1, 2, 3] {
            values.push(f64::from_bits(bits + delta));
            values.push(f64::from_bits(bits.wrapping_sub(delta)));
        }
    }
    values
}

/// **Injectivity over the awkward values:** distinct bit patterns render distinctly.
#[test]
fn distinct_bit_patterns_never_render_identically() {
    let mut seen: BTreeMap<String, u64> = BTreeMap::new();
    for x in awkward_values() {
        let bits = x.to_bits();
        let rendered = rendered_value(x);
        if let Some(previous) = seen.insert(rendered.clone(), bits) {
            assert_eq!(
                previous, bits,
                "two distinct f64 bit patterns ({previous:#x} and {bits:#x}) both render as \
                 {rendered:?}; answer comparison happens on this string, so the harness would \
                 report agreement between two different answers (I-1, S-31)"
            );
        }
    }
}

/// **Round-trip:** the rendered form parses back to the identical bit pattern.
#[test]
fn rendering_round_trips_to_the_same_bits() {
    for x in awkward_values() {
        let rendered = rendered_value(x);
        let parsed: f64 = rendered
            .parse()
            .unwrap_or_else(|e| panic!("{rendered:?} does not parse back as f64: {e}"));
        assert_eq!(
            parsed.to_bits(),
            x.to_bits(),
            "{rendered:?} parsed back to a different bit pattern: {:#x} vs {:#x}",
            parsed.to_bits(),
            x.to_bits()
        );
    }
}

/// `-0.0` and `0.0` are different values (S-7 orders them apart), so they must render apart too.
///
/// This is the smallest possible instance of the whole problem, and the one a `{}`-style formatter
/// gets wrong: `format!("{}", -0.0_f64)` is `"-0"` in current Rust but was `"0"` historically, and a
/// renderer that produced `"0"` for both would make `Value::Float(-0.0) != Value::Float(0.0)` while
/// their answers compared equal.
#[test]
fn negative_zero_renders_differently_from_positive_zero() {
    assert_ne!(rendered_value(-0.0), rendered_value(0.0));
    assert_ne!(Value::Float(-0.0), Value::Float(0.0));
    assert_eq!(
        rendered_value(-0.0).parse::<f64>().unwrap().to_bits(),
        (-0.0_f64).to_bits()
    );
}

/// A large seeded sweep of arbitrary bit patterns.
///
/// The awkward list covers the cases someone thought of. This covers the ones nobody did: 200,000
/// bit patterns from a fixed splitmix64 stream, checked for both injectivity and round-trip. Seeded
/// and hard-coded, so it is reproducible and adds no dependency (D-6, I-2).
#[test]
fn a_large_sweep_of_bit_patterns_renders_losslessly() {
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };

    let mut seen: BTreeMap<String, u64> = BTreeMap::new();
    let mut checked = 0_u32;
    for _ in 0..200_000 {
        let bits = next();
        let x = f64::from_bits(bits);
        // NaN cannot arise in an answer (S-31: AVG never divides by zero), and every NaN bit
        // pattern renders as "NaN", so it is the one value where rendering is legitimately not
        // injective. Skipped, and skipped for a stated reason rather than quietly.
        if x.is_nan() {
            continue;
        }
        let rendered = rendered_value(x);

        let parsed: f64 = rendered
            .parse()
            .unwrap_or_else(|e| panic!("{rendered:?} (bits {bits:#x}) does not parse: {e}"));
        assert_eq!(
            parsed.to_bits(),
            bits,
            "{rendered:?} did not round-trip: {:#x} vs {bits:#x}",
            parsed.to_bits()
        );

        if let Some(previous) = seen.insert(rendered.clone(), bits) {
            assert_eq!(
                previous, bits,
                "bit patterns {previous:#x} and {bits:#x} both render as {rendered:?}"
            );
        }
        checked += 1;
    }
    assert!(
        checked > 190_000,
        "expected to check nearly every pattern, checked {checked} (NaNs are skipped)"
    );
}

/// The property `AVG`'s exemption actually rests on: one division of two exact integers gives the
/// same bits every time, and rendering preserves them (S-31, D-10).
#[test]
fn avgs_arithmetic_is_bit_stable_through_rendering() {
    // A spread of (sum, count) pairs of the kind AVG produces, including sums beyond 2^53 where the
    // i64-to-f64 conversion rounds, and counts that do not divide evenly.
    let pairs: Vec<(i64, i64)> = vec![
        (1, 3),
        (2, 3),
        (10, 4),
        (-22, 7),
        (i64::MAX, 3),
        (i64::MIN, 7),
        (i64::MAX, 1),
        (1, i64::MAX),
        (9_007_199_254_740_993, 3),
    ];
    for (sum, count) in pairs {
        let once = sum as f64 / count as f64;
        let twice = sum as f64 / count as f64;
        assert_eq!(
            once.to_bits(),
            twice.to_bits(),
            "the same division must give the same bits"
        );
        let rendered = rendered_value(once);
        assert_eq!(
            rendered.parse::<f64>().unwrap().to_bits(),
            once.to_bits(),
            "AVG of {sum}/{count} rendered as {rendered:?} and lost bits"
        );
    }
}
