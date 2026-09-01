# Contributing to MutinyDB

MutinyDB welcomes bug reports, design discussions, documentation, integrations, and code. The
project values measured claims, small reviewable changes, and tests that prove they can fail.

## Start here

1. Search existing issues and discussions. For a substantial protocol, durable-format, or query
   language change, open a design issue before implementation.
2. Fork the repository and create a focused branch.
3. Keep dependency directions within the plane matrix in `docs/decisions/MD-1.md`.
4. Add a differential, model, or failure-injection test appropriate to the claim you changed.
5. Run the local gate below and open a pull request using the template.

```sh
python3 scripts/verify_component_lock.py
python3 scripts/verify_dependency_boundaries.py
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
PYTHONPATH=sdk/python/src python3 -m unittest discover -s sdk/python/tests -v
(cd sdk/typescript && npm ci && npm run lint:api && npm test)
```

The full crash and soak jobs run in CI. Do not rewrite evidence to make a gate green; fix the
system or document the limitation. Imported component trees are exact provenance snapshots and
must be updated through `components.lock.json`, never edited casually in place.

## Compatibility

The public v0.1 HTTP and MCP surfaces are additive-only until v1. Breaking changes need a version
bump, migration notes, and a decision record. Python and TypeScript client releases track the
server minor version.

## Developer Certificate of Origin

By contributing, you certify that you have the right to submit the work under Apache-2.0. Add
`Signed-off-by: Name <email>` to commits (`git commit -s`).
