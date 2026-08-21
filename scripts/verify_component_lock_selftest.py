#!/usr/bin/env python3
"""The lock verifier's counter-check (MD-7): prove the refusals actually fire.

A verifier that cannot fail is not a verifier. Every doctored lock below encodes one specific
lie — a production approval without a receipt, over standing blockers, without release
admission, an admitted component with no tag — and the check asserts the verifier refuses it
WITH THE NAMED MESSAGE, then asserts the real lock still passes. Runs in the same CI job as the
verifier itself.
"""

from __future__ import annotations

import copy
import json
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
VERIFIER = ROOT / "scripts" / "verify_component_lock.py"
LOCK = ROOT / "components.lock.json"


def run_verifier(lock_path: Path) -> tuple[int, str]:
    result = subprocess.run(
        [sys.executable, str(VERIFIER), str(lock_path)],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    return result.returncode, result.stdout + result.stderr


def doctored(mutate) -> Path:
    document = json.loads(LOCK.read_text(encoding="utf-8"))
    mutate(document)
    handle = tempfile.NamedTemporaryFile(
        mode="w", suffix=".json", delete=False, encoding="utf-8"
    )
    json.dump(document, handle)
    handle.close()
    return Path(handle.name)


def component(document: dict, name: str) -> dict:
    return next(item for item in document["components"] if item["name"] == name)


def expect_refusal(title: str, mutate, needle: str) -> None:
    code, output = run_verifier(doctored(mutate))
    if code == 0:
        print(f"counter-check FAILED: {title}: the doctored lock PASSED", file=sys.stderr)
        raise SystemExit(1)
    if needle not in output:
        print(
            f"counter-check FAILED: {title}: refused, but not by name — wanted {needle!r}, "
            f"got: {output.strip()}",
            file=sys.stderr,
        )
        raise SystemExit(1)
    print(f"counter-check: {title}: refused by name")


def approve_without_receipt(document: dict) -> None:
    prism = component(document, "prismdb")
    prism["releaseTag"] = "prismdb-v9.9.9-doctored"
    prism["admitted"] = True
    prism["blockers"] = []
    prism["productionBlockers"] = []
    prism["productionApproved"] = True
    prism["custodyReceipt"] = None


def approve_over_blockers(document: dict) -> None:
    prism = component(document, "prismdb")
    prism["releaseTag"] = "prismdb-v9.9.9-doctored"
    prism["admitted"] = True
    prism["blockers"] = []
    prism["productionApproved"] = True
    prism["custodyReceipt"] = "doctored-receipt"


def approve_without_admission(document: dict) -> None:
    prism = component(document, "prismdb")
    prism["productionBlockers"] = []
    prism["productionApproved"] = True
    prism["custodyReceipt"] = "doctored-receipt"


def admit_without_release(document: dict) -> None:
    prism = component(document, "prismdb")
    prism["admitted"] = True
    prism["blockers"] = []


def main() -> None:
    expect_refusal(
        "production approval without a custody receipt",
        approve_without_receipt,
        "without a custody receipt is refused (MD-7)",
    )
    expect_refusal(
        "production approval over standing production blockers",
        approve_over_blockers,
        "refused while production blockers stand",
    )
    expect_refusal(
        "production approval without release admission",
        approve_without_admission,
        "production approval without release admission is refused (MD-7)",
    )
    expect_refusal(
        "admission while PrismDB's release tag does not exist",
        admit_without_release,
        "an unreleased component cannot be admitted",
    )

    code, output = run_verifier(LOCK)
    if code != 0:
        print(
            f"counter-check FAILED: the REAL lock was refused: {output.strip()}",
            file=sys.stderr,
        )
        raise SystemExit(1)
    print("counter-check: the real lock still passes; every refusal direction proven")


if __name__ == "__main__":
    main()
