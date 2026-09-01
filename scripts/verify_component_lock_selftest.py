#!/usr/bin/env python3
"""The lock verifier's counter-check (MD-7): prove the refusals actually fire.

A verifier that cannot fail is not a verifier. Every doctored lock below encodes one specific
lie — a production approval without a receipt, over standing blockers, without release
admission, an admitted component with no tag, a PrismDB release admission with one of its three
artifact tags absent — and the check asserts the verifier refuses it WITH THE NAMED MESSAGE,
then asserts the real lock still passes. Runs in the same CI job as the verifier itself.
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
    # PrismDB is genuinely release-admitted in the real lock; the lie is the approval claim.
    prism = component(document, "prismdb")
    prism["productionBlockers"] = []
    prism["productionApproved"] = True
    prism["custodyReceipt"] = None


def approve_over_blockers(document: dict) -> None:
    prism = component(document, "prismdb")
    prism["productionApproved"] = True
    prism["custodyReceipt"] = "doctored-receipt"


def approve_without_admission(document: dict) -> None:
    prism = component(document, "prismdb")
    prism["admitted"] = False
    prism["blockers"] = ["doctored: release admission withdrawn"]
    prism["productionBlockers"] = []
    prism["productionApproved"] = True
    prism["custodyReceipt"] = "doctored-receipt"


def admit_without_release(document: dict) -> None:
    # The single-tag direction, proven on schweep: strip its release tag, keep it admitted.
    schweep = component(document, "schweep")
    schweep["releaseTag"] = None


def drop_release_tag(tag: str):
    def mutate(document: dict) -> None:
        prism = component(document, "prismdb")
        prism["releaseTags"] = [item for item in prism["releaseTags"] if item != tag]

    return mutate


def no_release_tags_at_all(document: dict) -> None:
    prism = component(document, "prismdb")
    prism["releaseTags"] = None


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
        "admission of a single-tag component whose release tag does not exist",
        admit_without_release,
        "an unreleased component cannot be admitted",
    )
    # MD-7 amendment: PrismDB's release is three artifact tags at one commit. Release
    # admission with ANY of the three absent is refused by the missing artifact's name.
    for artifact, tag in (
        ("prismd", "prismd-v0.1.0"),
        ("prism-shard", "prism-shard-v0.1.0"),
        ("model-service", "model-service-v0.1.0"),
    ):
        expect_refusal(
            f"PrismDB release admission with the {artifact} tag absent",
            drop_release_tag(tag),
            f"the {artifact} tag is absent",
        )
    expect_refusal(
        "PrismDB release admission with no release tags at all",
        no_release_tags_at_all,
        "the prismd tag is absent",
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
