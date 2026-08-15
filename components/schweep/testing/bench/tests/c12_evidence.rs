//! C12's pre-registered verdict and raw-sample integrity gate (D-28, I-10).

#![allow(clippy::indexing_slicing, clippy::panic)]

use std::path::Path;

const ARTIFACT: &str = include_str!("../../evidence/c12-accelerator.json");
const DECISIONS: &str = include_str!("../../../docs/DECISIONS.md");
const PROTOCOL: &str = include_str!("../../../docs/C12_ACCELERATOR_PROTOCOL.md");
const README: &str = include_str!("../../../README.md");

fn unsigned_after(text: &str, marker: &str) -> u64 {
    let tail = text
        .split_once(marker)
        .unwrap_or_else(|| panic!("missing numeric marker {marker:?}"))
        .1;
    let digits: String = tail
        .chars()
        .skip_while(|character| character.is_whitespace())
        .take_while(char::is_ascii_digit)
        .collect();
    digits
        .parse()
        .unwrap_or_else(|_| panic!("invalid unsigned value after {marker:?}"))
}

fn float_after(text: &str, marker: &str) -> f64 {
    let tail = text
        .split_once(marker)
        .unwrap_or_else(|| panic!("missing float marker {marker:?}"))
        .1;
    let digits: String = tail
        .chars()
        .skip_while(|character| character.is_whitespace())
        .take_while(|character| character.is_ascii_digit() || *character == '.')
        .collect();
    digits
        .parse()
        .unwrap_or_else(|_| panic!("invalid float value after {marker:?}"))
}

fn sample_arrays(section: &str) -> Vec<Vec<u64>> {
    section
        .split("\"raw_nanos\": [")
        .skip(1)
        .take(2)
        .map(|tail| {
            tail.split_once(']')
                .unwrap_or_else(|| panic!("unterminated raw_nanos array"))
                .0
                .split(',')
                .map(|number| {
                    number
                        .trim()
                        .parse()
                        .unwrap_or_else(|_| panic!("invalid raw_nanos sample {number:?}"))
                })
                .collect()
        })
        .collect()
}

fn median(mut samples: Vec<u64>) -> u64 {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

#[test]
fn raw_samples_recompute_the_pre_registered_go_verdict() {
    for required in [
        "\"verdict\": \"GO\"",
        "\"exact_all_warmups_and_measured_executions\": true",
        "\"exact_warmup_pairs\": 3",
        "\"exact_measured_executions\": 66",
        "\"all_11_paired_rounds_present\": true",
        "\"metal_device\": \"Apple M2\"",
        "input buffer copy, output allocation, command encoding, dispatch, synchronization",
        "no GPU production code, dependency, feature, or API ships",
    ] {
        assert!(ARTIFACT.contains(required), "C12 receipt lost {required:?}");
    }

    let mut measured_sizes = Vec::new();
    for section in ARTIFACT.split("\"rows\": ").skip(1) {
        let rows = section
            .split_once(',')
            .and_then(|(number, _)| number.trim().parse::<u64>().ok())
            .unwrap_or_else(|| panic!("measurement has no row count"));
        let arrays = sample_arrays(section);
        assert_eq!(arrays.len(), 2, "size {rows} needs CPU and GPU samples");
        assert!(arrays.iter().all(|samples| samples.len() == 11));
        let cpu_median = median(arrays[0].clone());
        let gpu_median = median(arrays[1].clone());
        let recorded_medians: Vec<u64> = section
            .split("\"median_nanos\": ")
            .skip(1)
            .take(2)
            .map(|tail| {
                tail.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse()
                    .unwrap_or_else(|_| panic!("invalid recorded median"))
            })
            .collect();
        assert_eq!(recorded_medians, vec![cpu_median, gpu_median]);
        let recomputed_speedup = cpu_median as f64 / gpu_median as f64;
        let recorded_speedup = float_after(section, "\"median_gpu_speedup\": ");
        assert!((recomputed_speedup - recorded_speedup).abs() < 0.000_001);
        if rows >= 1_000_000 {
            assert!(recomputed_speedup >= 2.0, "size {rows} missed D-28");
        }
        measured_sizes.push(rows);
    }
    assert_eq!(measured_sizes, vec![100_000, 1_000_000, 10_000_000]);
    assert!(unsigned_after(ARTIFACT, "\"break_even_rows\": ") <= 1_000_000);
}

#[test]
fn public_verdict_keeps_the_production_boundary_honest() {
    for document in [DECISIONS, PROTOCOL, README] {
        assert!(document.contains("89.85x"));
        assert!(document.contains("85.98x"));
    }
    assert!(DECISIONS.contains("CPU remains the only production path"));
    assert!(README.contains("CPU remains the only shipped path"));
    assert!(DECISIONS.contains("`GO` for a later design phase"));
    assert!(PROTOCOL.contains("no production GPU code ships"));
    assert!(README.contains("c12-accelerator.json"));
}

fn inspect_production_tree(path: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let candidate = entry.path();
        if candidate.is_dir() {
            inspect_production_tree(&candidate)?;
            continue;
        }
        let relevant = candidate
            .extension()
            .is_some_and(|extension| extension == "rs" || extension == "toml");
        if relevant {
            let source = std::fs::read_to_string(&candidate)?;
            assert!(
                !source.contains("Metal") && !source.contains("c12_accelerator"),
                "C12 spike leaked into production source {}",
                candidate.display()
            );
        }
    }
    Ok(())
}

#[test]
fn accelerator_spike_is_absent_from_every_production_crate() -> std::io::Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    inspect_production_tree(&root.join("crates"))?;
    let workspace = std::fs::read_to_string(root.join("Cargo.toml"))?;
    assert!(!workspace.contains("Metal"));
    Ok(())
}
