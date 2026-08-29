#!/usr/bin/env python3
"""Read-only inventory for compact, invariant-first Project Canon repositories."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import posixpath
import re
from collections import Counter, defaultdict
from pathlib import Path
from urllib.parse import unquote, urlsplit


MAX_BYTES = 64 * 1024
MAX_LINES = 250
MAX_OVERLAP_PAIRS = 100_000
STATUSES = {"normative", "reference", "draft", "deprecated"}
LEGACY_FIELDS = {"sources", "verified"}
WORD_RE = re.compile(r"[a-z][a-z0-9_-]{2,}")
MANIFEST_LINK_RE = re.compile(
    r"\[[^\]\n]+\]\(\s*(?:<(?P<angle>[^>\n]{1,1024})>|(?P<plain>[^\s)]{1,1024}))"
    r"(?:\s+(?:\"[^\"]*\"|'[^']*'|\([^)]*\)))?\s*\)"
)
SOURCE_REFERENCE_RE = re.compile(
    r"`[^`\n]*\.(?:c|cc|cpp|cs|exs?|go|java|jsx?|kt|php|py|rb|rs|swift|tsx?)"
    r"(?::\d+)?`"
)
FRONTMATTER_RE = re.compile(r"\A---\s*\n(.*?)\n---\s*\n", re.DOTALL)
FRONTMATTER_KEY_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_-]*")
PLAIN_STRING_RE = re.compile(r'''[A-Za-z0-9._/@:+-][^\s\[\]{},#&*!|>"`\\]*''')
IMPLICIT_NON_STRING_RE = re.compile(
    r"(?ix)(?:"
    r"null|~|true|false|yes|no|on|off|"
    r"[-+]?(?:0b[01_]+|0o[0-7_]+|0x[0-9a-f_]+|[0-9][0-9_]*"
    r"(?:\.[0-9_]*)?(?:e[-+]?[0-9]+)?|\.[0-9_]+(?:e[-+]?[0-9]+)?)|"
    r"[-+]?\.(?:inf|nan)|"
    r"[0-9]{4}-[0-9]{2}-[0-9]{2}(?:[tT ]\S+)?"
    r")"
)
STOP_WORDS = {
    "and", "are", "but", "canon", "file", "files", "for", "from", "has",
    "have", "into", "must", "not", "only", "project", "should", "that",
    "the", "their", "then", "this", "use", "when", "with",
}


def is_escaped(text: str, index: int) -> bool:
    backslashes = 0
    index -= 1
    while index >= 0 and text[index] == "\\":
        backslashes += 1
        index -= 1
    return backslashes % 2 == 1


def strip_inline_code(text: str) -> str:
    """Blank CommonMark-style code spans while preserving line positions."""
    characters = list(text)
    index = 0
    while index < len(text):
        if text[index] != "`" or is_escaped(text, index):
            index += 1
            continue
        opener_end = index
        while opener_end < len(text) and text[opener_end] == "`":
            opener_end += 1
        width = opener_end - index
        cursor = opener_end
        closing_end = None
        while cursor < len(text):
            if text[cursor] == "\n":
                line_end = text.find("\n", cursor + 1)
                if line_end == -1:
                    line_end = len(text)
                if not text[cursor + 1 : line_end].strip():
                    # A blank line ends the paragraph; a code span cannot
                    # continue across it, so the opener is a literal backtick.
                    break
                cursor += 1
                continue
            if text[cursor] != "`":
                cursor += 1
                continue
            run_end = cursor
            while run_end < len(text) and text[run_end] == "`":
                run_end += 1
            if run_end - cursor == width:
                closing_end = run_end
                break
            cursor = run_end
        if closing_end is None:
            index = opener_end
            continue
        for position in range(index, closing_end):
            if characters[position] != "\n":
                characters[position] = " "
        index = closing_end
    return "".join(characters)


def is_real_link(text: str, start: int) -> bool:
    if is_escaped(text, start):
        return False
    return not (
        start > 0
        and text[start - 1] == "!"
        and not is_escaped(text, start - 1)
    )


def strip_yaml_comment(value: str) -> str | None:
    quote = None
    escaped = False
    skip_next = False
    for index, char in enumerate(value):
        if skip_next:
            skip_next = False
            continue
        if escaped:
            escaped = False
            continue
        if quote == '"' and char == "\\":
            escaped = True
            continue
        if quote:
            if (
                char == quote
                and quote == "'"
                and index + 1 < len(value)
                and value[index + 1] == "'"
            ):
                skip_next = True
                continue
            if char == quote:
                quote = None
            continue
        prefix = value[:index].rstrip()
        if char in "'\"" and (not prefix or prefix.endswith(("[", ","))):
            quote = char
        elif char == "#" and (index == 0 or value[index - 1].isspace()):
            return value[:index].rstrip()
    return None if quote or escaped else value.rstrip()


def parse_yaml_scalar(raw: str) -> str | None:
    value = strip_yaml_comment(raw.strip())
    if value is None or not value:
        return None
    if value.startswith('"'):
        try:
            parsed = json.loads(value)
        except (json.JSONDecodeError, TypeError):
            return None
        return parsed if isinstance(parsed, str) else None
    if value.startswith("'"):
        if len(value) < 2 or not value.endswith("'"):
            return None
        inner = value[1:-1]
        index = 0
        result = []
        while index < len(inner):
            if inner[index] == "'":
                if index + 1 >= len(inner) or inner[index + 1] != "'":
                    return None
                result.append("'")
                index += 2
            else:
                result.append(inner[index])
                index += 1
        return "".join(result)
    if value.startswith(("[", "{", "&", "*", "!", "|", ">", "%", "`")):
        return None
    if (
        value.endswith(("]", "}"))
        or re.search(r":(?:\s|$)", value)
        or IMPLICIT_NON_STRING_RE.fullmatch(value)
        or not PLAIN_STRING_RE.fullmatch(value)
    ):
        return None
    return value


def parse_inline_list(raw: str) -> list[str] | None:
    value = strip_yaml_comment(raw.strip())
    if value is None or not value.startswith("[") or not value.endswith("]"):
        return None
    inner = value[1:-1].strip()
    if not inner:
        return []
    items = []
    start = 0
    quote = None
    escaped = False
    index = 0
    while index < len(inner):
        char = inner[index]
        if escaped:
            escaped = False
        elif quote == '"' and char == "\\":
            escaped = True
        elif quote:
            if (
                char == quote
                and quote == "'"
                and index + 1 < len(inner)
                and inner[index + 1] == "'"
            ):
                index += 1
            elif char == quote:
                quote = None
        elif char in "'\"" and not inner[start:index].strip():
            quote = char
        elif char == ",":
            parsed = parse_yaml_scalar(inner[start:index])
            if parsed is None:
                return None
            items.append(parsed)
            start = index + 1
        elif char in "[]{}":
            return None
        index += 1
    if quote or escaped:
        return None
    tail = inner[start:].strip()
    if not tail:
        return items or None
    parsed = parse_yaml_scalar(tail)
    if parsed is None:
        return None
    items.append(parsed)
    return items


def parse_simple_frontmatter(raw: str) -> dict[str, object] | None:
    data: dict[str, object] = {}
    pending_key = None
    pending_items: list[str] = []
    pending_indent = None
    flow_key = None
    flow_parts: list[str] = []
    for line in raw.splitlines():
        indentation = line[: len(line) - len(line.lstrip(" \t"))]
        if line.strip() and "\t" in indentation:
            return None
        if flow_key is not None:
            if not line.strip() or line.lstrip().startswith("#"):
                continue
            if not line.startswith(" "):
                return None
            fragment = strip_yaml_comment(line.strip())
            if fragment is None:
                return None
            if fragment == "]":
                parsed = parse_inline_list("[" + " ".join(flow_parts) + "]")
                if parsed is None:
                    return None
                data[flow_key] = parsed
                flow_key = None
                flow_parts = []
            elif fragment:
                flow_parts.append(fragment)
            continue
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if line.startswith(" "):
            fragment = strip_yaml_comment(line.strip())
            if pending_key is not None and not pending_items and fragment == "[":
                flow_key = pending_key
                pending_key = None
                pending_indent = None
                continue
            item = re.fullmatch(r"( +)-\s+(.+?)\s*", line)
            if pending_key is None or item is None:
                return None
            indent = len(item.group(1))
            if pending_indent is not None and indent != pending_indent:
                return None
            parsed = parse_yaml_scalar(item.group(2))
            if parsed is None:
                return None
            pending_indent = indent
            pending_items.append(parsed)
            continue
        if pending_key is not None:
            data[pending_key] = pending_items if pending_items else ""
            pending_key = None
            pending_items = []
            pending_indent = None
        if ":" not in line:
            return None
        key, value = line.split(":", 1)
        key = key.strip()
        if not FRONTMATTER_KEY_RE.fullmatch(key) or key in data:
            return None
        value = strip_yaml_comment(value.strip())
        if value is None:
            return None
        if not value:
            pending_key = key
        elif value.startswith("["):
            if strip_yaml_comment(value) == "[":
                flow_key = key
            else:
                parsed = parse_inline_list(value)
                if parsed is None:
                    return None
                data[key] = parsed
        else:
            parsed = parse_yaml_scalar(value)
            if parsed is None:
                return None
            data[key] = parsed
    if flow_key is not None:
        return None
    if pending_key is not None:
        data[pending_key] = pending_items if pending_items else ""
    return data


def strip_frontmatter(text: str) -> tuple[str, dict[str, object] | None]:
    match = FRONTMATTER_RE.match(text)
    if not match:
        return text, None
    return text[match.end() :], parse_simple_frontmatter(match.group(1))


def permanent_markdown(canon: Path) -> tuple[list[Path], list[str]]:
    files = []
    unsafe = []
    for directory, directories, filenames in os.walk(canon, followlinks=False):
        current = Path(directory)
        if current == canon:
            scratch = canon / "scratch"
            if scratch.is_symlink():
                unsafe.append("scratch")
            directories[:] = [name for name in directories if name != "scratch"]
        symlinked_directories = [
            name for name in directories if (current / name).is_symlink()
        ]
        unsafe.extend(
            (current / name).relative_to(canon).as_posix()
            for name in symlinked_directories
        )
        directories[:] = sorted(
            name for name in directories if name not in symlinked_directories
        )
        for name in sorted(filenames):
            if not name.endswith(".md"):
                continue
            path = current / name
            relative = path.relative_to(canon).as_posix()
            if path.is_symlink() or not path.is_file():
                unsafe.append(relative)
            else:
                files.append(path)
    return sorted(files), sorted(unsafe)


def normalize_route(target: str) -> str | None:
    # Mirrors tools/canonlib.py resolve_canon_reference for a link written in
    # manifest.md at the Canon root, so this inventory and the doctor agree.
    raw = unquote(target.strip().strip("<>"))
    if not raw or "\\" in raw or "\x00" in raw:
        return None
    parsed = urlsplit(raw)
    if parsed.scheme or parsed.netloc or raw.startswith(("/", "~")):
        return None
    if not parsed.path or not parsed.path.endswith(".md"):
        return None
    candidate = parsed.path.removeprefix("canon/")
    route = posixpath.normpath(candidate).removeprefix("./")
    if (
        not route
        or route in (".", "..")
        or route.startswith(("../", "/"))
        or not route.endswith(".md")
    ):
        return None
    return route


def visible_manifest_lines(text: str) -> list[str]:
    # Mirrors tools/canonlib.py visible_markdown_text: fence state takes
    # priority over comment state, and comments are tracked across lines.
    lines = []
    in_fence = False
    fence_marker = ""
    in_comment = False
    for line in text.splitlines():
        if in_comment:
            end = line.find("-->")
            if end == -1:
                continue
            in_comment = False
            line = line[end + 3 :]
        fence = re.match(r"(`{3,}|~{3,})", line.lstrip())
        if fence:
            marker = fence.group(1)
            if not in_fence:
                in_fence = True
                fence_marker = marker
            elif marker[0] == fence_marker[0] and len(marker) >= len(fence_marker):
                in_fence = False
                fence_marker = ""
            continue
        if in_fence:
            continue
        segments: list[str] = []
        rest = line
        while True:
            start = rest.find("<!--")
            if start == -1:
                segments.append(rest)
                break
            segments.append(rest[:start])
            # Search from start + 2 so the empty forms <!--> and <!--->
            # close immediately instead of swallowing the rest of the page.
            end = rest.find("-->", start + 2)
            if end == -1:
                in_comment = True
                break
            rest = rest[end + 3 :]
        lines.append("".join(segments))
    return lines


def markdown_routes(text: str) -> set[str]:
    routes = set()
    for line in visible_manifest_lines(text):
        for match in MANIFEST_LINK_RE.finditer(line):
            route = normalize_route(match.group("angle") or match.group("plain"))
            if route:
                routes.add(route)
    return routes


def manifest_routes(canon: Path, text: str) -> set[str]:
    routes = set()
    visible = "\n".join(visible_manifest_lines(text))
    for line in strip_inline_code(visible).splitlines():
        matches = [
            match
            for match in MANIFEST_LINK_RE.finditer(line)
            if is_real_link(line, match.start())
        ]
        if len(matches) != 1:
            continue
        match = matches[0]
        remainder = line[: match.start()] + line[match.end() :]
        if not re.search(r"(?i)\bread\s+(?:when|for)\s+[A-Za-z0-9]", remainder):
            continue
        route = normalize_route(match.group("angle") or match.group("plain"))
        if route:
            routes.add(route)
    return routes


def paragraphs(body: str) -> set[str]:
    result = set()
    for paragraph in re.split(r"\n\s*\n", body):
        normalized = re.sub(r"[`*_>#|\[\]()]", " ", paragraph.lower())
        normalized = re.sub(r"\s+", " ", normalized).strip()
        if len(normalized) >= 90:
            result.add(normalized)
    return result


def token_counts(body: str) -> Counter[str]:
    return Counter(
        word for word in WORD_RE.findall(body.lower()) if word not in STOP_WORDS
    )


def cosine(left: Counter[str], right: Counter[str]) -> float:
    common = left.keys() & right.keys()
    numerator = sum(left[word] * right[word] for word in common)
    left_norm = math.sqrt(sum(value * value for value in left.values()))
    right_norm = math.sqrt(sum(value * value for value in right.values()))
    return numerator / (left_norm * right_norm) if left_norm and right_norm else 0.0


def analyze(root: Path) -> dict[str, object]:
    root = root.resolve()
    canon = root / "canon"
    if canon.is_symlink() or not canon.is_dir():
        raise SystemExit(f"missing or unsafe Canon directory: {canon}")
    files, unsafe = permanent_markdown(canon)
    relative = {path.relative_to(canon).as_posix(): path for path in files}
    bodies: dict[str, str] = {}
    metadata: dict[str, dict[str, object] | None] = {}
    records = []
    decision_hashes = {}
    paragraph_owners: defaultdict[str, list[str]] = defaultdict(list)
    vectors: dict[str, Counter[str]] = {}
    for rel, path in relative.items():
        text = path.read_text(errors="replace")
        if rel.startswith("decisions/"):
            decision_hashes[rel] = hashlib.sha256(path.read_bytes()).hexdigest()
        body, frontmatter = strip_frontmatter(text)
        bodies[rel], metadata[rel] = body, frontmatter
        for paragraph in paragraphs(body):
            paragraph_owners[paragraph].append(rel)
        vectors[rel] = token_counts(body)
        lines = text.splitlines()
        source_references = sorted(set(SOURCE_REFERENCE_RE.findall(body)))
        records.append(
            {
                "path": rel,
                "lines": len(lines),
                "bytes": len(text.encode()),
                "compact_bytes": len(re.sub(r"\s+", " ", text).strip().encode()),
                "max_line_length": max((len(line) for line in lines), default=0),
                "canon_links": len(markdown_routes(body) - {rel}),
                "words": len(WORD_RE.findall(body.lower())),
                "status": (
                    frontmatter.get("status", "missing")
                    if frontmatter is not None
                    else "missing"
                ),
                "legacy_fields": sorted(
                    set(frontmatter or {}) & LEGACY_FIELDS
                ),
                "implementation_references": len(source_references),
            }
        )

    manifest = bodies.get("manifest.md", "")
    routes = manifest_routes(canon, manifest)
    normative = {
        rel
        for rel, frontmatter in metadata.items()
        if rel != "manifest.md"
        and (
            (frontmatter or {}).get("status") == "normative"
            or (frontmatter is None and rel.startswith("decisions/"))
        )
    }
    repeated = [
        {"files": owners, "text": paragraph[:220]}
        for paragraph, owners in paragraph_owners.items()
        if len(owners) > 1
    ]
    overlap = []
    names = sorted(vectors)
    overlap_pairs_examined = 0
    overlap_truncated = False
    for index, left in enumerate(names):
        for right in names[index + 1 :]:
            if overlap_pairs_examined >= MAX_OVERLAP_PAIRS:
                overlap_truncated = True
                break
            overlap_pairs_examined += 1
            score = cosine(vectors[left], vectors[right])
            if score >= 0.45:
                overlap.append(
                    {"left": left, "right": right, "score": round(score, 3)}
                )
        if overlap_truncated:
            break
    overlap.sort(
        key=lambda item: (
            -float(item["score"]),
            str(item["left"]),
            str(item["right"]),
        )
    )
    status_counts = Counter(str(record["status"]) for record in records)
    cap_violations = [
        record
        for record in records
        if int(record["lines"]) > MAX_LINES or int(record["bytes"]) > MAX_BYTES
    ]
    inventory_candidates = [
        {
            "path": record["path"],
            "implementation_references": record["implementation_references"],
            "legacy_fields": record["legacy_fields"],
        }
        for record in records
        if int(record["implementation_references"]) >= 5
        or record["legacy_fields"]
    ]
    return {
        "root": str(root),
        "canon": str(canon),
        "summary": {
            "files": len(records),
            "lines": sum(int(record["lines"]) for record in records),
            "bytes": sum(int(record["bytes"]) for record in records),
            "words": sum(int(record["words"]) for record in records),
            "routes": len(routes),
            "normative_pages": len(normative),
            "cap_violations": len(cap_violations),
            "status": dict(sorted(status_counts.items())),
            "inventory_candidates": len(inventory_candidates),
            "overlap_pairs_examined": overlap_pairs_examined,
            "overlap_truncated": overlap_truncated,
        },
        "missing_normative_routes": sorted(normative - routes),
        "dead_routes": sorted(routes - set(relative)),
        "unsafe_paths": unsafe,
        "cap_violations": cap_violations,
        "inventory_candidates": inventory_candidates,
        "decision_hashes": decision_hashes,
        "largest_files": sorted(
            records,
            key=lambda item: (-int(item["bytes"]), str(item["path"])),
        )[:15],
        "repeated_paragraphs": repeated[:20],
        "overlap_candidates": overlap[:20],
        "files": records,
    }


def print_text(report: dict[str, object]) -> None:
    summary = report["summary"]
    assert isinstance(summary, dict)
    print(f"Canon: {report['canon']}")
    print(
        "Permanent: "
        f"{summary['files']} files, {summary['lines']} lines, "
        f"{summary['bytes']} bytes, {summary['words']} words"
    )
    print(
        f"Routes: {summary['routes']} | normative: "
        f"{summary['normative_pages']} | status: {summary['status']}"
    )
    print(
        f"Size-cap violations: {summary['cap_violations']} | "
        f"inventory candidates: {summary['inventory_candidates']}"
    )
    print(
        f"Overlap pairs examined: {summary['overlap_pairs_examined']} | "
        f"truncated: {summary['overlap_truncated']}"
    )
    for key in ("missing_normative_routes", "dead_routes", "unsafe_paths"):
        values = report[key]
        assert isinstance(values, list)
        label = key.replace("_", " ").title()
        print(f"{label}: {', '.join(map(str, values)) if values else 'none'}")
    print("Largest files:")
    for item in report["largest_files"]:
        print(
            f"  {item['bytes']:>7} B  {item['lines']:>4} lines  "
            f"max-line={item['max_line_length']:<5} "
            f"compact={item['compact_bytes']:>7} B  "
            f"{item['path']}  [{item['status']}]"
        )
    print("Repeated paragraphs:", len(report["repeated_paragraphs"]))
    print("Overlap candidates:")
    for item in report["overlap_candidates"]:
        print(f"  {item['score']:.3f}  {item['left']} <> {item['right']}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root",
        type=Path,
        default=Path.cwd(),
        help="repository root",
    )
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    report = analyze(args.root)
    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        print_text(report)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
