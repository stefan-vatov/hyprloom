---
status: normative
scope: [session-operations]
validation: [src/restore.rs, tests/reconciliation_integration.rs, tests/inventory_cli.rs]
---

# Session operations

## Additive restore and reconciliation

Reconciliation is additive. It assigns saved targets to current windows one-to-one, uses each current window at most once, and refreshes a matched address before acting on it. Targets already in place remain untouched; mismatched targets are repaired; only genuinely missing targets are launched. Unmatched current windows remain open and are reported as extras.

Repeated reconciliation must be idempotent once the target set is satisfied. If a matched window disappears during refresh, reconciliation must stop safely rather than mutate a different window.

Normal restore provides additive launch and repair behavior. It must not perform the destructive close-first behavior reserved for replacement.

## Guarded replacement

Replacement is an explicit destructive operation. Before closing any current window, it must validate the loaded target and confirm that the target contains something restorable. It must first persist an `autosave-` safety snapshot and durable replacement state.

After those guards succeed, replacement closes the current Hyprland clients, including clients excluded from capture, and restores the one loaded target. A failed or interrupted replacement must retain enough durable state to attempt recovery from the safety snapshot; uncertainty must not be resolved by targeting an arbitrary window.

## Operation start marker

Every CLI operation that acquires the shared operation lock announces its start with exactly one stderr line, `dispatch: started <operation> <name>` — the subcommand and its target session name, or `-` when the operation has no session target. The marker fires only after the lock is actually acquired, so consumers that queue on the helper can measure true start times; a refused lock never emits one.
