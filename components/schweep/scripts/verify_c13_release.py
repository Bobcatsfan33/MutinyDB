#!/usr/bin/env python3
"""Fail closed unless the C13 release evidence and tag are complete."""

import json
from datetime import date
import os
from pathlib import Path
import re
import sys


ROOT = Path(__file__).resolve().parents[1]


def fail(message: str) -> None:
    print(f"release blocked: {message}", file=sys.stderr)
    raise SystemExit(1)


workspace = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
match = re.search(r"\[workspace\.package\].*?^version = \"([^\"]+)\"", workspace, re.M | re.S)
if match is None:
    fail("Cargo.toml has no workspace package version")
version = match.group(1)
major, minor, patch = version.split(".")
expected_tag = f"current-v{major}.{minor}"
tag = os.environ.get("GITHUB_REF_NAME") or (sys.argv[1] if len(sys.argv) == 2 else "")
if tag != expected_tag:
    fail(f"tag must be {expected_tag}, got {tag or '<none>'}")
if patch != "0":
    fail("the frozen current-v0.1 tag must point at the initial 0.1.0 package version")

evidence = json.loads((ROOT / "testing/evidence/c13-nightly-streak.json").read_text())
runs = evidence.get("runs", [])
qualifying = [run for run in runs if run.get("qualifies") is True]
required = evidence.get("required_consecutive_nights")
if evidence.get("status") != "complete" or evidence.get("release_blocked") is not False:
    fail("nightly evidence is not marked complete")
if required != 7 or len(qualifying) < required:
    fail(f"need 7 qualifying nightly runs, found {len(qualifying)}")
dates = [run.get("date") for run in qualifying[-required:]]
if len(dates) != len(set(dates)):
    fail("qualifying night dates are not unique")
try:
    parsed_dates = [date.fromisoformat(value) for value in dates]
except (TypeError, ValueError) as error:
    fail(f"qualifying night date is invalid: {error}")
for previous, current in zip(parsed_dates, parsed_dates[1:]):
    if (current - previous).days != 1:
        fail(f"qualifying dates are not consecutive: {previous} then {current}")
for run in qualifying[-required:]:
    if run.get("workflow_conclusion") != "success":
        fail(f"workflow {run.get('run_id')} is not successful")
    if run.get("nightly_crash") != "success" or run.get("nightly_soak") != "success":
        fail(f"workflow {run.get('run_id')} lacks a green required nightly job")

print(f"release approved: {tag}, {len(qualifying[-required:])} qualifying nights")
