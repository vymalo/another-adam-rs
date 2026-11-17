import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { IntegrityError, checkIntegrity, fetchPinnedBinary, sri, tarballUrl } from '../lib/npm.mjs';
import { PINS, coderRef, validatePins } from '../lib/pins.mjs';
import { tmp } from './helpers.mjs';

/** A real .tgz with the layout of an npm platform package, made by the test. */
function makeTarball(t, script = '#!/bin/sh\necho 9.9.9\n') {
  const dir = tmp(t);
  fs.mkdirSync(path.join(dir, 'package', 'bin'), { recursive: true });
  fs.writeFileSync(path.join(dir, 'package', 'package.json'), '{"name":"example-platform","version":"9.9.9"}\n');
  fs.writeFileSync(path.join(dir, 'package', 'bin', 'opencode'), script, { mode: 0o755 });
  const file = path.join(dir, 'x.tgz');
  execFileSync('tar', ['-czf', file, '-C', dir, 'package']);
  return fs.readFileSync(file);
}

const answer = (bytes, over = {}) => async () => ({ ok: true, status: 200, arrayBuffer: async () => bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.length), ...over });
const pinFor = (bytes, over = {}) => ({ version: '9.9.9', package: 'example-platform', integrity: sri(bytes), ...over });

test('the shipped pins are well formed and name the verified coder and OpenCode', () => {
  validatePins(PINS);
  assert.match(coderRef(PINS), /^ghcr\.io\/vymalo\/another-adam-rs\/coder:sha-[0-9a-f]{7}@sha256:[0-9a-f]{64}$/);
  assert.equal(PINS.coder.revision.slice(0, 7), PINS.coder.tag.slice(4), 'the tag is sha-<first 7 of the revision>');
  assert.match(PINS.opencode.version, /^1\.19\.\d+$/);
  assert.match(PINS.opencode.integrity, /^sha512-[A-Za-z0-9+/]{86}==$/);
});

test('validatePins refuses a pin that is loose', () => {
  const bad = (f) => assert.throws(() => validatePins(f(structuredClone(PINS))), /pins/);
  bad((p) => { p.coder.digest = 'sha256:abc'; return p; });
  bad((p) => { p.coder.tag = 'latest'; return p; });
  bad((p) => { p.coder.revision = 'abc'; return p; });
  bad((p) => { p.opencode.integrity = 'sha1-AAAA'; return p; });
  bad((p) => { p.opencode.version = '^1.19.0'; return p; });
  bad((p) => { p.opencode.package = '../x'; return p; });
});

test('the tarball URL is on the npm registry and names the pinned package and version', () => {
  assert.equal(tarballUrl('opencode-linux-x64', '1.19.5'), 'https://registry.npmjs.org/opencode-linux-x64/-/opencode-linux-x64-1.19.5.tgz');
  assert.throws(() => tarballUrl('../evil', '1.0.0'), /package/);
  assert.throws(() => tarballUrl('a', '1.0.0/../../x'), /version/);
});

test('checkIntegrity accepts the right bytes and refuses a wrong hash, a changed byte and a weak algorithm', () => {
  const bytes = Buffer.from('payload');
  checkIntegrity(bytes, sri(bytes));
  assert.throws(() => checkIntegrity(bytes, sri(Buffer.from('other'))), IntegrityError);
  assert.throws(() => checkIntegrity(Buffer.from('payloaD'), sri(bytes)), IntegrityError);
  const sha1 = `sha1-${crypto.createHash('sha1').update(bytes).digest('base64')}`;
  assert.throws(() => checkIntegrity(bytes, sha1), /algorithm/);
  assert.throws(() => checkIntegrity(bytes, 'not-an-integrity'), IntegrityError);
});

