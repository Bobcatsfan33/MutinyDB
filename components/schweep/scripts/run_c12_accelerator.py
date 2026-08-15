#!/usr/bin/env python3
"""Run the pre-registered C12 paired CPU/Metal experiment and write its full receipt."""

from __future__ import annotations

import argparse
import json
import os
import platform
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

SIZES = (100_000, 1_000_000, 10_000_000)
ROUNDS = 11


def checked(*command: str, cwd: Path) -> str:
    return subprocess.check_output(command, cwd=cwd, text=True, stderr=subprocess.STDOUT).strip()


def start(command: list[str], cwd: Path) -> subprocess.Popen[str]:
    return subprocess.Popen(
        command,
        cwd=cwd,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
    )


def line(process: subprocess.Popen[str], label: str) -> str:
    assert process.stdout is not None
    value = process.stdout.readline().strip()
    if not value:
        stderr = process.stderr.read() if process.stderr is not None else ""
        raise RuntimeError(f"{label} stopped before replying: {stderr}")
    return value


def run_once(process: subprocess.Popen[str], rows: int, label: str) -> tuple[int, int]:
    assert process.stdin is not None
    start_ns = time.perf_counter_ns()
    process.stdin.write(f"RUN {rows}\n")
    process.stdin.flush()
    response = line(process, label)
    elapsed = time.perf_counter_ns() - start_ns
    marker, separator, raw = response.partition("\t")
    if marker != "RESULT" or not separator:
        raise RuntimeError(f"{label} returned an invalid response: {response}")
    return elapsed, int(raw)


def stop(process: subprocess.Popen[str]) -> None:
    if process.poll() is None and process.stdin is not None:
        try:
            process.stdin.write("STOP\n")
            process.stdin.flush()
            process.wait(timeout=10)
        except (BrokenPipeError, subprocess.TimeoutExpired):
            process.kill()
            process.wait()


