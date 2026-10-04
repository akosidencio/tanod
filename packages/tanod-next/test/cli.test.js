import { test } from 'node:test';
import assert from 'node:assert/strict';
import path from 'node:path';

import { isDirectExecution } from '../src/cli.js';

test('direct CLI execution compares complete paths', () => {
  assert.equal(isDirectExecution('/repo/src/cli.js', '/repo/src/cli.js'), true);
  assert.equal(isDirectExecution('/repo/src/cli.js', '/another/cli.js'), false);
});

test('direct CLI execution resolves an installed bin symlink', () => {
  const canonicalize = (file) => file.replace('/node_modules/.bin/tanod-next', '/src/cli.js');
  assert.equal(
    isDirectExecution(
      '/src/cli.js',
      '/node_modules/.bin/tanod-next',
      path.posix,
      canonicalize,
    ),
    true,
  );
});

test('direct CLI execution supports Windows path separators', () => {
  assert.equal(
    isDirectExecution('C:\\repo\\src\\cli.js', 'c:\\REPO\\src\\cli.js', path.win32),
    true,
  );
  assert.equal(
    isDirectExecution('C:\\repo\\src\\cli.js', 'C:\\another\\cli.js', path.win32),
    false,
  );
});
