"""Tests for scripts/check-repository-identity.py.

    python3 -m unittest discover -s scripts/tests -v

Standard library only. Each test builds a small tree that passes the manifest
checks and adds the file under test.
"""
import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent.parent / "check-repository-identity.py"
spec = importlib.util.spec_from_file_location("check_repository_identity", SCRIPT)
identity = importlib.util.module_from_spec(spec)
spec.loader.exec_module(identity)

CANONICAL = "https://github.com/tanod-sh/tanod"


def valid_tree(root):
    (root / "Cargo.toml").write_text(f'[package]\nname = "tanod"\nversion = "0.4.1"\nrepository = "{CANONICAL}"\n')
    (root / "Dockerfile").write_text(f'LABEL org.opencontainers.image.source="{CANONICAL}"\n')
    (root / "SECURITY.md").write_text(f"[report]({CANONICAL}/security/advisories/new)\n")
    (root / "README.md").write_text(f"[![CI]({CANONICAL}/actions/workflows/ci.yml/badge.svg)]({CANONICAL}/actions/workflows/ci.yml)\n")
    for name, homepage in (("tanod-next", f"{CANONICAL}/tree/main/packages/tanod-next#readme"),
                           ("tanod-linux-x64", f"{CANONICAL}#readme")):
        (root / "packages" / name).mkdir(parents=True)
        (root / "packages" / name / "package.json").write_text(json.dumps({
            "name": f"@tanod/{name}",
            "homepage": homepage,
            "repository": {"type": "git", "url": f"git+{CANONICAL}.git", "directory": f"packages/{name}"},
            "bugs": {"url": f"{CANONICAL}/issues"},
        }))


