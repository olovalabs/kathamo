"""Exercise channel retries against a fake gh CLI; never writes to GitHub."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

FAKE_GH = r'''#!/usr/bin/env python3
import base64, json, os, sys
from pathlib import Path
args = sys.argv[1:]
with open(os.environ['FAKE_GH_LOG'], 'a') as output:
    output.write(json.dumps(args) + '\n')
mode = os.environ['FAKE_GH_MODE']
if args[:2] == ['api', 'user']:
    print('test-user')
elif args[0] == 'api':
    path = next(a for a in args if a.startswith('repos/'))
    if '/releases/tags/' in path:
        query = args[args.index('--jq') + 1]
        print('2026-10-17' if 'published_at' in query else '1' * 64)
    elif '--method' in args:
        print('{}')
    elif path.startswith('repos/microsoft/winget-pkgs/contents/'):
        sys.exit(0 if mode == 'merged' else 1)
    elif '/git/ref/heads/' in path:
        sys.exit(1 if mode == 'new' else 0)
    elif path.endswith('/commits/master'):
        print('a' * 40)
    elif path.endswith('/winget-pkgs'):
        print('{}')
    elif '/contents/' in path:
        if mode == 'new':
            sys.exit(1)
        if 'homebrew-tap' in path:
            data = b'version "9.0.0"\n'
        elif 'scoop-bucket' in path:
            data = Path('packaging/scoop/rpi.json').read_bytes()
        else:
            data = b'old manifest'
        print(json.dumps({'sha': 'existing-file-sha', 'content': base64.b64encode(data).decode()}))
    else:
        raise AssertionError(path)
elif args[:2] == ['pr', 'list']:
    if mode in ('new', 'merged'):
        print('[]')
    else:
        print(json.dumps([{'url': 'https://github.com/microsoft/winget-pkgs/pull/123',
                           'headRepositoryOwner': {'login': 'another-user' if mode == 'foreign' else 'test-user'},
                           'state': 'CLOSED' if mode == 'closed' else 'OPEN'}]))
elif args[:2] == ['pr', 'create']:
    print('https://github.com/microsoft/winget-pkgs/pull/456')
elif args[:2] == ['repo', 'sync']:
    pass
else:
    raise AssertionError(args)
'''


@unittest.skipUnless(shutil.which("bash"), "requires bash")
class ChannelScriptTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.log = self.directory / "gh.jsonl"
        fake = self.directory / "gh"
        fake.write_text(FAKE_GH, encoding="utf-8", newline="\n")
        fake.chmod(0o755)
        self.root = Path(__file__).resolve().parent.parent

    def execute(self, script, mode, *args, cwd=None):
        env = dict(os.environ, FAKE_GH_LOG=str(self.log), FAKE_GH_MODE=mode,
                   PATH=str(self.directory) + os.pathsep + os.environ["PATH"])
        result = subprocess.run(["bash", str(self.root / "scripts" / script), *args],
                                cwd=cwd or self.root, env=env, text=True, encoding="utf-8", capture_output=True)
        calls = [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []
        return result, calls

    def version(self):
        return json.loads((self.root / "packaging/scoop/rpi.json").read_text())["version"]

    def test_winget_retry_updates_existing_files_with_sha_without_duplicate_pr(self):
        result, calls = self.execute("winget-submit.sh", "existing", self.version())
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        uploads = [call for call in calls if "PUT" in call]
        self.assertEqual(len(uploads), 3)
        self.assertTrue(all("sha=existing-file-sha" in call for call in uploads))
        self.assertFalse(any(call[:2] == ["pr", "create"] for call in calls))
        lookup = next(call for call in calls if call[:2] == ["pr", "list"])
        self.assertEqual(lookup[lookup.index("--head") + 1], "rpi-" + self.version())
        self.assertIn("Updated existing PR", result.stdout)

    def test_new_winget_submission_syncs_fork_before_creating_branch(self):
        result, calls = self.execute("winget-submit.sh", "new", self.version())
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        sync = next(i for i, call in enumerate(calls) if call[:2] == ["repo", "sync"])
        branch = next(i for i, call in enumerate(calls) if "POST" in call)
        self.assertLess(sync, branch)
        self.assertEqual(sum(call[:2] == ["pr", "create"] for call in calls), 1)

    def test_another_forks_same_named_branch_is_not_reused(self):
        result, calls = self.execute("winget-submit.sh", "foreign", self.version())
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(sum(call[:2] == ["pr", "create"] for call in calls), 1)

    def test_merged_submission_is_skipped_and_closed_submission_requires_inspection(self):
        for mode, expected in (("merged", 0), ("closed", 1)):
            self.log.unlink(missing_ok=True)
            result, calls = self.execute("winget-submit.sh", mode, self.version())
            self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
            self.assertFalse(any("PUT" in call or "POST" in call for call in calls))

    def test_refresh_updates_stale_bilingual_cards_and_core_packages_idempotently(self):
        workspace = self.directory / "workspace"
        subprocess.run(["git", "init", "--quiet", str(workspace)], check=True, capture_output=True)
        shutil.copytree(self.root / "packaging", workspace / "packaging")
        data = workspace / "website" / "data"
        shutil.copytree(self.root / "website" / "data", data)
        packages = json.loads((data / "packages.json").read_text(encoding="utf-8"))
        extensions = [package for package in packages if package["type"] != "crate"]
        original_core_names = {package["name"] for package in packages if package["type"] == "crate"}
        for name, version in (("site", "0.3.0"), ("locales", "0.4.0")):
            path = data / f"{name}.json"
            document = json.loads(path.read_text(encoding="utf-8"))
            releases = document["releases"] if name == "site" else document["en"]["site"]["releases"]
            next(card for card in releases if card["status"] == "current")["version"] = version
            path.write_text(json.dumps(document), encoding="utf-8")
        for package in packages:
            if package["type"] == "crate":
                package["version"] = "0.2.0"
        (data / "packages.json").write_text(json.dumps(packages), encoding="utf-8")

        result, _ = self.execute("refresh-channels.sh", "release", "9.8.7", cwd=workspace)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        packages = json.loads((data / "packages.json").read_text(encoding="utf-8"))
        core = [package for package in packages if package["type"] == "crate"]
        self.assertEqual(len(core), len(original_core_names))
        self.assertEqual({package["name"] for package in core}, original_core_names)
        self.assertTrue(all(package["version"] == "9.8.7" and package["ageLabel"] == "v9.8.7"
                            and package["publishedAt"] == "2026-10-17" for package in core))
        self.assertEqual([package for package in packages if package["type"] != "crate"], extensions)
        for name in ("site", "locales"):
            document = json.loads((data / f"{name}.json").read_text(encoding="utf-8"))
            releases = document["releases"] if name == "site" else document["en"]["site"]["releases"]
            self.assertEqual(next(card for card in releases if card["status"] == "current")["version"], "9.8.7")
            self.assertEqual(releases[1]["version"], "next")
        files = [path for folder in (workspace / "packaging", data) for path in folder.rglob("*") if path.is_file()]
        before = {path: path.read_bytes() for path in files}
        result, _ = self.execute("refresh-channels.sh", "release", "9.8.7", cwd=workspace)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(before, {path: path.read_bytes() for path in files})

    def test_homebrew_cannot_be_downgraded(self):
        result, calls = self.execute("push-taps.sh", "newer", self.version(), "homebrew")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("refusing to downgrade", result.stderr)
        self.assertFalse(any("PUT" in call for call in calls))

    def test_unchanged_scoop_manifest_is_a_noop(self):
        result, calls = self.execute("push-taps.sh", "existing", self.version(), "scoop")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse(any("PUT" in call for call in calls))


if __name__ == "__main__":
    unittest.main()
