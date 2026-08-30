# Structured restore reports

Use the optional report flag when a script or UI needs per-window results:

```sh
hyprloom restore coding --reconcile --report-json
hyprloom restore coding --reconcile --dry-run --report-json
hyprloom replace coding --report-json
```

`replace` still closes existing windows behind its normal validation and safety
backup guards. The reporting flag does not make it non-destructive. Plain
`restore` without `--reconcile` and `--on-login` do not accept this flag.

When a reconciliation report is available, stdout contains one JSON document:

```json
{
  "schema_version": 1,
  "operation": "reconcile",
  "session": "coding",
  "dry_run": false,
  "recovery": null,
  "report": {
    "matched": 1,
    "unchanged": 1,
    "moved": 0,
    "launched": 0,
    "extras": 0,
    "skipped": 0,
    "failed": 0,
    "windows": [{
      "workspace": 3,
      "workspace_name": "3",
      "class": "foot",
      "title": "Project terminal",
      "status": "unchanged",
      "match_kind": "exact identity",
      "message": null
    }]
  }
}
```

This example is illustrative. `operation` is `reconcile` or `replace`.
`recovery` is null unless recovery followed an incomplete replacement report;
then it is `succeeded` or `failed`. Window statuses are:

| Status | Meaning |
| --- | --- |
| `unchanged` | An existing window already satisfied the saved target. |
| `moved` | An existing window was adjusted to the saved target. |
| `launched` | A missing target was restored and its window identified and positioned. |
| `extra` | An unmatched current window was left alone. |
| `skipped` | The engine safely declined to act; see `message`. |
| `failed` | The target could not be restored; see `message`. |

Target rows use the saved workspace/name/title/class; extra rows use the current
window. `workspace_name`, `match_kind`, and `message` may be null. A matched
window that fails a repair counts in both `matched` and `failed`, so `matched`
is not a disjoint outcome count. Human-readable match descriptions and messages
can evolve; do not parse them as machine enums.

With `dry_run: true`, action counts and statuses are plans. No restore actions
were executed. Reports omit full launch argument lists; diagnostics may name
an executable. Window titles can be private, so avoid publishing reports
unintentionally.

Always check the process exit code. Skips and failures return a nonzero code,
as do cleanup/finalization failures even when every window outcome succeeded.
A successful recovery does not turn a failed replacement into success. For
early/fatal errors, there may be no report at all: stdout is empty, diagnostics
are on stderr, and the desktop outcome must be treated as unknown. An age-based
skip can also exit successfully without a report; its notice is on stderr.

The flag leaves default text output unchanged. Schema version 1 permits new
optional fields; consumers should ignore those and reject unsupported versions.
