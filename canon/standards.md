---
status: normative
scope: [repository]
validation: [.github/workflows/ci.yml]
---

# Engineering standards

Every change must pass the repository CI validation gate:

- formatting is clean under `cargo fmt -- --check`;
- Clippy passes for all targets and all features with the lockfile, with warnings denied;
- all tests pass with the lockfile;
- the vendored Harder to Fool block in `AGENTS.md` is synchronized with `.ai/harder-to-fool/CODE.md` under `.ai/harder-to-fool/check_sync.py`.