test('the binary is fetched, checked against the pinned integrity, extracted and made read-only', async (t) => {
  const bytes = makeTarball(t);
  const dir = tmp(t);
  const urls = [];
  const got = await fetchPinnedBinary({
    pin: pinFor(bytes),
    dir,
    fetch: async (url, init) => {
      urls.push([url, init?.redirect]);
      return answer(bytes)();
    },
  });
  assert.deepEqual(urls, [['https://registry.npmjs.org/example-platform/-/example-platform-9.9.9.tgz', 'error']]);
  assert.equal(fs.readFileSync(got.path, 'utf8'), '#!/bin/sh\necho 9.9.9\n');
  assert.equal(fs.statSync(got.path).mode & 0o777, 0o555);
  assert.equal(got.cached, false);
  assert.equal(execFileSync(got.path, { encoding: 'utf8' }).trim(), '9.9.9');
  assert.deepEqual(fs.readdirSync(dir).sort(), ['INTEGRITY', 'opencode'], 'no tarball and no scratch is left behind');
});

test('a wrong integrity refuses the download before anything is written', async (t) => {
  const bytes = makeTarball(t);
  const dir = tmp(t);
  const wrong = sri(Buffer.from('some other tarball'));
  await assert.rejects(fetchPinnedBinary({ pin: pinFor(bytes, { integrity: wrong }), dir, fetch: answer(bytes) }), IntegrityError);
  assert.deepEqual(fs.readdirSync(dir), [], 'nothing was written, so nothing can be run');
});

test('a tarball changed after the pin was taken is refused (a registry or a proxy that alters it)', async (t) => {
  const good = makeTarball(t);
  const evil = makeTarball(t, '#!/bin/sh\necho evil\n');
  const dir = tmp(t);
  await assert.rejects(fetchPinnedBinary({ pin: pinFor(good), dir, fetch: answer(evil) }), IntegrityError);
  assert.deepEqual(fs.readdirSync(dir), []);
});

test('a second fetch reuses the verified binary; a binary changed on disk is fetched again', async (t) => {
  const bytes = makeTarball(t);
  const dir = tmp(t);
  let fetches = 0;
  const counted = async () => {
    fetches += 1;
    return answer(bytes)();
  };
  const first = await fetchPinnedBinary({ pin: pinFor(bytes), dir, fetch: counted });
  const second = await fetchPinnedBinary({ pin: pinFor(bytes), dir, fetch: counted });
  assert.deepEqual([fetches, second.cached, second.path], [1, true, first.path]);
  fs.chmodSync(first.path, 0o755);
  fs.writeFileSync(first.path, '#!/bin/sh\necho tampered\n');
  const third = await fetchPinnedBinary({ pin: pinFor(bytes), dir, fetch: counted });
  assert.deepEqual([fetches, third.cached], [2, false]);
  assert.equal(fs.readFileSync(third.path, 'utf8'), '#!/bin/sh\necho 9.9.9\n');
});

test('a registry that answers an error or redirects gives no binary', async (t) => {
  const bytes = makeTarball(t);
  const dir = tmp(t);
  await assert.rejects(fetchPinnedBinary({ pin: pinFor(bytes), dir, fetch: answer(bytes, { ok: false, status: 404 }) }), /404/);
  await assert.rejects(
    fetchPinnedBinary({ pin: pinFor(bytes), dir, fetch: async () => { throw new TypeError('fetch failed'); } }),
    /fetch failed/,
  );
  assert.deepEqual(fs.readdirSync(dir), []);
});

test('a tarball without the binary is refused', async (t) => {
  const dir0 = tmp(t);
  fs.mkdirSync(path.join(dir0, 'package'));
  fs.writeFileSync(path.join(dir0, 'package', 'package.json'), '{}');
  execFileSync('tar', ['-czf', path.join(dir0, 'x.tgz'), '-C', dir0, 'package']);
  const bytes = fs.readFileSync(path.join(dir0, 'x.tgz'));
  await assert.rejects(fetchPinnedBinary({ pin: pinFor(bytes), dir: tmp(t), fetch: answer(bytes) }), /package\/bin\/opencode/);
});
