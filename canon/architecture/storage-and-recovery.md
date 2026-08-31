---
status: normative
scope: [storage, recovery]
validation: [src/session.rs, src/restore.rs, tests/inventory_cli.rs]
---

# Storage and recovery

## Validated private snapshots

Sessions are JSON snapshots under the Hyprloom user data directory. Session names must be safe single path components, and a loaded payload name must match its filename. Size and structural limits are checked before accepting a read or committing a write.

Session directories and files must be private to the current user. Symlinked storage directories are rejected. Saving over a destination symlink replaces the link itself without following it. Completed snapshots and replacement markers are written atomically so readers do not observe partial state.

## Legacy migration

Legacy HyprFlow sessions are copied into Hyprloom storage on first use without removing legacy data or overwriting existing Hyprloom snapshots. Migration validates candidates independently, skips malformed or oversized files while retaining valid files, and does not re-copy a snapshot that a user subsequently deleted from Hyprloom storage.

## Conservative replacement recovery

Replacement recovery is tied to the safety snapshot, the durable replacement phase, and the exact target snapshot fingerprint when present. Window address, process identity, and stable window identity are retained where available so address or process reuse cannot be mistaken for progress.

Recovery must remain conservative when older markers lack identity or target information. It must not declare an interrupted replacement complete from ambiguous evidence. An in-progress replacement safety snapshot is protected from autosave rotation until recovery state is cleared.

## Content-addressed revisions

Every stored session has a content revision: the first 16 hexadecimal characters of the SHA-256 digest over the raw session file bytes. The revision is recomputed from disk at read time and never persisted, so it always identifies exactly the bytes a reader would load.

`save --force`, `replace`, and `delete` accept an optional `--if-revision` guard. Before any mutation — including capturing a replacement safety snapshot — the target session's current revision is recomputed. A malformed revision token is a usage error and mutates nothing. A stale revision or a missing session is a conflict: the helper prints a versioned `revision-conflict` JSON document on stdout, the full plain-text explanation on stderr, exits with status 3, and modifies nothing. On a match the operation proceeds unchanged.

`list --json` emits the versioned `deskloom.inventory` document: a schema version, the protocol name, and one entry per valid session with its name, content revision, window count, creation time (RFC 3339), and automatic flag. Human `list` output remains unchanged, and diagnostics must never contaminate the JSON document.
