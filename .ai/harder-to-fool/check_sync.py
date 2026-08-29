#!/usr/bin/env python3
"""Fail if the AGENTS.md protocol block has drifted from CODE.md.

Usage: python check_sync.py [CODE.md] [AGENTS.md]
Intended home: .ai/harder-to-fool/check_sync.py, run in CI.
"""
import re, sys

code_path = sys.argv[1] if len(sys.argv) > 1 else "CODE.md"
agents_path = sys.argv[2] if len(sys.argv) > 2 else "AGENTS.md"

code = open(code_path, encoding="utf-8").read()
agents = open(agents_path, encoding="utf-8").read()

m = re.search(r"<!-- BEGIN HARDER-TO-FOOL.*?<!-- END HARDER-TO-FOOL -->", agents, re.S)
if not m:
    sys.exit("FAIL: no HARDER-TO-FOOL block markers in " + agents_path)
block = m.group(0)

required = re.findall(r"^\*\*[KM]\d+\.\*\* .+$", code, re.M)          # every kernel rule, verbatim
required += re.findall(r"^\d+\. What .+$", code, re.M)                 # the three Invocation questions
required += [
    "We would substantially revise this conclusion if we observed __________.",
    "The proposed observation must be plausible and discriminating. A reviser that no one expects to observe is not a reviser.",
    "For a material empirical premise, if the first question has no plausible answer, stop and complete the conformance test. For a normative disagreement, state the values, trade-offs, affected parties, and authority instead of inventing an observational test.",
]
decision = re.search(r"A consequential decision conforms only if .+?explicit\.", code, re.S)
if decision:
    required.append(" ".join(decision.group(0).split()))

norm_block = " ".join(block.split())
missing = [r for r in required if " ".join(r.split()) not in norm_block]
if missing:
    print("FAIL: AGENTS.md block has drifted from CODE.md. Missing verbatim:")
    for r in missing:
        print("  - " + (r[:100] + ("…" if len(r) > 100 else "")))
    sys.exit(1)
print(f"OK: {len(required)} canonical lines verified verbatim in the AGENTS.md block.")
