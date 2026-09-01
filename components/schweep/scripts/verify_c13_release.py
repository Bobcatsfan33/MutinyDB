#!/usr/bin/env python3
"""Fail closed unless the C13 release evidence and tag are complete.

Two modes:

* default — the release gate. Reads the workspace version, the tag, and the nightly-streak
  evidence, and exits non-zero with a `release blocked: …` line on stderr at the first check that
  does not hold. `.github/workflows/release.yml` runs this with no arguments.
* `--consistency` — the evidence-file audit. Checks that the live evidence file agrees with
  itself (counts match the runs, dates parse and ascend, `qualifies` is derived from the job
  conclusions, `release_blocked` mirrors `status`) WITHOUT judging which lifecycle state the file is
  in. `c13_release_contract.rs` runs this against the committed file so the file can never be pinned
  to `pending` or `complete` by a test again.

`--evidence PATH` points either mode at a different file. The test uses it to feed synthetic
fixtures through the exact code path the release workflow runs.
"""

import argparse
import json
from datetime import date
import os
from pathlib import Path
import re
import sys


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_EVIDENCE = ROOT / "testing/evidence/c13-nightly-streak.json"
REQUIRED_NIGHTS = 7
LIFECYCLE_STATES = ("pending", "complete")


def fail(message: str) -> None:
    print(f"release blocked: {message}", file=sys.stderr)
    raise SystemExit(1)


def workspace_version() -> str:
    workspace = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    match = re.search(r"\[workspace\.package\].*?^version = \"([^\"]+)\"", workspace, re.M | re.S)
    if match is None:
        fail("Cargo.toml has no workspace package version")
    return match.group(1)


def check_tag(tag: str, version: str) -> None:
    major, minor, patch = version.split(".")
    expected_tag = f"current-v{major}.{minor}"
    if tag != expected_tag:
        fail(f"tag must be {expected_tag}, got {tag or '<none>'}")
    if patch != "0":
        fail("the frozen current-v0.1 tag must point at the initial 0.1.0 package version")


def parse_dates(dates: list) -> list:
    try:
        return [date.fromisoformat(value) for value in dates]
    except (TypeError, ValueError) as error:
        fail(f"qualifying night date is invalid: {error}")
    return []


def check_evidence(evidence: dict) -> int:
    """The release gate proper. Returns the number of qualifying nights credited."""
    runs = evidence.get("runs", [])
    qualifying = [run for run in runs if run.get("qualifies") is True]
    required = evidence.get("required_consecutive_nights")
    if evidence.get("status") != "complete" or evidence.get("release_blocked") is not False:
        fail("nightly evidence is not marked complete")
    if required != REQUIRED_NIGHTS or len(qualifying) < required:
        fail(f"need {REQUIRED_NIGHTS} qualifying nightly runs, found {len(qualifying)}")
    window = qualifying[-required:]
    dates = [run.get("date") for run in window]
    if len(dates) != len(set(dates)):
        fail("qualifying night dates are not unique")
    parsed_dates = parse_dates(dates)
    for previous, current in zip(parsed_dates, parsed_dates[1:]):
        if (current - previous).days != 1:
            fail(f"qualifying dates are not consecutive: {previous} then {current}")
    for run in window:
        if run.get("workflow_conclusion") != "success":
            fail(f"workflow {run.get('run_id')} is not successful")
        if run.get("nightly_crash") != "success" or run.get("nightly_soak") != "success":
            fail(f"workflow {run.get('run_id')} lacks a green required nightly job")
    return len(window)


def check_consistency(evidence: dict) -> None:
    """The file agrees with itself. Deliberately silent on WHICH lifecycle state it is in."""
    status = evidence.get("status")
    if status not in LIFECYCLE_STATES:
        fail(f"status must be one of {LIFECYCLE_STATES}, got {status!r}")
    if evidence.get("release_blocked") is not (status != "complete"):
        fail(f"release_blocked must mirror status {status!r}")
    if evidence.get("required_consecutive_nights") != REQUIRED_NIGHTS:
        fail(f"required_consecutive_nights must be {REQUIRED_NIGHTS}")
    parse_dates([evidence.get("as_of")])
    runs = evidence.get("runs", [])
    if not runs:
        fail("evidence lists no runs")
    if evidence.get("observed_scheduled_workflow_days") != len(runs):
        fail(f"observed_scheduled_workflow_days does not match {len(runs)} listed runs")
    parsed_dates = parse_dates([run.get("date") for run in runs])
    if parsed_dates != sorted(set(parsed_dates)):
        fail("run dates must be unique and ascending")
    if parsed_dates[-1] > parse_dates([evidence.get("as_of")])[0]:
        fail("a run is dated after as_of")
    complete_nights = 0
    for run in runs:
        both_green = run.get("nightly_crash") == "success" and run.get("nightly_soak") == "success"
        complete_nights += both_green
        derived = both_green and run.get("workflow_conclusion") == "success"
        if run.get("qualifies") is not derived:
            fail(f"run {run.get('run_id')} qualifies={run.get('qualifies')} contradicts its job conclusions")
        if not isinstance(run.get("run_id"), int):
            fail(f"run on {run.get('date')} has no integer run_id")
    if evidence.get("observed_complete_soak_nights") != complete_nights:
        fail(f"observed_complete_soak_nights does not match {complete_nights} nights with both jobs green")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("tag", nargs="?", default="", help="candidate tag; GITHUB_REF_NAME wins when set")
    parser.add_argument("--evidence", type=Path, default=DEFAULT_EVIDENCE, help="nightly-streak evidence file")
    parser.add_argument("--consistency", action="store_true", help="audit the file's self-consistency only")
    args = parser.parse_args()

    evidence = json.loads(args.evidence.read_text(encoding="utf-8"))
    if args.consistency:
        check_consistency(evidence)
        print(f"evidence consistent: {args.evidence.name}, status {evidence.get('status')}, {len(evidence.get('runs', []))} runs")
        return

    tag = os.environ.get("GITHUB_REF_NAME") or args.tag
    check_tag(tag, workspace_version())
    credited = check_evidence(evidence)
    print(f"release approved: {tag}, {credited} qualifying nights")


if __name__ == "__main__":
    main()
