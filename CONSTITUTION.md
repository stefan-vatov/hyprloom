# CONSTITUTION.md

## Preamble

Hyprloom exists to give Hyprland users direct CLI control over automated
workspace and window management. It captures named session presets—including
windows, workspaces, positions, and monitor layouts—and can restore them after
reboot or disruption.

It can reconcile a preset with the current desktop by retaining windows already
in place, repairing mismatches, launching what is missing, and leaving unrelated
windows alone; when the user deliberately requests replacement, it can close
the current desktop behind safety and recovery guards.

Its future capabilities are open-ended, but its purpose remains anchored to
managing Hyprland workspaces and the windows within them.

## Founding Principles

1. **User state is more important than task completion.** Hyprloom must prefer
   leaving a requested workspace incomplete over risking damage to the user's
   live desktop. This rejects speed, convenience, and "best effort" when they
   compromise safety.

2. **Ambiguity stops action.** When Hyprloom cannot confidently identify the
   correct window, target, or recovery path, it must not guess. This rejects
   plausible-but-unproven mutation of desktop state.

3. **Destruction requires explicit intent.** Any operation that closes windows
   must sit behind an unmistakable, explicitly requested CLI action or flag.
   Destructive behavior must never emerge as a hidden consequence of an
   additive command.

4. **A safe refusal is a usable result.** When Hyprloom declines an action, it
   must explain why clearly enough that a person or direct consumer can respond
   appropriately. This rejects silent failure and opaque errors.

5. **Compatibility changes must be honest.** Breaking changes must be obvious
   to consumers through proper semantic versioning and a clear changelog.
   This rejects silent contract drift that surprises scripts, plugins, or
   users.

## Growth Directives

- **Become the foundation for Hyprland workspace automation.** Hyprloom should
  grow as the dependable primitive layer used by scripts, plugins such as
  Deskloom, and other higher-level abstractions, while remaining useful
  directly from the CLI.
- **Grow through composable interfaces.** New capabilities should expose clear
  building blocks that consumers can combine into workflows rather than forcing
  every use case through one monolithic operation.
- **Provide convenience without concealing capability.** Hyprloom may offer
  composed operations for common consumer needs, but the underlying primitives
  must remain available for direct use and further composition.

## Boundaries

- **Never become general desktop automation.** Hyprloom may capture, persist,
  launch, place, reconcile, close, and recover windows only as part of Hyprland
  workspace management. Unrelated application, desktop, or system automation
  is outside its purpose.
- **Remain the workspace engine.** Hyprloom performs the actual workspace and
  window-management work through reusable primitives and convenient composed
  operations. It must be usable on its own and easy to script. Presentation and
  higher-level application behavior belong in other apps.
- **Never generalize beyond Hyprland.** Hyprland is a permanent platform
  boundary, not merely the first backend of a compositor-agnostic framework.

## Tension Pairs

- **Simple consumer interfaces over simple internals** — but never at the cost
  of safety. Hyprloom should absorb complexity internally so its primitives
  remain straightforward to compose and use.
- **Additive evolution over breaking replacement** — but never at the cost of
  blocking useful improvements. Introduce enhanced capabilities alongside
  existing ones, retaining old interfaces with explicit deprecation notices
  rather than silently replacing their behavior.
- **Useful convenience over rigid primitive-only minimalism** — but never at
  the cost of hiding the building blocks or leaving the project's domain. Add
  composed operations when a real need exists; no universal rule must justify
  every such addition.

## Amendments

This constitution was ratified on 2026-08-30. It carries no amendment log; Git
preserves the history of replaced constitutional text.

### Amendment Process

Only the human project owner may initiate, approve, and ratify an amendment.
Amendments are purely owner-initiated; there is no automatic or periodic review.
Changes go through the official constitution-writing skill's questioning,
conflict-checking, and explicit approval process for every affected section.

Approved text replaces the old text. Agents may apply this constitution but
cannot independently amend it or propose amendments.
