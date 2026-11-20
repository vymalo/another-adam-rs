// Needs a container runtime: skipped unless BENCH_TEST_DOCKER=1, a failure under BENCH_TEST_REQUIRE_DOCKER=1.
// BENCH_TEST_WORKSPACE_IMAGE names the base image by digest (name@sha256:...). The runtime is the host's Docker: a
// mount source is a path on this machine.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { HostContainers } from '../lib/containers.mjs';
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
  const containers = new HostContainers();

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
  const context = path.join(tmp(t), 'context');
  prepareOwnContext({ cloneDir: clone, base, outDir: path.join(context, 'repo') });
  fs.copyFileSync(path.join(out, 'environment', 'Dockerfile'), path.join(context, 'Dockerfile'));
  await containers.build(context, tag, path.join(context, 'Dockerfile'));
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

// ---- a musl task image (Alpine): the pinned OpenCode binary starts there with the harness's glibc mounted ----
// BENCH_TEST_MUSL_IMAGE: a locally present image whose C library is musl (a SWE-bench Pro teleport image is one);
// BENCH_TEST_OPENCODE_BIN: the pinned binary; BENCH_TEST_GLIBC_SOURCE: a locally present Debian image the runtime is copied
// from, as `Stack.ensureGlibc` does from the coder image (debian:trixie-slim has every file).
const MUSL = process.env.BENCH_TEST_MUSL_IMAGE;
const OC = process.env.BENCH_TEST_OPENCODE_BIN;
const SRC = process.env.BENCH_TEST_GLIBC_SOURCE;
const muslSkip = MUSL && OC && SRC ? false : 'BENCH_TEST_MUSL_IMAGE, BENCH_TEST_OPENCODE_BIN and BENCH_TEST_GLIBC_SOURCE are not all set';

test('OpenCode starts in a musl image with the harness\'s glibc mounted, and without it the shell says "not found" (exit 127)', { skip: muslSkip }, async (t) => {
  const { execFileSync } = await import('node:child_process');
  const { GLIBC_FILES, GLIBC_LIB_DIR, glibcMounts, lacksLoader } = await import('../lib/glibc.mjs');
  const glibc = fs.mkdtempSync(path.join(os.tmpdir(), 'adam-bench-glibc-'));
  t.after(() => fs.rmSync(glibc, { recursive: true, force: true }));
  const containers = new HostContainers();
  await containers.extract(SRC, GLIBC_FILES.map((f) => `${GLIBC_LIB_DIR}/${f}`), glibc, { follow: true });
  assert.equal(await lacksLoader({ containers, image: MUSL }), true, 'the image has no /lib64/ld-linux-x86-64.so.2');
  const dockerRun = (mounts) => {
    try {
      return { status: 0, out: execFileSync('docker', ['run', '--rm', '--network=none', '--user', '10001:10001', '-e', 'HOME=/tmp', '-v', `${OC}:/opt/bench/opencode:ro`, ...mounts.flatMap((m) => ['-v', `${m.source}:${m.target}:ro`]), '--entrypoint', 'sh', MUSL, '-c', '/opt/bench/opencode --version 2>&1'], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }) };
    } catch (e) {
      return { status: e.status, out: `${e.stdout}${e.stderr}` };
    }
  };
  const bare = dockerRun([]);
  assert.equal(bare.status, 127, 'the failure of the second pilot, reproduced');
  assert.match(bare.out, /not found/);
  const through = dockerRun(glibcMounts(glibc));
  assert.equal(through.status, 0, through.out);
  assert.match(through.out, /^opencode v\d/);
});
