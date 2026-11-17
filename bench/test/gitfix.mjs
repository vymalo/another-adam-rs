// Git fixtures for the tests: repositories made in a temporary directory by the tests themselves.
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { tmp } from './helpers.mjs';

const GIT_ENV = { ...process.env, GIT_AUTHOR_NAME: 'Test', GIT_AUTHOR_EMAIL: 'test@example.test', GIT_COMMITTER_NAME: 'Test', GIT_COMMITTER_EMAIL: 'test@example.test', GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_SYSTEM: '/dev/null' };

export function git(dir, ...args) {
  return execFileSync('git', ['-C', dir, ...args], { encoding: 'utf8', env: GIT_ENV, stdio: ['ignore', 'pipe', 'pipe'] });
}

/** An empty repository on branch main, in a temporary directory. */
export function initRepo(t) {
  const dir = tmp(t);
  git(dir, 'init', '-q', '-b', 'main');
  return dir;
}

/** Write files (a null value deletes one), stage everything and commit. Returns the commit id. */
export function commitFiles(dir, files, message) {
  for (const [rel, content] of Object.entries(files)) {
    const p = path.join(dir, rel);
    if (content === null) fs.rmSync(p, { force: true });
    else {
      fs.mkdirSync(path.dirname(p), { recursive: true });
      fs.writeFileSync(p, content);
    }
  }
  git(dir, 'add', '-A');
  git(dir, 'commit', '-q', '--allow-empty', '-m', message);
  return git(dir, 'rev-parse', 'HEAD').trim();
}

/** n numbered lines, to make a change of a chosen size. */
export const lines = (n, prefix = 'line') => `${Array.from({ length: n }, (_, i) => `${prefix} ${i}`).join('\n')}\n`;

export const SHA = (c) => c.repeat(64);
