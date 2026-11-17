// Needs a container service: skipped unless BENCH_TEST_DOCKER=1, a failure under BENCH_TEST_REQUIRE_DOCKER=1.
// BENCH_TEST_WORKSPACE_IMAGE names the base image by digest (name@sha256:...); BENCH_TEST_PODMAN_CONNECTION
// and BENCH_TEST_PODMAN_URL name the Podman service (optional). The service must see this machine's temporary
// directory at the same path (a shared mount).
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { PodmanRemote } from '../lib/containers.mjs';
import { checkTask } from '../lib/envcheck.mjs';
import { gradePatch } from '../lib/grade.mjs';
import { materializeOwn, parseOwnSpec, prepareOwnContext } from '../lib/own.mjs';
import { loadTask } from '../lib/task.mjs';
import { commitFiles, git, initRepo } from './gitfix.mjs';
import { tmp } from './helpers.mjs';

const enabled = process.env.BENCH_TEST_DOCKER === '1';
const required = process.env.BENCH_TEST_REQUIRE_DOCKER === '1';

test('docker: a self-made task, the oracle grades 1 and the empty patch 0', { skip: !enabled && !required ? 'BENCH_TEST_DOCKER is not 1' : false }, async (t) => {
  assert.ok(enabled, 'BENCH_TEST_REQUIRE_DOCKER=1 needs BENCH_TEST_DOCKER=1');
  const workspaceImage = process.env.BENCH_TEST_WORKSPACE_IMAGE;
  assert.match(workspaceImage ?? '', /@sha256:[0-9a-f]{64}$/, 'BENCH_TEST_WORKSPACE_IMAGE must name the workspace image by digest');
  const containers = new PodmanRemote({ connection: process.env.BENCH_TEST_PODMAN_CONNECTION, url: process.env.BENCH_TEST_PODMAN_URL });

  const clone = initRepo(t);
  commitFiles(clone, { 'src/app.txt': 'start\n' }, 'chore: start (#1)');
  const base = git(clone, 'rev-parse', 'HEAD').trim();
  const commit = commitFiles(clone, { 'src/app.txt': 'start\nadded\n', 'tests/marker.txt': 'hidden\n' }, 'feat: add (#2)');
  const specDir = path.join(tmp(t), 'own', 'tiny-2');
  fs.mkdirSync(specDir, { recursive: true });
  fs.writeFileSync(path.join(specDir, 'instruction.md'), 'Append a line "added" to src/app.txt.\n');
  const spec = { ...parseOwnSpec({ id: 'tiny-2', repo: 'tiny', clone: 'tiny', commit, base, testCommand: 'grep -qx added src/app.txt && test -f tests/marker.txt' }, 'tiny-2'), dir: specDir };

  const out = path.join(tmp(t), 'm', 'tiny-2');
  const { tag } = materializeOwn({ spec, cloneDir: clone, outDir: out, workspaceImage });
  prepareOwnContext({ cloneDir: clone, base, outDir: path.join(out, 'environment', 'repo') });
  await containers.build(path.join(out, 'environment'), tag, path.join(out, 'environment', 'Dockerfile'));
  const digest = await containers.digestOf(tag);
  const task = loadTask(out, { pins: { [tag]: digest }, source: 'own' });

  const work = tmp(t);
  const check = await checkTask({ containers, task, workRoot: work });
  assert.deepEqual([check.ok, check.reason], [true, null]);
  const gold = await gradePatch({ containers, task, patchFile: task.goldPatch, workRoot: work });
  assert.deepEqual([gold.pass, gold.passes], [true, 3]);
  const empty = await gradePatch({ containers, task, patchFile: null, workRoot: work });
  assert.deepEqual([empty.pass, empty.passes], [false, 0]);
});
