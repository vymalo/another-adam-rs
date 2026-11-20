import assert from 'node:assert/strict';
import { test } from 'node:test';
import { MAX_STDERR_CHARS, PermanentError, summarize, withStderr } from '../lib/errors.mjs';
import { Stack, StackError } from '../lib/stack.mjs';
import { tmp } from './helpers.mjs';

const GIT = "\nfatal: detected dubious ownership in repository at '/work/bench-base/base.git'\nTo add an exception for this directory, call:\n\n\tgit config --global --add safe.directory /work/bench-base/base.git\nfatal: Could not read from remote repository.\n\nPlease make sure you have the correct access rights\nand the repository exists.\n";

test('the summary of a stderr keeps git\'s fatal line, which names the path, and drops empty lines', () => {
  const s = summarize(GIT);
  assert.match(s, /^fatal: detected dubious ownership in repository at '\/work\/bench-base\/base\.git'/);
  assert.match(s, /and the repository exists\.$/);
  assert.ok(!/ \| {2}\| /.test(s) && !s.startsWith(' |'));
  assert.ok(s.length <= 300);
  assert.equal(summarize(''), '');
  assert.equal(summarize('one line'), 'one line');
});

test('an error carries the capped stderr, and a permanent one says a retry cannot help', () => {
  const e = withStderr(new Error('x'), 'a'.repeat(MAX_STDERR_CHARS + 10));
  assert.equal(e.stderr.length, MAX_STDERR_CHARS);
  const p = new PermanentError('y', { stderr: 's' });
  assert.deepEqual([p.retryable, p.stderr], [false, 's']);
});

test('a failed compose call names the git error in its message and keeps the whole stderr, without the stack\'s secrets', async (t) => {
  const stack = new Stack({ composeFile: '/x/compose.yaml', home: tmp(t), meterPort: 1, exec: async () => ({ exitCode: 128, stdout: '', stderr: `${GIT}token=stack-secret-value\n` }), baseEnv: { PATH: '/usr/bin' } });
  stack.secrets.add('stack-secret-value');
  const e = await stack.compose(['run', 'x']).catch((err) => err);
  assert.ok(e instanceof StackError);
  assert.match(e.message, /exit 128\): fatal: detected dubious ownership/);
  assert.match(e.stderr, /Could not read from remote repository/);
  assert.ok(!e.stderr.includes('stack-secret-value'));
});
