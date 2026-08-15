#!/usr/bin/env python3
"""Verify that every imported component is exact, licensed, and honestly admitted."""

from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
LOCK = ROOT / "components.lock.json"
EXPECTED = {"substrate", "loomdb", "prismdb", "schweep"}
SHA = re.compile(r"^[0-9a-f]{40}$")


def fail(message: str) -> None:
    print(f"component-lock: {message}", file=sys.stderr)
    raise SystemExit(1)


def component_tree(path: str) -> str:
    result = subprocess.run(
        ["git", "write-tree", f"--prefix={path}/"],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        fail(f"cannot compute the indexed tree for {path}: {result.stderr.strip()}")
    return result.stdout.strip()


def main() -> None:
    try:
        document = json.loads(LOCK.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        fail(f"cannot read {LOCK.relative_to(ROOT)}: {error}")

    if document.get("schemaVersion") != 1:
        fail("schemaVersion must be 1")
    if document.get("product") != "MutinyDB":
        fail("product must be MutinyDB")
    if document.get("sourceMode") != "monorepo-import":
        fail("sourceMode must be monorepo-import")

    components = document.get("components")
    if not isinstance(components, list):
        fail("components must be a list")
    names = [component.get("name") for component in components if isinstance(component, dict)]
    if set(names) != EXPECTED or len(names) != len(EXPECTED):
        fail(f"component names must be exactly {sorted(EXPECTED)}, found {names}")

    for component in components:
        name = component["name"]
        path = component.get("path")
        repository = component.get("repository")
        commit = component.get("commit")
        source_tree = component.get("sourceTree")
        tree = component.get("tree")
        release = component.get("releaseTag")
        blockers = component.get("blockers")

        if not isinstance(path, str) or path != f"components/{name}":
            fail(f"{name}: path must be components/{name}")
        if not isinstance(repository, str) or not repository.startswith("https://github.com/"):
            fail(f"{name}: repository must be an HTTPS GitHub URL")
        for field, value in (("commit", commit), ("sourceTree", source_tree), ("tree", tree)):
            if not isinstance(value, str) or not SHA.fullmatch(value):
                fail(f"{name}: {field} must be a full lowercase Git SHA")
        if release is not None and (not isinstance(release, str) or not release):
            fail(f"{name}: releaseTag must be null or a non-empty exact tag")
        if component.get("admitted") is True and release is None:
            fail(f"{name}: an unreleased component cannot be admitted")
        if component.get("admitted") is True and blockers:
            fail(f"{name}: an admitted component cannot retain blockers")
        if component.get("admitted") is not True and not (
            isinstance(blockers, list) and blockers and all(isinstance(item, str) and item for item in blockers)
        ):
            fail(f"{name}: a quarantined component must name at least one blocker")

        directory = ROOT / path
        if not directory.is_dir():
            fail(f"{name}: imported directory {path} is missing")
        if (directory / ".git").exists():
            fail(f"{name}: nested .git metadata is forbidden; imports are ordinary product source")
        if not (directory / "LICENSE").is_file():
            fail(f"{name}: imported source has no LICENSE")
        actual_tree = component_tree(path)
        if actual_tree != tree:
            fail(f"{name}: indexed tree is {actual_tree}, lock declares {tree}")

    print(
        "component-lock: 4 exact source imports verified; "
        f"{sum(1 for item in components if item['admitted'])} admitted, "
        f"{sum(1 for item in components if not item['admitted'])} quarantined"
    )


if __name__ == "__main__":
    main()
