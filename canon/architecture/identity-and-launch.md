---
status: normative
scope: [identity, launch]
validation: [src/capture.rs, src/restore.rs]
---

# Identity and launch

## Fail-closed identity

Window matching and post-launch correlation must fail closed when identity is ambiguous. Hyprloom must not move an unrelated same-class window, choose arbitrarily among equivalent candidates, or trust a reused process identifier without a matching process start identity. When reliable process identity is unavailable, the launch cannot be correlated on that basis.

Browser identity must remain window-specific. A generic Chromium class without reliable site identity is treated as a normal browser window, not guessed to be a web app. A Brave target without reliable per-window profile identity is skipped rather than assigned a profile by window count or shared process membership.

## Authorized executable sources

Restore may launch only a command captured from application identity or explicitly authorized by the matching `apps.<class>.binary` configuration. Missing or unauthorized commands are reported as failures, including during dry runs, and must not be spawned.

After spawning, Hyprloom may position only a window correlated to the launched process or otherwise identified unambiguously. Ambiguous or unrelated candidates must produce failure without dispatching window mutations to them.
