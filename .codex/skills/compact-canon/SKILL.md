---
name: compact-canon
description: Audit and safely rework an overgrown Project Canon (`canon/`) into a compact set of architectural laws, product invariants, decisions, and validation links. Use when asked to compact, prune, deduplicate, reorganize, migrate, or reduce Canon; remove implementation inventories or legacy `sources`/`verified` metadata; tighten manifest routing; or assess Canon maintenance burden in a dry run.
---

# Compact Canon

Reduce maintenance burden without weakening durable architectural memory.
Canon should survive file moves because it records why the system is shaped
this way and what must remain true, not where every implementation lives.

## Choose the mode

- Treat `audit`, `assess`, `mock`, `plan`, or `dry run` as read-only.
- Treat an explicit request to `compact`, `clean`, `prune`, `deduplicate`,
  `tighten`, `reorganize`, `migrate`, or `rework` as authorization to edit
  `canon/` only.
- Do not edit code, tests, build configuration, generated reports, or decision
  records unless the user separately puts them in scope.
- Do not commit, push, stash, reset, restore, stage, or delete unrelated work.

Apply mode requires a completely clean worktree at entry, including untracked
files. If it is dirty, report the exact paths and stop after the audit. Record
clean status and `BASE_HEAD` as the transaction boundary. Track every
run-owned path and stop if unrelated dirt appears. Never delete an untracked
Canon file.

## Establish the baseline

1. Read applicable repository instructions.
2. Record `git status --short`, `BASE_HEAD`, and hashes of every decision.
3. Read `canon/manifest.md`, then the routed normative pages in manifest order.
   Read reference or draft pages only when needed to classify them. Do not read
   `canon/scratch/` unless the user explicitly included scratch in the task.
4. Run the bundled analyzer, `scripts/analyze_canon.py`, from wherever this
   skill is installed — commonly `.claude/skills/compact-canon/` or
   `.codex/skills/compact-canon/`, but use this file's actual directory:

   ```sh
   python3 <this-skill-directory>/scripts/analyze_canon.py --root /path/to/repo
   ```

   Use `--json` when another tool will consume the report.
5. Run `tools/canon-doctor.py --root <repo> --json` when it exists. Mechanical
   findings are evidence, not permission to rewrite policy.

Record before-metrics: permanent files, lines, bytes, status counts, normative
route coverage, broken links, metadata errors, size-cap failures, repeated
paragraphs, overlap candidates, implementation-inventory signals, doctor
findings, and decision hashes.

## Classify every permanent page

Assign one disposition and a short evidence statement:

- **Keep** — unique durable law, invariant, decision, rationale, or validation
  expectation.
- **Merge** — durable content duplicated across pages that has one clear owner.
- **Rewrite** — useful rules obscured by chronology, repetition, inventories,
  or implementation detail.
- **Move out** — useful commands, setup, troubleshooting, or generated state
  that belongs in development or generated documentation.
- **Delete** — content reconstructible from localized code or ordinary search,
  obsolete current-state prose, completed-work narration, or exact duplicate.
- **Abstain** — normative, ambiguous, weakly sourced, or unsafe to change.

Retain a claim only when all are true:

1. A future session is likely to need it.
2. Getting it wrong would materially change a decision or implementation.
3. It is stable and has an identified human owner or executable validation.
4. It is not reliably reconstructible from code, tests, types, schemas,
   manifests, build graphs, generated reports, or ordinary search.

For a proposed merge, rewrite, move, or deletion, inspect only the smallest
relevant repository evidence. Do not trust Canon prose as proof of its own
value. Size, age, long lines, and lexical similarity are investigation
signals, never deletion proof.

## Preserve these boundaries

- Never edit, delete, rename, combine, or reuse an existing decision record.
  A new human direction may supersede one only through a new record.
- Treat legacy metadata inside an existing decision as immutable history. Do
  not remove or retrofit it; only new decision records use the compact schema.
