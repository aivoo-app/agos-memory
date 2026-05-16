#!/usr/bin/env python3
"""Fail if any GitHub Actions workflow is not valid YAML.

Why this exists: GitHub Actions silently *rejects* a workflow file that fails
to parse — the run simply never starts, and because the checks never report,
nothing looks red. A single unquoted `: ` in a step name is enough (for example
`- name: Offline eval gate (hermetic: deterministic hash embedder)`, where YAML
reads the colon as the start of a nested mapping). That disabled every gate in
`.github/workflows/ci.yml` without any visible failure, so it is now checked
explicitly, locally (`make yaml-guard`, part of `make check`) and in CI.

Only YAML *syntax* is validated here; semantic correctness (action versions,
job keys) is GitHub's business.
"""

from __future__ import annotations

import sys
from pathlib import Path

try:
    import yaml
except ImportError:  # pragma: no cover - environment problem, not a repo problem
    sys.exit("yaml-guard: PyYAML is required (pip install pyyaml)")

REPO_ROOT = Path(__file__).resolve().parent.parent
WORKFLOW_DIR = REPO_ROOT / ".github" / "workflows"


def main() -> int:
    if not WORKFLOW_DIR.is_dir():
        print(f"yaml-guard: no {WORKFLOW_DIR} — nothing to check")
        return 0

    files = sorted(p for p in WORKFLOW_DIR.iterdir() if p.suffix in {".yml", ".yaml"})
    if not files:
        print(f"yaml-guard: no workflow files in {WORKFLOW_DIR}")
        return 0

    failures: list[tuple[Path, str]] = []
    for path in files:
        try:
            doc = yaml.safe_load(path.read_text(encoding="utf-8"))
        except yaml.YAMLError as exc:
            failures.append((path, str(exc)))
            continue
        # A workflow that parses to a non-mapping (or to nothing) is also broken.
        if not isinstance(doc, dict):
            failures.append((path, f"expected a mapping at the top level, got {type(doc).__name__}"))
            continue
        jobs = doc.get("jobs")
        if not isinstance(jobs, dict) or not jobs:
            failures.append((path, "no `jobs:` mapping found"))
            continue
        for name, job in jobs.items():
            if not isinstance(job, dict) or "steps" not in job:
                failures.append((path, f"job `{name}` has no `steps:` list"))
        print(f"yaml-guard: {path.relative_to(REPO_ROOT)} OK ({len(jobs)} job(s): {', '.join(jobs)})")

    if failures:
        print("\nyaml-guard: FAIL", file=sys.stderr)
        for path, reason in failures:
            print(f"  {path.relative_to(REPO_ROOT)}: {reason}", file=sys.stderr)
        print(
            "\nNote: GitHub silently skips a workflow that does not parse, so this"
            " would disable every CI gate without a red check.",
            file=sys.stderr,
        )
        return 1

    print("yaml-guard: OK — every workflow parses and declares jobs")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
