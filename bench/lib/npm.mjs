// OpenCode's binary, from its npm platform package, checked against the pinned integrity before anything is
// written or run. The package is a gzip tarball whose only member the harness reads is `package/bin/opencode`.
//
// The integrity (`sha512-<base64>`, an SRI string, as npm records it) is of the tarball. The bytes are checked in
// memory first; a wrong hash writes nothing. The extracted binary is kept in a directory of its own with the
// hash of the binary itself, so a later run can tell a binary changed on disk and fetch it again.
import { execFileSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

export class IntegrityError extends Error {
  constructor(message) {
    super(message);
    this.name = 'IntegrityError';
  }
}

export const REGISTRY = 'https://registry.npmjs.org';
const MEMBER = 'package/bin/opencode';

/** The SRI string of some bytes. */
export const sri = (bytes) => `sha512-${crypto.createHash('sha512').update(bytes).digest('base64')}`;

/** Throws IntegrityError unless `bytes` hash to `integrity`. Only sha512 is accepted. */
export function checkIntegrity(bytes, integrity) {
  const m = /^([a-z0-9]+)-([A-Za-z0-9+/]+={0,2})$/.exec(integrity ?? '');
  if (!m) throw new IntegrityError('the integrity is not an SRI string');
  if (m[1] !== 'sha512') throw new IntegrityError(`the integrity algorithm ${m[1]} is not accepted; only sha512 is`);
  const actual = crypto.createHash('sha512').update(bytes).digest();
  const want = Buffer.from(m[2], 'base64');
  if (want.length !== actual.length || !crypto.timingSafeEqual(want, actual)) throw new IntegrityError('the downloaded package does not match the pinned integrity');
}

export function tarballUrl(pkg, version, registry = REGISTRY) {
  const m = /^(?:(@[a-z0-9][a-z0-9._-]*)\/)?([a-z0-9][a-z0-9._-]*)$/.exec(pkg ?? '');
  if (!m) throw new Error('the package name is not plain');
  if (!/^\d+\.\d+\.\d+$/.test(version ?? '')) throw new Error('the version is not one exact version');
  return `${registry}/${m[1] ? `${m[1]}/` : ''}${m[2]}/-/${m[2]}-${version}.tgz`;
}

const fileSha = (file) => crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');

/**
 * The verified binary in `dir`. Returns { path, cached, integrity, version }.
 * @param {{version: string, package: string, integrity: string}} opts.pin
 */
export async function fetchPinnedBinary({ pin, dir, fetch = globalThis.fetch, registry = REGISTRY }) {
  const bin = path.join(dir, 'opencode');
  const record = path.join(dir, 'INTEGRITY');
  const ours = `${pin.integrity}\n`;
  try {
    const [recordedIntegrity, recordedBinary] = fs.readFileSync(record, 'utf8').split('\n');
    if (`${recordedIntegrity}\n` === ours && recordedBinary === fileSha(bin)) return { path: bin, cached: true, integrity: pin.integrity, version: pin.version };
  } catch {
    // nothing kept, or unreadable: fetch it
  }

  const res = await fetch(tarballUrl(pin.package, pin.version, registry), { redirect: 'error' });
  if (!res.ok) throw new Error(`the registry answered ${res.status} for ${pin.package}@${pin.version}`);
  const bytes = Buffer.from(await res.arrayBuffer());
  checkIntegrity(bytes, pin.integrity);

  fs.mkdirSync(dir, { recursive: true });
  const scratch = fs.mkdtempSync(path.join(dir, '.fetch-'));
  try {
    const tgz = path.join(scratch, 'package.tgz');
    fs.writeFileSync(tgz, bytes);
    try {
      execFileSync('tar', ['-xzf', tgz, '-C', scratch, '--no-same-owner', '--no-same-permissions', MEMBER], { stdio: 'pipe' });
    } catch {
      throw new Error(`the package has no ${MEMBER}`);
    }
    const extracted = path.join(scratch, ...MEMBER.split('/'));
    if (!fs.lstatSync(extracted).isFile()) throw new Error(`${MEMBER} is not a regular file`);
    fs.rmSync(bin, { force: true });
    fs.renameSync(extracted, bin);
    fs.chmodSync(bin, 0o555);
    fs.writeFileSync(record, `${pin.integrity}\n${fileSha(bin)}\n`);
  } finally {
    fs.rmSync(scratch, { recursive: true, force: true });
  }
  return { path: bin, cached: false, integrity: pin.integrity, version: pin.version };
}