def summary(samples: list[int]) -> dict[str, object]:
    return {
        "raw_nanos": samples,
        "median_nanos": int(statistics.median(samples)),
        "fastest_nanos": min(samples),
        "slowest_nanos": max(samples),
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--output",
        type=Path,
        default=Path("testing/evidence/c12-accelerator.json"),
    )
    arguments = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    if platform.system() != "Darwin" or platform.machine() != "arm64":
        raise RuntimeError("the frozen C12 spike requires a real Apple-arm64 Metal host")

    checked("cargo", "build", "--release", "--locked", "-p", "schweep-bench", "--bin", "c12_cpu_worker", cwd=root)
    cpu_binary = root / "target/release/c12_cpu_worker"

    with tempfile.TemporaryDirectory(prefix="schweep-c12-") as raw_temp:
        temp = Path(raw_temp)
        input_path = temp / "pairs.i64x2"
        metal_binary = temp / "c12_metal_worker"
        clang_command = [
            "xcrun", "--sdk", "macosx", "clang++", "-x", "objective-c++", "-O3", "-fobjc-arc",
            "-framework", "Foundation", "-framework", "Metal",
            str(root / "testing/bench/src/c12_metal_worker.m"), "-o", str(metal_binary),
        ]
        subprocess.check_call(clang_command, cwd=root)
        subprocess.check_call([str(cpu_binary), "--generate", str(input_path), str(max(SIZES))], cwd=root)

        cpu = start([str(cpu_binary), str(input_path)], root)
        gpu = start([str(metal_binary), str(input_path)], root)
        try:
            if line(cpu, "CPU") != "READY":
                raise RuntimeError("CPU worker did not become ready")
            gpu_ready = line(gpu, "GPU").split("\t", 2)
            if len(gpu_ready) != 3 or gpu_ready[0] != "READY":
                raise RuntimeError(f"GPU worker did not become ready: {gpu_ready}")
            gpu_setup_nanos = int(gpu_ready[1])
            metal_device = gpu_ready[2]

            measurements = []
            exact_warmup_pairs = 0
            exact_executions = 0
            for rows in SIZES:
                _, cpu_warm = run_once(cpu, rows, "CPU")
                _, gpu_warm = run_once(gpu, rows, "GPU")
                if cpu_warm != gpu_warm:
                    raise RuntimeError(f"warm-up mismatch at {rows}: CPU={cpu_warm}, GPU={gpu_warm}")
                exact_warmup_pairs += 1

                cpu_samples: list[int] = []
                gpu_samples: list[int] = []
                for round_index in range(ROUNDS):
                    order = ((cpu, "CPU", cpu_samples), (gpu, "GPU", gpu_samples))
                    if round_index % 2 == 1:
                        order = tuple(reversed(order))
                    results = {}
                    for process, label, samples in order:
                        elapsed, result = run_once(process, rows, label)
                        samples.append(elapsed)
                        results[label] = result
                        exact_executions += 1
                    if results["CPU"] != results["GPU"] or results["CPU"] != cpu_warm:
                        raise RuntimeError(f"measured result mismatch at {rows}, round {round_index}")

                cpu_report = summary(cpu_samples)
                gpu_report = summary(gpu_samples)
                speedup = cpu_report["median_nanos"] / gpu_report["median_nanos"]
                measurements.append({
                    "rows": rows,
                    "result": cpu_warm,
                    "cpu": cpu_report,
                    "gpu": gpu_report,
                    "median_gpu_speedup": round(speedup, 6),
                })

            break_even = next(
                (item["rows"] for item in measurements if item["gpu"]["median_nanos"] <= item["cpu"]["median_nanos"]),
                None,
            )
            by_size = {item["rows"]: item for item in measurements}
            criteria = {
                "exact_all_warmups_and_measured_executions": (
                    exact_warmup_pairs == len(SIZES)
                    and exact_executions == len(SIZES) * ROUNDS * 2
                ),
                "gpu_speedup_at_least_2x_at_1m": by_size[1_000_000]["median_gpu_speedup"] >= 2.0,
                "gpu_speedup_at_least_2x_at_10m": by_size[10_000_000]["median_gpu_speedup"] >= 2.0,
                "break_even_no_later_than_1m": break_even is not None and break_even <= 1_000_000,
                "all_11_paired_rounds_present": all(
                    len(item["cpu"]["raw_nanos"]) == ROUNDS and len(item["gpu"]["raw_nanos"]) == ROUNDS
                    for item in measurements
                ),
                "committed_real_gpu_toolchain": True,
            }
            verdict = "GO" if all(criteria.values()) else "NO-GO"
            artifact = {
                "schema_version": 1,
                "suite": "schweep-c12-accelerator-spike",
                "verdict": verdict,
                "criteria": criteria,
                "exact_warmup_pairs": exact_warmup_pairs,
                "exact_measured_executions": exact_executions,
                "break_even_rows": break_even,
                "rounds": ROUNDS,
                "paired_order": "CPU then GPU on even rounds; GPU then CPU on odd rounds, after one untimed warm-up",
                "query": "SELECT SUM(t.n) AS total FROM t WHERE t.k > 0",
                "input": "same checked-in deterministic generator and same little-endian Int64-pair file for both workers",
                "gpu_sample_includes": "input buffer copy, output allocation, command encoding, dispatch, synchronization, and host partial reduction",
                "gpu_sample_excludes": "one-time device discovery, runtime shader compilation, pipeline creation, and command-queue creation",
                "gpu_setup_nanos": gpu_setup_nanos,
                "machine": {
                    "platform": platform.platform(),
                    "architecture": platform.machine(),
                    "processor": platform.processor(),
                    "logical_cpus": os.cpu_count(),
                    "metal_device": metal_device,
                    "rustc": checked("rustc", "--version", cwd=root),
                    "clang": checked("xcrun", "--sdk", "macosx", "clang++", "--version", cwd=root).splitlines()[0],
                    "python": sys.version.split()[0],
                },
                "source_commit": checked("git", "rev-parse", "HEAD", cwd=root),
                "measurements": measurements,
                "scope": "C12 evidence only; no GPU production code, dependency, feature, or API ships",
            }
            output = arguments.output if arguments.output.is_absolute() else root / arguments.output
            output.parent.mkdir(parents=True, exist_ok=True)
            output.write_text(json.dumps(artifact, indent=2) + "\n")
            print(json.dumps({"output": str(output), "verdict": verdict, "criteria": criteria}, indent=2))
        finally:
            stop(cpu)
            stop(gpu)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
