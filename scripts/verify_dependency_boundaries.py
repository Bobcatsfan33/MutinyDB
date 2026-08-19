#!/usr/bin/env python3
"""Fail closed when a MutinyDB crate crosses an undeclared component boundary."""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
ALLOWED_COMPONENT_EDGES = {
    "mutiny-charter": set(),
    "mutiny-bridge": {
        "loom-core",
        "schweep-log",
        "schweep-zset",
        "substrate-pager",
        "substrate-wal",
    },
    "mutiny-semantic": {
        "prism-engine",
        "prism-part",
        "prism-quantizer",
        "prism-types",
        "schweep-zset",
    },
    "mutiny-trust": {"loom-action", "loom-branch", "loom-core", "loom-policy"},
    # M4: the taint core is a trust-plane feature over the compute plane (MD-1 R1 trust→compute).
    # The volatile semantic heal crosses through a trait the host implements, so the core needs
    # neither mutiny-trust nor mutiny-semantic.
    "mutiny-taint": {
        "loom-core",
        "schweep-log",
        "schweep-memo",
        "schweep-server",
        "schweep-zset",
    },
    # M5: the fork-lineage library is pure (loom-core + zset types only as normal deps); the
    # loom-branch and substrate edges belong to tests/m5_spike.rs — the committed CoW spike
    # evidence behind MD-5's verdict — and cargo metadata counts dev-dependencies.
    "mutiny-forks": {
        "loom-branch",
        "loom-core",
        "schweep-zset",
        "substrate-pager",
    },
    # M6: mutinyd is the one surface — the composed-development form of the product binary
    # (docs/M6-SURFACE.md's quarantine notice governs its distribution). It composes every plane
    # behind one admission boundary, which is why its reviewed edge set is the union.
    "mutinyd": {
        "loom-action",
        "loom-branch",
        "loom-core",
        "loom-policy",
        "prism-types",
        "schweep-log",
        "schweep-memo",
        "schweep-server",
        "schweep-zset",
        "substrate-pager",
        "substrate-wal",
    },
    # M4: the dev-only incident host composes every plane the supported M6 binary eventually
    # will; it is not that binary and produces no release artifact.
    "mutiny-incident": {
        "loom-action",
        "loom-branch",
        "loom-core",
        "loom-policy",
        "prism-types",
        "schweep-log",
        "schweep-memo",
        "schweep-server",
        "schweep-zset",
        "substrate-pager",
        "substrate-wal",
    },
}
COMPONENT_ROOT = ROOT / "components"


def fail(message: str) -> None:
    print(f"dependency-boundary: {message}", file=sys.stderr)
    raise SystemExit(1)


def main() -> None:
    result = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked"],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        fail(f"cargo metadata failed: {result.stderr.strip()}")
    document = json.loads(result.stdout)
    packages = {package["id"]: package for package in document["packages"]}
    nodes = {node["id"]: node for node in document["resolve"]["nodes"]}

    checked = 0
    for package in packages.values():
        manifest = Path(package["manifest_path"]).resolve()
        if manifest.parent.parent != ROOT / "crates":
            continue
        name = package["name"]
        if name not in ALLOWED_COMPONENT_EDGES:
            fail(f"workspace crate {name!r} has no reviewed boundary declaration")
        actual: set[str] = set()
        for dependency in nodes[package["id"]]["deps"]:
            target = packages[dependency["pkg"]]
            target_manifest = Path(target["manifest_path"]).resolve()
            if target_manifest.is_relative_to(COMPONENT_ROOT):
                actual.add(target["name"])
            elif target["source"] is None and not target_manifest.is_relative_to(ROOT):
                fail(f"{name}: local dependency escapes the repository: {target_manifest}")
        expected = ALLOWED_COMPONENT_EDGES[name]
        if actual != expected:
            fail(
                f"{name}: component edges are {sorted(actual)}, expected {sorted(expected)}"
            )
        checked += 1

    if checked != len(ALLOWED_COMPONENT_EDGES):
        fail(f"checked {checked} workspace crates, expected {len(ALLOWED_COMPONENT_EDGES)}")
    print(
        "dependency-boundary: reviewed direct component edges verified for "
        f"{checked} workspace crates"
    )


if __name__ == "__main__":
    main()
