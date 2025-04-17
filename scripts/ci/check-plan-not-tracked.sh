#!/usr/bin/env bash
# =============================================================================
# plan-guard — fail if any local-only file is tracked by git.
#
# Local-only set: plan/ (development plans, issues, progress reports),
# .clinerules, AGENT.md. These must never be committed, pushed, or shipped
# (see .clinerules and plan/README.md).
# =============================================================================
set -euo pipefail

violations=()

for path in plan .clinerules AGENT.md; do
    if git ls-files --error-unmatch "$path" >/dev/null 2>&1; then
        violations+=("$path")
    fi
done

# Catch any tracked path under plan/ even if the directory itself is not.
while IFS= read -r f; do
    [ -n "$f" ] && violations+=("$f")
done < <(git ls-files plan/ || true)

if [ ${#violations[@]} -eq 0 ]; then
    echo "plan-guard: OK — no local planning files are tracked"
    exit 0
fi

echo "plan-guard: FAIL — local-only files are tracked by git:" >&2
printf '  %s\n' "${violations[@]}" >&2
echo "remove them from the index (git rm --cached) and keep them git-ignored" >&2
exit 1
