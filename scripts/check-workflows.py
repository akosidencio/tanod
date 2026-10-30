#!/usr/bin/env python3
"""Structural checks on .github/workflows that a YAML parser alone does not make.

    python3 scripts/check-workflows.py

* Every third-party action and reusable workflow is pinned to a full 40-hex
  commit SHA, with a `# <release>` comment naming what it was resolved from.
  Local `./` workflows are exempt: they come from the same commit.
* Every workflow declares top-level `permissions`.
* `needs.<job>.outputs.<name>` names a job in that job's own `needs` and an
  output that job declares; `steps.<id>.outputs` names a step id in the job.
* No `${{ inputs.* }}` or `${{ github.ref_name }}` is expanded inside a `run:`
  script, where a crafted value would be shell syntax; pass it through `env:`.

This is not actionlint and does not replace it; it is what the release
workflow's correctness depends on, checkable without extra tools. Needs PyYAML.
"""
import re
import sys
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parent.parent
PINNED = re.compile(r"^[\w.-]+/[\w./-]+@[0-9a-f]{40}$")
NEEDS_REF = re.compile(r"needs\.([\w-]+)\.outputs\.([\w-]+)")
STEPS_REF = re.compile(r"steps\.([\w-]+)\.outputs")
UNSAFE_IN_RUN = re.compile(r"\$\{\{[^}]*\b(inputs\.[\w-]+|github\.ref_name|github\.head_ref)\b[^}]*\}\}")


def check(path):
    problems = []
    text = path.read_text()
    doc = yaml.safe_load(text)
    if "permissions" not in doc:
        problems.append("no top-level permissions")
    for number, line in enumerate(text.splitlines(), 1):
        match = re.search(r"uses:\s*([^\s#]+)\s*(#.*)?$", line)
        if not match or match.group(1).startswith("./"):
            continue
        if not PINNED.match(match.group(1)):
            problems.append(f"line {number}: {match.group(1)} is not pinned to a commit SHA")
        elif not match.group(2):
            problems.append(f"line {number}: pinned {match.group(1)} has no '# <release>' comment")

    jobs = doc.get("jobs", {})
    for name, job in jobs.items():
        needs = job.get("needs", [])
        needs = [needs] if isinstance(needs, str) else needs
        body = yaml.safe_dump(job)
        for job_ref, output in NEEDS_REF.findall(body):
            if job_ref not in needs:
                problems.append(f"job {name}: uses needs.{job_ref} but does not need it directly")
            elif output not in (jobs.get(job_ref, {}).get("outputs") or {}):
                problems.append(f"job {name}: needs.{job_ref}.outputs.{output} is not declared")
        step_ids = {step.get("id") for step in job.get("steps", []) if isinstance(step, dict)}
        for step_ref in STEPS_REF.findall(body):
            if step_ref not in step_ids:
                problems.append(f"job {name}: steps.{step_ref} has no step with that id")
        for step in job.get("steps", []):
            run = step.get("run", "") if isinstance(step, dict) else ""
            for match in UNSAFE_IN_RUN.finditer(run):
                problems.append(f"job {name}: '{match.group(0)}' expanded inside a run script")
    problems += check_publication_order(jobs)
    return problems


def writes(job):
    permissions = job.get("permissions") or {}
    return isinstance(permissions, dict) and any(v == "write" for v in permissions.values())


def check_publication_order(jobs):
    """A job that can publish must (transitively) need every job that cannot.

    GitHub skips a job whose `needs` failed, so this is what makes every check
    a gate on every publication rather than a report beside it.
    """
    def ancestors(name, seen=None):
        seen = set() if seen is None else seen
        needs = jobs[name].get("needs", [])
        for need in [needs] if isinstance(needs, str) else needs:
            if need not in seen:
                seen.add(need)
                ancestors(need, seen)
        return seen

    problems = []
    publishers = [name for name, job in jobs.items() if writes(job)]
    checks = [name for name, job in jobs.items() if not writes(job)]
    for publisher in publishers:
        missing = [c for c in checks if c not in ancestors(publisher)]
        if missing:
            problems.append(f"job {publisher} can publish without waiting for: {', '.join(missing)}")
        if jobs[publisher].get("if"):
            problems.append(f"job {publisher} has an `if:`, which can run it past a failed need")
    return problems


def main():
    failed = False
    for path in sorted((ROOT / ".github" / "workflows").glob("*.yml")):
        problems = check(path)
        for problem in problems:
            print(f"{path.relative_to(ROOT)}: {problem}")
        failed |= bool(problems)
        if not problems:
            print(f"{path.relative_to(ROOT)}: ok")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
