---
status: normative
scope: [storage, recovery]
validation: [src/session.rs, src/restore.rs]
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