class Tree(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        valid_tree(self.root)

    def tearDown(self):
        self.tmp.cleanup()

    def write(self, rel, text):
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def findings(self):
        return identity.check(self.root)[1]


class Manifests(Tree):
    def test_valid_tree_passes(self):
        self.assertEqual(self.findings(), [])

    def test_legacy_npm_repository_is_reported(self):
        manifest = self.root / "packages/tanod-next/package.json"
        data = json.loads(manifest.read_text())
        data["repository"]["url"] = "git+https://github.com/akosidencio/tanod.git"
        manifest.write_text(json.dumps(data, indent=2))
        text = "\n".join(self.findings())
        self.assertIn("packages/tanod-next/package.json\nWrong repository.url", text)
        self.assertIn(f"Expected:\ngit+{CANONICAL}.git", text)

    def test_missing_cargo_repository_is_reported(self):
        self.write("Cargo.toml", '[package]\nname = "tanod"\n')
        self.assertIn("Cargo.toml\nWrong package.repository:\n(missing)", "\n".join(self.findings()))

    def test_missing_ci_badge_is_reported(self):
        self.write("README.md", "# Tanod\n")
        self.assertTrue(any("CI badge" in f for f in self.findings()))


class RepositoryReferences(Tree):
    def test_readme_link_reports_line_and_expected_url(self):
        self.write("README.md", f"[![CI]({CANONICAL}/actions/workflows/ci.yml/badge.svg)](x)\n"
                   "See [issues](https://github.com/akosidencio/tanod/issues).\n")
        self.assertEqual(self.findings(), [
            "README.md:2\nOutdated repository URL:\nhttps://github.com/akosidencio/tanod/issues\n\n"
            "Expected:\nhttps://github.com/tanod-sh/tanod/issues\n"])

    def test_every_spelling_is_caught(self):
        self.write("docs/GUIDE.md", "git clone git@github.com:akosidencio/tanod.git\n"
                   "gh release list -R akosidencio/tanod\n"
                   '"url": "git+https://github.com/akosidencio/tanod.git"\n')
        self.assertEqual(len(self.findings()), 3)

    def test_a_record_of_an_old_release_may_keep_old_links(self):
        self.write("docs/RELEASE-NOTES-0.4.0.md", "Built at https://github.com/akosidencio/tanod/actions/runs/1\n")
        self.write("docs/reviews/CACHE-SECURITY-REVIEW-v0.4.0.md", "https://github.com/akosidencio/tanod/pull/16\n")
        self.write("docs/demo/results/run/env.txt", "remote https://github.com/akosidencio/tanod\n")
        self.assertEqual(self.findings(), [])

    def test_a_later_release_record_may_not(self):
        self.write("docs/RELEASE-NOTES-0.4.1.md", "https://github.com/akosidencio/tanod/issues\n")
        self.assertEqual(len(self.findings()), 1)

    def test_a_migration_note_may_name_the_old_repository(self):
        self.write("CHANGELOG.md", "The repository moved from `akosidencio/tanod` to `tanod-sh/tanod`.\n")
        self.assertEqual(self.findings(), [])

    def test_a_migration_note_may_not_link_the_old_repository(self):
        self.write("CHANGELOG.md", "Moved from https://github.com/akosidencio/tanod to tanod-sh/tanod.\n")
        self.assertEqual(len(self.findings()), 1)

    def test_other_names_are_not_reported(self):
        self.write("docs/GUIDE.md", "github.com/akosidencio/tanodx, https://github.com/akosidencio/tanod-fork "
                   "and github.com/someone/akosidencio/tanod\n")
        self.assertEqual(self.findings(), [])


class ImageReferences(Tree):
    def test_qualified_legacy_location_is_history(self):
        self.write("README.md", f"[![CI]({CANONICAL}/actions/workflows/ci.yml/badge.svg)](x)\n"
                   "Images for 0.2.0 through 0.4.0 remain at `ghcr.io/akosidencio/tanod`.\n")
        self.write("docs/UPGRADING.md", "Pin the digest; 0.4.0 itself was published as\n"
                   "`ghcr.io/akosidencio/tanod`) or verify the archive.\n")
        self.write("docs/DEPLOY.md", "docker pull ghcr.io/akosidencio/tanod:0.3.2\n"
                   "COPY --from=ghcr.io/akosidencio/tanod@sha256:abc /tanod /tanod  # 0.4.0\n")
        self.assertEqual(self.findings(), [])

    def test_unqualified_legacy_location_is_reported(self):
        self.write("docs/DEPLOY.md", "Pull the image from ghcr.io/akosidencio/tanod.\n")
        found = self.findings()
        self.assertEqual(len(found), 1)
        self.assertTrue(found[0].startswith("docs/DEPLOY.md:1\nReference to the pre-0.4.1 image location"))

    def test_qualifier_two_lines_up_does_not_count(self):
        self.write("docs/DEPLOY.md", "0.4.0 is old.\nUnrelated line.\nUse ghcr.io/akosidencio/tanod\n")
        self.assertEqual(len(self.findings()), 1)

    def test_later_version_at_old_location_is_refused_even_when_qualified(self):
        self.write("docs/DEPLOY.md", "0.4.0 and earlier: ghcr.io/akosidencio/tanod:0.4.1\n")
        self.assertEqual(self.findings(), [
            "docs/DEPLOY.md:1\n0.4.1 was never published at the old image location:\n"
            "ghcr.io/akosidencio/tanod:0.4.1\n\nExpected:\nghcr.io/tanod-sh/tanod:0.4.1\n"])

    def test_moving_tag_at_old_location_is_refused(self):
        self.write("docs/DEPLOY.md", "rollback for 0.4.0: ghcr.io/akosidencio/tanod:latest\n")
        self.assertIn("moving tag 'latest'", self.findings()[0])

    def test_old_version_at_new_location_is_refused(self):
        self.write("docs/DEPLOY.md", "docker pull ghcr.io/tanod-sh/tanod:0.4.0\n")
        self.assertEqual(self.findings(), [
            "docs/DEPLOY.md:1\n0.4.0 was never published at the organization's image location:\n"
            "ghcr.io/tanod-sh/tanod:0.4.0\n\nExpected:\nghcr.io/akosidencio/tanod:0.4.0\n"])

    def test_new_location_for_new_versions_passes(self):
        self.write("docs/DEPLOY.md", "docker pull ghcr.io/tanod-sh/tanod:0.4.1\n"
                   "image: ghcr.io/tanod-sh/tanod@sha256:abc\nghcr.io/tanod-sh/tanod:latest\n")
        self.assertEqual(self.findings(), [])

    def test_old_image_is_refused_even_in_historical_records_when_wrong(self):
        self.write("docs/RELEASE-NOTES-0.4.0.md", "Published as ghcr.io/tanod-sh/tanod:0.4.0\n")
        self.assertEqual(len(self.findings()), 1)


class Command(unittest.TestCase):
    def test_exit_status_and_output(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            valid_tree(root)
            ok = subprocess.run([sys.executable, str(SCRIPT), "--root", str(root)], capture_output=True, text=True)
            self.assertEqual(ok.returncode, 0, ok.stdout + ok.stderr)
            (root / "SECURITY.md").write_text("https://github.com/akosidencio/tanod/security/advisories/new\n")
            bad = subprocess.run([sys.executable, str(SCRIPT), "--root", str(root)], capture_output=True, text=True)
            self.assertEqual(bad.returncode, 1)
            self.assertIn("SECURITY.md:1\nOutdated repository URL:\n"
                          "https://github.com/akosidencio/tanod/security/advisories/new\n", bad.stdout)

    def test_repository_itself_passes(self):
        result = subprocess.run([sys.executable, str(SCRIPT)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
