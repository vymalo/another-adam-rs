import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { TarError, extractEntries, parseTar, pickFiles } from '../lib/tarstream.mjs';
import { tmp } from './helpers.mjs';

/** A tar made by the real `tar` of a directory the test builds. */
function tarOf(dir, args = []) {
  return execFileSync('tar', ['--format=posix', '-cf', '-', '-C', dir, ...args, '.'], { maxBuffer: 1 << 28 });
}

const block = (s, n) => Buffer.concat([Buffer.from(s), Buffer.alloc(n - Buffer.byteLength(s))]);
/** A hand-made header, for the names and types a real tar will not write. */
function header({ name, type = '0', size = 0, link = '' }) {
  const h = Buffer.alloc(512);
  h.write(name, 0);
  h.write('0000644\0', 100);
  h.write('0000000\0', 108);
  h.write('0000000\0', 116);
  h.write(`${size.toString(8).padStart(11, '0')}\0`, 124);
  h.write('00000000000\0', 136);
  h.write('        ', 148);
  h.write(type, 156);
  h.write(link, 157);
  h.write('ustar\0', 257);
  h.write('00', 263);
  let sum = 0;
  for (const b of h) sum += b;
  h.write(`${sum.toString(8).padStart(6, '0')}\0 `, 148);
  return h;
}
const entry = (o, body = '') => Buffer.concat([header({ ...o, size: Buffer.byteLength(body) }), block(body, Math.ceil(Buffer.byteLength(body) / 512) * 512)]);
const end = Buffer.alloc(1024);

test('a real tar of a tree: files, directories and symlinks come out with their content and modes', (t) => {
  const d = tmp(t);
  fs.mkdirSync(path.join(d, 'src', 'deep'), { recursive: true });
  fs.writeFileSync(path.join(d, 'src', 'deep', 'a.txt'), 'a\n');
  fs.writeFileSync(path.join(d, 'run.sh'), '#!/bin/sh\n', { mode: 0o755 });
  fs.symlinkSync('/app/node_modules', path.join(d, 'node_modules'));
  const entries = parseTar(tarOf(d), { allowSymlinks: true });
  const by = Object.fromEntries(entries.map((e) => [e.name, e]));
  assert.equal(by['src/deep/a.txt'].data.toString(), 'a\n');
  assert.equal(by['src'].type, 'dir');
  assert.equal(by['node_modules'].type, 'symlink');
  assert.equal(by['node_modules'].linkpath, '/app/node_modules');
  assert.ok(by['run.sh'].mode & 0o111);
  const out = path.join(tmp(t), 'o');
  extractEntries(entries, out);
  assert.equal(fs.readFileSync(path.join(out, 'src', 'deep', 'a.txt'), 'utf8'), 'a\n');
  assert.equal(fs.readlinkSync(path.join(out, 'node_modules')), '/app/node_modules');
  assert.ok(fs.statSync(path.join(out, 'run.sh')).mode & 0o100);
});

test('a long path (pax) and a long file are read whole', (t) => {
  const d = tmp(t);
  const long = path.join(d, 'x'.repeat(90), 'y'.repeat(90));
  fs.mkdirSync(long, { recursive: true });
  fs.writeFileSync(path.join(long, 'f.txt'), 'z'.repeat(100_000));
  const e = parseTar(tarOf(d)).find((x) => x.type === 'file');
  assert.equal(e.name, `${'x'.repeat(90)}/${'y'.repeat(90)}/f.txt`);
  assert.equal(e.data.length, 100_000);
});

test('symlinks are refused unless the caller allows them; hard links, devices and FIFOs never', (t) => {
  const d = tmp(t);
  fs.writeFileSync(path.join(d, 'a'), 'a');
  fs.symlinkSync('a', path.join(d, 'l'));
  assert.throws(() => parseTar(tarOf(d)), TarError);
  fs.rmSync(path.join(d, 'l'));
  execFileSync('mkfifo', [path.join(d, 'pipe')]);
  assert.throws(() => parseTar(tarOf(d), { allowSymlinks: true }), /not a plain file/);
  fs.rmSync(path.join(d, 'pipe'));
  fs.linkSync(path.join(d, 'a'), path.join(d, 'hard'));
  assert.throws(() => parseTar(tarOf(d), { allowSymlinks: true }), /not a plain file/);
  assert.throws(() => parseTar(Buffer.concat([entry({ name: 'dev', type: '3' }), end]), { allowSymlinks: true }), /not a plain file/);
});

test('a name that leaves the target is refused: absolute, .., and a file under a symlink', (t) => {
  for (const name of ['/etc/passwd', '../x', 'a/../../x', 'a/..']) {
    const tar = Buffer.concat([entry({ name }, 'x'), end]);
    assert.throws(() => parseTar(tar), TarError, name);
  }
  const tar = Buffer.concat([entry({ name: 'd', type: '2', link: '/etc' }), entry({ name: 'd/passwd' }, 'x'), end]);
  const entries = parseTar(tar, { allowSymlinks: true });
  const out = tmp(t);
  assert.throws(() => extractEntries(entries, out), /through a link/);
  assert.equal(fs.existsSync('/etc/passwd.bench-test'), false);
});

test('caps: total bytes, entries and one file', (t) => {
  const d = tmp(t);
  fs.writeFileSync(path.join(d, 'big'), 'x'.repeat(10_000));
  assert.throws(() => parseTar(tarOf(d), { maxBytes: 4096 }), /larger than/);
  assert.throws(() => parseTar(tarOf(d), { maxFileBytes: 100 }), /file is larger/);
  for (let i = 0; i < 20; i++) fs.writeFileSync(path.join(d, `f${i}`), 'x');
  assert.throws(() => parseTar(tarOf(d), { maxEntries: 5 }), /more than 5 entries/);
});

test('a header with a wrong checksum, or a size that runs past the end, is refused', () => {
  const bad = entry({ name: 'a' }, 'x');
  bad.write('0000001', 148);
  assert.throws(() => parseTar(Buffer.concat([bad, end])), /checksum/);
  const short = Buffer.concat([header({ name: 'a', size: 5000 }), Buffer.alloc(512)]);
  assert.throws(() => parseTar(short), /ends inside/);
});

test('pickFiles keeps the named regular files, and says what it refused', () => {
  const tar = Buffer.concat([entry({ name: 'events.jsonl' }, '{}\n'), entry({ name: 'export.json', type: '2', link: '/home/x/.ssh/id' }), entry({ name: 'other.txt' }, 'o'), end]);
  const { files, refused } = pickFiles(parseTar(tar, { allowSymlinks: true }), ['events.jsonl', 'export.json', 'stderr.txt']);
  assert.deepEqual([...files.keys()], ['events.jsonl']);
  assert.equal(files.get('events.jsonl').toString(), '{}\n');
  assert.deepEqual(refused, ['export.json']);
});
