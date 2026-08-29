---
status: normative
scope: [autosave]
validation: [src/autosave.rs, src/session.rs]
---

# Autosave lifecycle

## Timer-driven capture and isolated rotation

The installed user timer runs the Hyprloom autosave operation periodically; that operation captures a new uniquely named `autosave-` snapshot and then applies retention. Rotation considers only autosave-prefixed snapshots, leaves named user sessions untouched, keeps the newest configured count, and treats zero retention as removal of all unprotected autosaves. A safety snapshot referenced by an active replacement remains protected.

## Transactional unit lifecycle

Autosave service and timer installation must reject unsafe unit directories and transaction markers. The service and timer are installed as one recoverable transaction using atomic writes and durable transaction state. If installation is interrupted or a write fails, recovery restores the exact previous pair rather than leaving a partial update.

When legacy HyprFlow autosave units exist, installation migrates to the Hyprloom units and preserves whether the legacy timer was enabled. Uninstall settles any interrupted installation before removing current and legacy units.
