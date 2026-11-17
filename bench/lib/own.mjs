// Own tasks: the owner's statement plus commit ids and a test command are in the repository
// (`bench/tasks/own/<id>/`); everything else is built locally, outside it, from a full clone the owner names.
//
//   own.json         { id, repo, clone, commit, base, language, testCommand, toolchain?: { env, run } }
//   instruction.md   written by the owner (see bench/tasks/own/README.md)
//
// `materializeOwn` turns that into the task format of lib/task.mjs under $BENCH_HOME. The hidden tests are
// read from the clone then, never stored in the repository.
import { execFileSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { classifyPath, gitIn } from './mine.mjs';
import { TASK_ID, parseImageRef } from './task.mjs';

export class OwnTaskError extends Error {
  constructor(id, message) {
    super(`own task ${id}: ${message}`);
    this.name = 'OwnTaskError';
  }
}

const FIELDS = new Set(['id', 'repo', 'clone', 'commit', 'base', 'language', 'testCommand', 'toolchain']);
const SHA = /^[0-9a-f]{40}$/;

/** Validate an own.json object. Returns the spec. */
export function parseOwnSpec(raw, dirName) {
  const id = raw?.id ?? dirName ?? '?';
  const bad = (m) => {
    throw new OwnTaskError(id, m);
  };
  if (raw === null || typeof raw !== 'object' || Array.isArray(raw)) bad('own.json must be a JSON object');
  for (const k of Object.keys(raw)) if (!FIELDS.has(k)) bad(`unknown field ${k}`);
  if (typeof raw.id !== 'string' || !TASK_ID.test(raw.id) || raw.id.includes('..')) bad('id is not a safe task id');
  if (dirName !== undefined && raw.id !== dirName) bad('id must equal the directory name');
  for (const k of ['repo', 'clone']) if (typeof raw[k] !== 'string' || !/^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$/.test(raw[k])) bad(`${k} must be a short plain name`);
  for (const k of ['commit', 'base']) if (typeof raw[k] !== 'string' || !SHA.test(raw[k])) bad(`${k} must be a full commit id`);
  if (typeof raw.testCommand !== 'string' || raw.testCommand.trim() === '' || raw.testCommand.length > 1000 || /[\0\r\n]/.test(raw.testCommand)) bad('testCommand must be one non-empty line');
  if (raw.language !== undefined && !/^[a-z]{2,20}$/.test(raw.language)) bad('language must be a lowercase word');
  const toolchain = raw.toolchain ?? {};
  if (typeof toolchain !== 'object' || Array.isArray(toolchain) || Object.keys(toolchain).some((k) => k !== 'env' && k !== 'run')) bad('toolchain holds only env and run');
  const env = toolchain.env ?? {};
  for (const [k, v] of Object.entries(env)) if (!/^[A-Z_][A-Z0-9_]*$/.test(k) || typeof v !== 'string' || /[\r\n\0$"\\]/.test(v)) bad(`toolchain.env.${k} is not a plain value`);
  const run = toolchain.run ?? [];
  if (!Array.isArray(run) || run.some((l) => typeof l !== 'string' || l.trim() === '' || /[\r\n\0]/.test(l))) bad('toolchain.run must be a list of one-line commands');
  return { id: raw.id, repo: raw.repo, clone: raw.clone, commit: raw.commit, base: raw.base, language: raw.language ?? 'unknown', testCommand: raw.testCommand.trim(), toolchain: { env, run } };
}

/** Read every `bench/tasks/own/<id>/own.json`. Returns { specs, errors }. */
export function loadOwnSpecs(dir) {
  const specs = [];
  const errors = [];
  let names = [];
  try {
    names = fs.readdirSync(dir, { withFileTypes: true }).filter((e) => e.isDirectory()).map((e) => e.name).sort();
  } catch {
    return { specs, errors };
  }
  for (const name of names) {
    try {
      const raw = JSON.parse(fs.readFileSync(path.join(dir, name, 'own.json'), 'utf8'));
      const spec = parseOwnSpec(raw, name);
      const statement = path.join(dir, name, 'instruction.md');
      if (!fs.existsSync(statement) || fs.readFileSync(statement, 'utf8').trim() === '') throw new OwnTaskError(name, 'instruction.md is missing or empty: the owner writes it');
      specs.push({ ...spec, dir: path.join(dir, name) });
    } catch (e) {
      if (e instanceof OwnTaskError) errors.push({ id: name, problems: [e.message] });
      else if (e instanceof SyntaxError || e?.code === 'ENOENT') errors.push({ id: name, problems: [`own.json is missing or not JSON`] });
      else throw e;
    }
  }
  return { specs, errors };
}

/** The local tag an own task's image is built under. */
export const ownImageTag = (spec) => `adam-bench-own/${spec.id.toLowerCase()}:${spec.base.slice(0, 12)}`;

/** The Dockerfile of an own task: the workspace image by digest, the repository at /app at the base, the pins. Pure. */
export function ownDockerfile({ workspaceImage, workdir = '/app', toolchain = { env: {}, run: [] } }) {
  const ref = parseImageRef(workspaceImage);
  if (!ref || !ref.digest) throw new OwnTaskError('(dockerfile)', 'the workspace image must be named by digest (name@sha256:...)');
  const lines = [`FROM ${workspaceImage}`, 'USER root', `COPY repo/ ${workdir}/`, `RUN chown -R 10001:10001 ${workdir} && git config --system --add safe.directory '*'`];
  for (const [k, v] of Object.entries(toolchain.env ?? {})) lines.push(`ENV ${k}="${v}"`);
  for (const l of toolchain.run ?? []) lines.push(`RUN ${l}`);
  lines.push(`WORKDIR ${workdir}`, 'USER 10001', '');
  return lines.join('\n');
}

/**
 * A clone of `cloneDir` at `base` into `outDir` that holds only the history up to the base:
 * no later commit, branch, tag or remote. Own tasks never serve later commits.
 */
export function prepareOwnContext({ cloneDir, base, outDir }) {
  fs.rmSync(outDir, { recursive: true, force: true });
  fs.mkdirSync(path.dirname(outDir), { recursive: true });
  execFileSync('git', ['clone', '-q', '--no-local', '--no-checkout', cloneDir, outDir], { stdio: 'pipe' });
  const git = gitIn(outDir);
  git(['checkout', '-q', '--detach', base]);
  for (const ref of git(['for-each-ref', '--format=%(refname)']).split('\n').filter(Boolean)) git(['update-ref', '-d', ref]);
  git(['remote', 'remove', 'origin']);
  git(['reflog', 'expire', '--expire=now', '--all']);
  git(['gc', '-q', '--prune=now']);
}

/** The test script of an own task. Pure: the hidden tests are applied from /tests, the command decides. */
export function ownTestScript({ workdir = '/app', testFiles, testCommand }) {
  for (const f of testFiles) if (/['\0\n]/.test(f)) throw new OwnTaskError('(test.sh)', 'a test path holds a quote or a line break');
  const list = testFiles.map((f) => `'${f}'`).join(' ');
  return [
    '#!/bin/bash',
    'mkdir -p /logs/verifier',
    'fail() { echo 0 > /logs/verifier/reward.txt; exit 0; }',
    `cd ${workdir} || fail`,
    `for f in ${list}; do git checkout -q HEAD -- "$f" 2>/dev/null || rm -f "$f"; done`,
    'git apply --whitespace=nowarn /tests/test_patch.patch || fail',
    `if ( ${testCommand} ); then echo 1 > /logs/verifier/reward.txt; else echo 0 > /logs/verifier/reward.txt; fi`,
    '',
  ].join('\n');
}

/**
 * Build the task directory of an own task under `outDir` (outside the repository). `cloneDir` is the owner's
 * full clone. Returns { dir, tag, testFiles }.
 */
export function materializeOwn({ spec, cloneDir, outDir, workspaceImage, memoryMb = 8192, cpus = 2 }) {
  const git = gitIn(cloneDir);
  const parent = git(['rev-parse', `${spec.commit}^`]).trim();
  if (parent !== spec.base) throw new OwnTaskError(spec.id, 'base is not the first parent of commit');
  const changed = git(['diff', '--name-only', '-z', '--no-renames', spec.base, spec.commit]).split('\0').filter(Boolean);
  const testFiles = changed.filter((p) => classifyPath(p) === 'test').sort();
  if (testFiles.length === 0) throw new OwnTaskError(spec.id, 'the commit changes no test file');
  const excludes = testFiles.map((p) => `:(exclude,literal)${p}`);
  const testPatch = git(['diff', '--binary', '--no-renames', '--no-color', spec.base, spec.commit, '--', ...testFiles.map((p) => `:(literal)${p}`)]);
  const goldPatch = git(['diff', '--binary', '--no-renames', '--no-color', spec.base, spec.commit, '--', '.', ...excludes]);
  const tag = ownImageTag(spec);

  fs.rmSync(outDir, { recursive: true, force: true });
  const put = (rel, text) => {
    const p = path.join(outDir, rel);
    fs.mkdirSync(path.dirname(p), { recursive: true });
    fs.writeFileSync(p, text);
  };
  put('instruction.md', fs.readFileSync(path.join(spec.dir, 'instruction.md'), 'utf8'));
  put(
    'task.toml',
    [
      '[agent]',
      'network_mode = "none"',
      '',
      '[verifier]',
      'network_mode = "public"',
      '',
      '[environment]',
      `cpus = ${cpus}`,
      `memory_mb = ${memoryMb}`,
      `docker_image = "${tag}"`,
      '',
      '[metadata]',
      `repo = "${spec.repo}"`,
      `language = "${spec.language}"`,
      '',
    ].join('\n'),
  );
  put('environment/Dockerfile', ownDockerfile({ workspaceImage, toolchain: spec.toolchain }));
  put('tests/test_patch.patch', testPatch);
  put('tests/test.sh', ownTestScript({ testFiles, testCommand: spec.testCommand }));
  put('solution/solution.patch', goldPatch);
  return { dir: outDir, tag, testFiles };
}

/** A checksum of what the owner wrote and pinned for an own task: instruction.md and own.json. */
export function ownChecksum(spec) {
  const h = crypto.createHash('sha256');
  for (const f of ['own.json', 'instruction.md']) h.update(`${f}\0`).update(fs.readFileSync(path.join(spec.dir, f))).update('\0');
  return h.digest('hex');
}