- Preserve the `standards.md` body verbatim by default. An explicit migration
  may add schema front matter; a semantic rewrite requires human direction.
- Preserve supplied rationale, rejected alternatives, numeric limits,
  exceptions, invalid cases, error behavior, negations, and uncertainty
  exactly in meaning. Preserve the whole durable contract, not a representative
  subset.
- Keep `manifest.md` and `standards.md`. Keep every normative page routed.
  Each route has exactly one local Markdown link and an explicit, non-empty
  `read when ...` or `read for ...` condition.
- Never route, cite, promote, or normally read `canon/scratch/`.
- When scratch is explicitly in scope, retain active material only with an
  owner and expiry condition where practical.
- Do not archive deleted bloat into another permanent page or scratch.
- A smaller Canon is not automatically better. Set no deletion quota.
- Never fill a policy gap by guessing a value, rule, exception, owner, or
  rationale. Abstain on the policy-dependent edit and report the exact missing
  authority.

## Apply the invariant-first model

Create an operation ledger before editing. Then:

1. Rewrite rules in place before moving or deleting pages.
2. Replace exhaustive instance lists with one general ownership or behavior
   rule. Keep at most a few explicitly non-exhaustive examples when they aid
   comprehension.
3. Remove legacy `sources` and `verified` metadata from non-decision pages.
   Add only truthful compact metadata:

   ```yaml
   ---
   status: normative
   scope:
     - package-or-architectural-area
   validation:
     - path/to/policy_test
   related:
     - ./related-page.md
   ---
   ```

   Do not invent scopes, validation, or relationships. Validation entries are
   existing regular, non-symlink files named from the repository root, never
   paths relative to the Canon page. Missing automation is evidence debt to
   report.
4. Put each cross-cutting fact in one owning page and link from intentional
   exceptions. Never copy whole policy tables between pages.
5. Move operational guidance to existing development docs when separately
   authorized; otherwise report the move as follow-up work.
6. Update the manifest and inbound links atomically with every page move,
   merge, or deletion. The manifest is a concern-to-page router, not a file
   catalog.
7. Remove changelog prose, completed-task summaries, speculative notes,
   temporary migration status, internal function lists, line numbers, and
   routine implementation detail.
8. Leave uncertain norms and immutable history unchanged and list them under
   `Abstained`.
9. For an explicitly authorized supersession, make `supersedes` a non-empty
   list of predecessor decision paths, preserve those records byte-for-byte,
   and update the owning current-state page to state the active rule or value
   explicitly. Keep each predecessor routed from the manifest as clearly
   labeled historical decision context.

Use patch-based edits. Preserve pre-existing user changes. Parallel workers,
when explicitly authorized, must own disjoint paths; reserve shared indexes
and the manifest for one coordinator.

## Verify the result

1. Run the repository formatter or formatting check only on run-owned paths.
2. Re-run the analyzer and Canon doctor with `--baseline "$BASE_HEAD"`.
   Resolve malformed front matter,
   broken routes and links, missing normative routes, unsafe validation
   references, size failures, and new inventory warnings.
3. Run `git diff --check`; inspect the complete status and
   `git diff BASE_HEAD -- canon/`. Reject every unowned change.
4. Reconcile the operation ledger with the diff. Every removed durable claim
   needs one retained owner; every deletion needs retention-test evidence.
5. Search the final Canon for each active decision name, public contract,
   security rule, numeric limit, exception, and domain term from the baseline.
6. Give every normative rewrite a cold review against `BASE_HEAD`, final
   formatted bytes, and the smallest relevant evidence.
7. Confirm decision paths and hashes are identical and the standards body is
   unchanged except for authorized schema metadata unless the user explicitly
   authorized a semantic rewrite.

Report before/after metrics, pages merged/moved/removed, inventory eliminated,
durable rules preserved, abstentions, formatter/analyzer/doctor results,
semantic-review corrections, `BASE_HEAD`, exact changed paths, and commit
status. In a dry run, report candidates and projected savings without edits.
