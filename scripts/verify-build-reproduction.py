#!/usr/bin/env python3
"""Compare locked release builds from two copies of the current source.

This verifies byte reproduction of a working-tree snapshot. A dirty input is
still dirty source and does not pass scripts/release-preflight.py.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--out', type=Path, default=Path('benchmark-results/reproduction'))
    parser.add_argument('--features', default='tls')
    args = parser.parse_args()
    root = Path(subprocess.check_output(['git', 'rev-parse', '--show-toplevel'], text=True).strip())
    commit = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=root, text=True).strip()
    clean = not bool(subprocess.check_output(['git', 'status', '--porcelain'], cwd=root))
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    names = subprocess.check_output(['git', 'ls-files', '-z', '--cached', '--others', '--exclude-standard'], cwd=root).decode().split('\0')
    inputs = {n: digest(root/n) for n in sorted(set(names)) if n and (root/n).is_file()}
    snapshot_hash = hashlib.sha256(json.dumps(inputs, sort_keys=True).encode()).hexdigest()
    # Capture once before either build. The live checkout may change during
    # compilation; both build directories must still receive identical bytes.
    frozen = out / 'snapshot'
    frozen.mkdir(exist_ok=False)
    for name, expected in inputs.items():
        destination = frozen / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(root/name, destination)
        if digest(destination) != expected:
            raise SystemExit(f'source changed while freezing {name}')
    (out / 'inputs.json').write_text(json.dumps(inputs, indent=2)+'\n')
    if commit != subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=root, text=True).strip():
        raise SystemExit('commit changed while freezing source')
    if clean:
        # Establish the clean-commit claim from frozen bytes, rather than a
        # second observation of a live tree that could have changed and reverted.
        tree = subprocess.check_output(['git', 'ls-tree', '-rz', commit], cwd=root)
        blobs = {}
        for record in tree.split(b'\0'):
            if record:
                metadata, name = record.split(b'\t', 1)
                _mode, kind, object_id = metadata.split()
                if kind != b'blob':
                    raise SystemExit('source snapshot does not support submodules')
                blobs[name.decode()] = object_id.decode()
        if set(blobs) != set(inputs):
            raise SystemExit('frozen file list differs from the clean commit')
        algorithm = subprocess.check_output(
            ['git', 'rev-parse', '--show-object-format'], cwd=root, text=True).strip()
        for name, expected in blobs.items():
            data = (frozen/name).read_bytes()
            actual = hashlib.new(algorithm, b'blob '+str(len(data)).encode()+b'\0'+data).hexdigest()
            if actual != expected:
                raise SystemExit(f'frozen input differs from clean commit: {name}')
    epoch = subprocess.check_output(['git', 'log', '-1', '--format=%ct', commit], cwd=root, text=True).strip()
    hashes = {}
    for side in ('a', 'b'):
        source = out / side
        source.mkdir(exist_ok=False)
        for name in inputs:
            destination = source / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(frozen/name, destination)
        env = os.environ.copy()
        registry = Path(env.get('CARGO_HOME', str(Path.home()/'.cargo'))) / 'registry/src'
        env['RUSTFLAGS'] = f'--remap-path-prefix={source}=/tanod --remap-path-prefix={registry}=/cargo/registry'
        env['SOURCE_DATE_EPOCH'] = epoch
        env['CARGO_TARGET_DIR'] = str(source / 'target')
        command = ['cargo', 'build', '--release', '--locked', '--bin', 'tanod']
        if args.features:
            command += ['--features', args.features]
        with (out / f'build-{side}.log').open('w') as log:
            subprocess.run(command, cwd=source, env=env, stdout=log, stderr=subprocess.STDOUT, check=True)
        binary = source / 'target/release/tanod'
        hashes[side] = digest(binary)
    result = dict(source_commit=commit, snapshot_sha256=snapshot_hash, source_clean=clean,
                  lockfile_sha256=digest(frozen / 'Cargo.lock'),
                  features=args.features, binary_sha256=hashes, reproduced=len(set(hashes.values())) == 1)
    (out / 'result.json').write_text(json.dumps(result, indent=2)+'\n')
    print(json.dumps(result))
    if not result['reproduced']:
        raise SystemExit('release binaries differ across checkout paths')


if __name__ == '__main__':
    main()
