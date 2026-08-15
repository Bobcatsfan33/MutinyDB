#!/usr/bin/env python3
"""Run all four C10 benchmarks and write one reproducible evidence artifact."""

from __future__ import annotations

import argparse
import json
import platform
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

try:
    import duckdb
except ImportError as error:
    raise SystemExit(
        "install testing/bench/requirements-c10.txt in an isolated environment first"
    ) from error


ROUNDS = 11
QUERY = """
SELECT l_shipmode AS k, SUM(l_linenumber)::BIGINT AS s
FROM lineitem
GROUP BY l_shipmode
ORDER BY l_shipmode
"""


def timed(action):
    started = time.perf_counter_ns()
    value = action()
    return time.perf_counter_ns() - started, value


def summary(samples: list[int]) -> dict[str, object]:
    return {
        "rounds": len(samples),
        "median_nanos": int(statistics.median(samples)),
        "fastest_nanos": min(samples),
        "slowest_nanos": max(samples),
        "all_nanos": samples,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--output",
        type=Path,
        default=Path("testing/evidence/c10-benchmarks.json"),
    )
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    output = args.output if args.output.is_absolute() else root / args.output

    subprocess.run(
        [
            "cargo",
            "build",
            "--release",
            "-p",
            "schweep-bench",
            "--bin",
            "c10_native",
            "--bin",
            "c10_oneshot_worker",
        ],
        cwd=root,
        check=True,
    )
    native = subprocess.run(
        [root / "target/release/c10_native"],
        cwd=root,
        check=True,
        text=True,
        capture_output=True,
    )

    with tempfile.TemporaryDirectory(prefix="schweep-c10-") as scratch:
        projection = Path(scratch) / "lineitem.psv"
        connection = duckdb.connect()
        connection.execute("INSTALL tpch")
        connection.execute("LOAD tpch")
        connection.execute("CALL dbgen(sf=0.1)")
        escaped = str(projection).replace("'", "''")
        connection.execute(
            f"COPY (SELECT l_shipmode, l_linenumber FROM lineitem ORDER BY rowid) "
            f"TO '{escaped}' (DELIMITER '|', HEADER false)"
        )
        row_count = connection.execute("SELECT count(*) FROM lineitem").fetchone()[0]

        worker = subprocess.Popen(
            [root / "target/release/c10_oneshot_worker", projection],
            cwd=root,
            text=True,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
        )
        if worker.stdin is None or worker.stdout is None:
            raise RuntimeError("worker pipes were not created")
        if worker.stdout.readline().strip() != "READY":
            raise RuntimeError("Schweep worker did not become ready")

        def schweep_round():
            worker.stdin.write("RUN\n")
            worker.stdin.flush()
            return int(worker.stdout.readline())

        def duckdb_round():
            return len(connection.execute(QUERY).fetchall())

        # Warm both engines outside the measurement.
        if schweep_round() != 7 or duckdb_round() != 7:
            raise RuntimeError("paired engines did not return seven ship modes")

        schweep_times: list[int] = []
        duckdb_times: list[int] = []
        for round_index in range(ROUNDS):
            order = (
                ((schweep_round, schweep_times), (duckdb_round, duckdb_times))
                if round_index % 2 == 0
                else ((duckdb_round, duckdb_times), (schweep_round, schweep_times))
            )
            for action, samples in order:
                elapsed, result_rows = timed(action)
                if result_rows != 7:
                    raise RuntimeError("a measured answer had the wrong cardinality")
                samples.append(elapsed)

        worker.stdin.write("STOP\n")
        worker.stdin.flush()
        worker.wait(timeout=30)
        if worker.returncode != 0:
            raise RuntimeError(f"Schweep worker exited {worker.returncode}")

    artifact = {
        "schema_version": 1,
        "suite": "schweep-c10",
        "generated_at_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "machine": {
            "platform": platform.platform(),
            "python": platform.python_version(),
            "duckdb": duckdb.__version__,
            "profile": "release",
        },
        "method": {
            "rounds": ROUNDS,
            "comparison": "paired alternating rounds after one untimed warm-up",
            "reporting": "median, full sample, fastest, and slowest; headlines use slowest",
        },
        "native": json.loads(native.stdout),
        "tpch_sf0_1": {
            "scale_factor": 0.1,
            "source": "DuckDB tpch extension dbgen",
            "lineitem_rows": row_count,
            "query_scope": (
                "supported-dialect projection over TPC-H lineitem: group by l_shipmode and "
                "sum integer l_linenumber; this is not the official TPC-H query suite"
            ),
            "schweep_oneshot": summary(schweep_times),
            "duckdb": summary(duckdb_times),
            "slowest_ratio_schweep_over_duckdb": max(schweep_times)
            / max(duckdb_times),
        },
    }
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(artifact, indent=2) + "\n", encoding="utf-8")
    print(output)
    return 0


if __name__ == "__main__":
    sys.exit(main())
