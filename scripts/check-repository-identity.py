#!/usr/bin/env python3
"""Fail when an active file names the repository's or the image's old home.

    python3 scripts/check-repository-identity.py [--root DIR]

The repository moved from akosidencio/tanod to tanod-sh/tanod after 0.4.0.
GitHub redirects the old URLs, so a stale link keeps working and nothing else
notices it coming back. Images are different: 0.2.0 through 0.4.0 were
published to ghcr.io/akosidencio/tanod and stay there, so that name is still
correct for those versions and wrong for every later one.

* A repository reference (github.com/akosidencio/tanod, git+https, git@, or
  bare `akosidencio/tanod`) is refused in every tracked text file except the
  records of a release up to 0.4.0 (a file whose name carries that version)
  and benchmark evidence (docs/demo/results/). A bare name that is not a link
  is also accepted on a line naming tanod-sh/tanod: that is a migration note
  ("moved from akosidencio/tanod to tanod-sh/tanod"), not a link to follow.
* ghcr.io/akosidencio/tanod is accepted only where the same line or the one
  before it names a version published there (0.4.0 or earlier), or the
  reference is tagged with one. A later version tag, or a moving tag such as
  `latest`, at the old location is always refused: it never existed, or it
  stopped moving at 0.4.0.
* ghcr.io/tanod-sh/tanod tagged 0.4.0 or earlier is always refused: those
  images were never published there.
* The package manifests, Cargo.toml, the Dockerfile's OCI source label, the
  security reporting link and the README's CI badge name tanod-sh/tanod.

Standard library only. Exits 1 with one block per finding.
"""
import argparse
import json
import re
import subprocess
import sys
import tomllib
from pathlib import Path

CANONICAL = "https://github.com/tanod-sh/tanod"
NEW_IMAGE = "ghcr.io/tanod-sh/tanod"
LEGACY_IMAGE = "ghcr.io/akosidencio/tanod"
LAST_LEGACY_RELEASE = (0, 4, 0)

# This script and its tests spell the old names on purpose.
SELF = {"scripts/check-repository-identity.py", "scripts/tests/test_check_repository_identity.py"}
SKIPPED_PREFIXES = ("vendor/",)
SKIPPED_SUFFIXES = (".lock",)
EVIDENCE_PREFIXES = ("docs/demo/results/",)

IMAGE_REF = re.compile(
    r"ghcr\.io/(?P<owner>akosidencio|tanod-sh)/tanod(?![\w-])(?::(?P<tag>[\w][\w.-]*[\w]|[\w]))?", re.I
)
REPO_REF = re.compile(
    r"(?:(?:git\+)?https?://github\.com/|git@github\.com:|(?<![\w./-]))akosidencio/tanod(?![\w-])[^\s)\]>\"'`]*",
    re.I,
)
VERSION = re.compile(r"(?<![\w.])v?(\d+)\.(\d+)\.(\d+)(?![\w])")


def version_tuple(text):
    match = re.fullmatch(r"v?(\d+)\.(\d+)\.(\d+)", text or "")
    return tuple(int(part) for part in match.groups()) if match else None


def is_historical(path):
    """A record of a release up to 0.4.0, or benchmark evidence."""
    if path.startswith(EVIDENCE_PREFIXES):
        return True
    named = [tuple(int(p) for p in m.groups()) for m in VERSION.finditer(Path(path).name)]
    return bool(named) and all(v <= LAST_LEGACY_RELEASE for v in named)


def names_legacy_version(text):
    return any(tuple(int(p) for p in m.groups()) <= LAST_LEGACY_RELEASE for m in VERSION.finditer(text))


def finding(path, line, problem, found, expected):
    where = f"{path}:{line}" if line else path
    return f"{where}\n{problem}:\n{found}\n\nExpected:\n{expected}\n"


