#!/usr/bin/env python3
"""Record build inputs and binary identity immediately after a locked build.

Run from the checkout that produced the binary. This records provenance; it
cannot establish that an arbitrary supplied binary was built from those inputs.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess


def command(*args):
    return subprocess.check_output(args, text=True).strip()


def sha(path):
    h = hashlib.sha256()
    with open(path, 'rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', type=Path, nargs='?')
    parser.add_argument('--source-only', action='store_true', help='print the input digest for image build labels')
    parser.add_argument('--profile', default='release')
    parser.add_argument('--features', default='')
    parser.add_argument('--build-command', help='exact command used, including workspace and benchmark target selection')
    args = parser.parse_args()
    root = Path(command('git', 'rev-parse', '--show-toplevel'))
    paths = subprocess.check_output(
        ['git', '-C', str(root), 'ls-files', '-z', '--cached', '--others', '--exclude-standard']
    ).decode().split('\0')
    inputs = {}
    for name in sorted(set(paths)):
        path = root / name
        if name and path.is_file() and (
            path.suffix == '.rs' or path.name in ('Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml')
            or name.startswith(('.cargo/', 'src/'))
        ):
            inputs[name] = sha(path)
    source = hashlib.sha256(json.dumps(inputs, sort_keys=True).encode()).hexdigest()
    if args.source_only:
        print(source)
        return
    if args.binary is None:
        parser.error('binary is required unless --source-only is selected')
    manifest = {
        'commit': command('git', 'rev-parse', 'HEAD'),
        'status': command('git', 'status', '--porcelain'),
        'build_inputs_sha256': source,
        'inputs': inputs,
        'binary_sha256': sha(args.binary),
        'lockfile_sha256': sha(root / 'Cargo.lock'),
        'rustc': command('rustc', '-Vv'),
        'profile': args.profile,
        'features': args.features,
        'build_command': args.build_command,
        'rustflags': os.environ.get('RUSTFLAGS', ''),
        'platform': platform.platform(),
        'cpu_affinity': sorted(os.sched_getaffinity(0)) if hasattr(os, 'sched_getaffinity') else None,
        'memory': Path('/proc/meminfo').read_text().splitlines()[:3] if Path('/proc/meminfo').exists() else None,
    }
    dest = Path(str(args.binary) + '.build.json')
    dest.write_text(json.dumps(manifest, indent=2) + '\n')
    print(f'{dest}: {source}')


if __name__ == '__main__':
    main()
