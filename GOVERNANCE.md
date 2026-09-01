# Governance

MutinyDB is maintained in public under a lightweight meritocratic model. Maintainers merge work,
cut releases, steward security reports, and protect compatibility and evidence standards.

- Routine changes require one approving maintainer and green required checks.
- Public API, durable-format, security-boundary, and governance changes require a recorded design
  decision plus two maintainer approvals when two active maintainers are available.
- A release tag must match the `mutinyd` package version and pass the release workflow.
- Production approval cannot be inferred from a developer release; its external-assurance gates
  are named in MD-7 and the component lock.
- Maintainer status is earned through sustained, constructive contributions and may be proposed by
  any maintainer in a public governance issue.

If consensus cannot be reached, the project lead makes the final call and records the rationale.
This is the bootstrap model; it should move to a multi-maintainer or foundation home as the
contributor base grows.
