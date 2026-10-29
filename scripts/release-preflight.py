#!/usr/bin/env python3
"""Check clean-source and package-version gates without publishing anything."""
import argparse
import json
from pathlib import Path
import subprocess
import tomllib


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--allow-dirty', action='store_true', help='local preparation only; never a release gate')
    args = parser.parse_args()
    root = Path(subprocess.check_output(['git', 'rev-parse', '--show-toplevel'], text=True).strip())
    dirty = subprocess.check_output(['git', '-C', str(root), 'status', '--porcelain'], text=True)
    if dirty and not args.allow_dirty:
        raise SystemExit('release gate refused: source tree is not clean; review and commit the intended changes first')
    manifest = tomllib.loads((root / 'Cargo.toml').read_text())
    version = manifest['package']['version']
    lock = tomllib.loads((root / 'Cargo.lock').read_text())
    locked = next(p['version'] for p in lock['package'] if p['name'] == 'tanod')
    assert locked == version, (locked, version)
    packages = {p: json.loads((root / f'packages/{p}/package.json').read_text()) for p in ('tanod-next', 'tanod-linux-x64')}
    for name, package in packages.items():
        assert package['version'] == version, (name, package['version'], version)
    assert packages['tanod-next']['optionalDependencies']['@tanod/linux-x64'] == version
    subprocess.run(['cargo', 'metadata', '--locked', '--format-version', '1'], cwd=root, stdout=subprocess.DEVNULL, check=True)
    print(json.dumps(dict(version=version, source_clean=not bool(dirty), release_gate=not args.allow_dirty)))


if __name__ == '__main__':
    main()
