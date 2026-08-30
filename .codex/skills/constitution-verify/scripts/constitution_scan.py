#!/usr/bin/env python3
"""Deterministic mechanics for constitution-verify.

Judgment stays with the model; this script only gathers facts:
  - wiring: CONSTITUTION.md presence, managed-block integrity in
    instruction files
  - diff inventory: changed files for a range / staged / working tree
  - chunk plan: whole-project file groups for chunk-by-chunk inspection
  - footer claims: `Constitution:` lines found in commit messages

Output is JSON on stdout. Read-only; never modifies anything.
"""

import argparse
import json
import subprocess
import sys
from pathlib import Path

BLOCK_BEGIN = "<!-- BEGIN PROJECT CONSTITUTION -->"
BLOCK_END = "<!-- END PROJECT CONSTITUTION -->"
INSTRUCTION_FILES = ("AGENTS.md", "CLAUDE.md")
SKIP_DIRS = {".git", "node_modules", "dist", ".astro", "__pycache__",
             ".venv", "venv", "target", "build", ".next"}


def run_git(root: Path, *args: str) -> str | None:
    try:
        out = subprocess.run(
            ["git", "-C", str(root), *args],
            capture_output=True, text=True, timeout=60,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    return out.stdout if out.returncode == 0 else None


def wiring(root: Path) -> dict:
    constitution = root / "CONSTITUTION.md"
    report = {
        "constitution_present": constitution.is_file(),
        "constitution_bytes": constitution.stat().st_size if constitution.is_file() else 0,
        "instruction_files": {},
    }
    for name in INSTRUCTION_FILES:
        path = root / name
        entry = {"present": path.is_file(), "block_pairs": 0, "links_constitution": False}
        if path.is_file():
            text = path.read_text(encoding="utf-8", errors="replace")
            begins, ends = text.count(BLOCK_BEGIN), text.count(BLOCK_END)
            entry["block_pairs"] = min(begins, ends) if begins == ends else -1  # -1 = malformed
            if begins == 1 and ends == 1:
                inner = text.split(BLOCK_BEGIN, 1)[1].split(BLOCK_END, 1)[0]
                entry["links_constitution"] = "CONSTITUTION.md" in inner
        report["instruction_files"][name] = entry
    return report


def diff_inventory(root: Path, base: str | None, staged: bool) -> dict:
    if staged:
        raw = run_git(root, "diff", "--cached", "--name-status")
        label = "staged"
    elif base:
        merge_base = (run_git(root, "merge-base", base, "HEAD") or "").strip()
        ref = merge_base or base
        raw = run_git(root, "diff", "--name-status", f"{ref}...HEAD") or \
            run_git(root, "diff", "--name-status", f"{ref}..HEAD")
        label = f"{base}..HEAD (merge-base {merge_base[:12] or 'n/a'})"
    else:
        raw = run_git(root, "diff", "--name-status")
        label = "working tree"
    files = []
    for line in (raw or "").splitlines():
        parts = line.split("\t")
        if len(parts) >= 2:
            files.append({"status": parts[0], "path": parts[-1]})
    return {"range": label, "available": raw is not None, "files": files}


def footer_claims(root: Path, base: str | None) -> list:
    spec = f"{base}..HEAD" if base else "-20"
    raw = run_git(root, "log", spec, "--format=%H%x00%B%x01")
    claims = []
    for record in (raw or "").split("\x01"):
        if "\x00" not in record:
            continue
        sha, body = record.split("\x00", 1)
        for line in body.splitlines():
            if line.strip().startswith("Constitution:"):
                claims.append({"commit": sha.strip()[:12], "claim": line.strip()})
    return claims


def chunk_plan(root: Path, chunk_bytes: int) -> list:
    files = []
    for path in sorted(root.rglob("*")):
        if not path.is_file() or path.is_symlink():
            continue
        if any(part in SKIP_DIRS for part in path.parts):
            continue
        try:
            size = path.stat().st_size
        except OSError:
            continue
        files.append((str(path.relative_to(root)), size))
    chunks, current, current_size = [], [], 0
    for rel, size in files:
        if current and current_size + size > chunk_bytes:
            chunks.append(current)
            current, current_size = [], 0
        current.append(rel)
        current_size += size
    if current:
        chunks.append(current)
    return [{"chunk": i + 1, "files": chunk} for i, chunk in enumerate(chunks)]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", default=".", help="project root (default: .)")
    parser.add_argument("--base", help="base ref for diff/claims (e.g. main)")
    parser.add_argument("--staged", action="store_true", help="inspect staged changes")
    parser.add_argument("--project", action="store_true", help="emit whole-project chunk plan")
    parser.add_argument("--chunk-bytes", type=int, default=65536,
                        help="approximate bytes per project chunk (default 65536)")
    args = parser.parse_args()

    root = Path(args.root).resolve()
    if not root.is_dir():
        print(json.dumps({"error": f"root is not a directory: {root}"}))
        return 1

    result = {"root": str(root), "wiring": wiring(root)}
    result["diff"] = diff_inventory(root, args.base, args.staged)
    result["footer_claims"] = footer_claims(root, args.base)
    if args.project:
        result["chunk_plan"] = chunk_plan(root, args.chunk_bytes)
    print(json.dumps(result, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
