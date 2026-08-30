# Isolate duplicate-restore test process metadata

The owner requested all checks passing before committing and publishing both
repositories. GitHub run 33287862186 failed only
`test_restore_partial_duplicate_restores_missing`; the same full suite passed
locally. The test used synthetic PIDs 1001/1002 with real `/proc` inspection.

Three explanations were checked: test-order interference (the failure also
reproduced in an isolated single-test process); an unexpectedly installed launch
binary (the fixture binary was absent); and host process metadata leaking into
the fixture. A read-only Bubblewrap namespace with an empty `/proc` passed.
Mounting a real stat record at `/proc/1002/stat` in that private namespace
reproduced the exact CI diagnostic: two skips followed by an identity-change
refusal instead of the expected missing-binary failure.

Use the existing injected process-provider entry point with `EmptyProcessInfo`
for this synthetic fixture. Keep every outcome, diagnostic, and no-dispatch
assertion. Changing production identity rules or skipping the test would weaken
the wrong boundary. The constitution's "Ambiguity stops action" remains intact;
no live desktop operations or constitutional changes are authorized or needed.

This diagnosis would be revised if the same stat-overlay reproduction still
failed with isolated process metadata. Validation must repeat that reproduction,
run the full formatting/Clippy/test/build gates, then check the pushed CI result.

The stat-overlay reproduction now passes. All 274 tests, formatting, strict
Clippy, release compilation, and protocol synchronization pass locally. The
change affects only the test fixture; runtime behavior and Canon guarantees
are unchanged.
