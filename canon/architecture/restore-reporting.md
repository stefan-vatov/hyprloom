---
status: normative
scope: [reporting, cli]
validation: [tests/restore_reporting_cli.rs, src/restore.rs]
---

# Restore reporting

Structured reporting is opt-in. `restore --reconcile --report-json` and
`replace --report-json` return a versioned JSON document on stdout when an
operation produces a reconciliation report. Default human output and exit
semantics remain available unchanged. Diagnostic notices must not contaminate
the JSON document. The report flag does not authorize additional desktop work.

Each target outcome identifies the saved workspace and window and distinguishes
unchanged existing windows, repaired existing windows, restored missing targets,
safe skips, and failures. Extras identify unmatched current windows and their
current workspace. Outcomes are available regardless of verbosity. A launch is
not reported successful until correlation and placement have succeeded.

Dry-run outcomes describe intended actions, never completed mutations. Partial
failures preserve the nonzero exit status. Replacement results describe the
target attempt; subsequent safety recovery is identified separately, and its
success does not make the requested replacement successful.

Early or fatal failures that do not return a report leave stdout empty and
report diagnostics on stderr. Absence of a report means the result is unknown,
not that zero windows changed. Consumers must handle that case and check exit
status even when a report exists, including finalization or cleanup failures.

Schema versions distinguish incompatible format changes. Consumers may ignore
additional fields within a supported version but must not interpret an unknown
schema as success. Human diagnostic text and match descriptions are not stable
machine enums; window status values are the machine-facing outcome contract.