def scan_text(path, text):
    findings = []
    historical = is_historical(path)
    lines = text.splitlines()
    for number, line in enumerate(lines, 1):
        image_spans = []
        for match in IMAGE_REF.finditer(line):
            image_spans.append(match.span())
            owner, tag = match.group("owner").lower(), match.group("tag")
            version = version_tuple(tag)
            ref = match.group(0)
            if owner == "tanod-sh":
                if version and version <= LAST_LEGACY_RELEASE:
                    findings.append(finding(
                        path, number,
                        f"{tag} was never published at the organization's image location",
                        ref, f"{LEGACY_IMAGE}:{tag}"))
                continue
            if tag is None:
                window = line if number == 1 else lines[number - 2] + "\n" + line
                if not historical and not names_legacy_version(window):
                    findings.append(finding(
                        path, number,
                        "Reference to the pre-0.4.1 image location without the versions published there",
                        ref,
                        f"{NEW_IMAGE}, or say on this line or the one before it that "
                        f"{LEGACY_IMAGE} holds 0.4.0 and earlier"))
            elif version and version <= LAST_LEGACY_RELEASE:
                continue
            elif version:
                findings.append(finding(
                    path, number, f"{tag} was never published at the old image location",
                    ref, f"{NEW_IMAGE}:{tag}"))
            else:
                findings.append(finding(
                    path, number,
                    f"The moving tag '{tag}' at the old image location stopped at 0.4.0",
                    ref, f"{NEW_IMAGE}:{tag}"))
        if historical:
            continue
        for match in REPO_REF.finditer(line):
            if any(start <= match.start() < end for start, end in image_spans):
                continue
            found = match.group(0).rstrip(".,;:")
            if "/" not in found.split("akosidencio", 1)[0] and ":" not in found and "tanod-sh/tanod" in line:
                continue
            expected = re.sub("akosidencio/tanod", "tanod-sh/tanod", found, flags=re.I)
            findings.append(finding(path, number, "Outdated repository URL", found, expected))
    return findings


def check_manifests(root):
    findings = []

    def require(path, field, got, want):
        if got != want:
            findings.append(finding(path, None, f"Wrong {field}", got or "(missing)", want))

    cargo = root / "Cargo.toml"
    if cargo.is_file():
        package = tomllib.loads(cargo.read_text()).get("package", {})
        require("Cargo.toml", "package.repository", package.get("repository"), CANONICAL)
    for manifest in sorted(root.glob("packages/*/package.json")):
        rel = manifest.relative_to(root).as_posix()
        data = json.loads(manifest.read_text())
        repository = data.get("repository") or {}
        url = repository.get("url") if isinstance(repository, dict) else repository
        require(rel, "repository.url", url, f"git+{CANONICAL}.git")
        require(rel, "bugs.url", (data.get("bugs") or {}).get("url"), f"{CANONICAL}/issues")
        homepage = data.get("homepage") or ""
        if homepage != CANONICAL and not homepage.startswith(CANONICAL + "/") and not homepage.startswith(CANONICAL + "#"):
            findings.append(finding(rel, None, "Wrong homepage", homepage or "(missing)", f"{CANONICAL} or a page under it"))
    for rel, needle, what in (
        ("Dockerfile", f'org.opencontainers.image.source="{CANONICAL}"', "OCI source label"),
        ("SECURITY.md", f"{CANONICAL}/security/advisories/new", "private vulnerability reporting link"),
        ("README.md", f"{CANONICAL}/actions/workflows/ci.yml/badge.svg", "CI badge"),
    ):
        path = root / rel
        if not path.is_file() or needle not in path.read_text():
            findings.append(finding(rel, None, f"No {what} for the canonical repository", "(missing)", needle))
    return findings


def candidate_files(root):
    try:
        top = subprocess.run(["git", "-C", str(root), "rev-parse", "--show-toplevel"],
                             capture_output=True, text=True, check=True).stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        top = None
    if top and Path(top).resolve() == root.resolve():
        # Untracked files too, so a local run sees a file before it is added.
        listed = subprocess.run(["git", "-C", str(root), "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
                                capture_output=True, text=True, check=True).stdout
        return [name for name in listed.split("\0") if name]
    return sorted(p.relative_to(root).as_posix() for p in root.rglob("*")
                  if p.is_file() and ".git" not in p.relative_to(root).parts)


def check(root):
    findings = []
    checked = 0
    for rel in candidate_files(root):
        if rel in SELF or rel.startswith(SKIPPED_PREFIXES) or rel.endswith(SKIPPED_SUFFIXES):
            continue
        path = root / rel
        if not path.is_file():
            continue
        data = path.read_bytes()
        if b"\0" in data:
            continue
        try:
            text = data.decode("utf-8")
        except UnicodeDecodeError:
            continue
        checked += 1
        findings.extend(scan_text(rel, text))
    return checked, findings + check_manifests(root)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parent.parent)
    args = parser.parse_args()
    checked, findings = check(args.root)
    for item in findings:
        print(item)
    if findings:
        print(f"repository identity: {len(findings)} finding(s) in {checked} files", file=sys.stderr)
        return 1
    print(f"repository identity: {checked} files checked, ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
