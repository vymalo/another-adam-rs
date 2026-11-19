// What a container wrote reaches the host as a tar stream this module parses, never as files the host opens.
//
// The principle (bench/README.md, "What the host trusts"): the host runs no git in, reads no file from and deletes no
// file in a place a container could write. A tree or a log comes out of a container as `tar -cf -`; this parser
// reads it with limits: regular files and directories only (symbolic links only when the caller asks, for a work
// tree, and they are never followed), no hard link, device or FIFO, every name normalised and refused if it is
// absolute, holds `..` or a NUL byte, a cap on the bytes, the entries and one file.
//
// An entry that is not acceptable (a FIFO, a device, a hard link, a link nobody asked for, a file over its cap) is
// **refused and skipped**: it is recorded in `entries.refused` with its path and reason, and the rest is read, so one
// bad entry does not end a run. Only a broken stream (a wrong checksum, a name that leaves the target, an archive that
// ends inside an entry) or one over the TOTAL cap throws: that is infrastructure. What it extracts the host owns.
import fs from 'node:fs';
import path from 'node:path';

export class TarError extends Error {
  constructor(message) {
    super(message);
    this.name = 'TarError';
  }
}

export const DEFAULT_LIMITS = Object.freeze({ maxBytes: 512 * 1024 * 1024, maxEntries: 200_000, maxFileBytes: 256 * 1024 * 1024 });

const octal = (buf) => {
  if (buf[0] & 0x80) {
    // base-256 (GNU): a big-endian number in the remaining bytes
    let n = 0n;
    for (let i = 0; i < buf.length; i++) n = (n << 8n) | BigInt(i === 0 ? buf[i] & 0x7f : buf[i]);
    if (n > BigInt(Number.MAX_SAFE_INTEGER)) throw new TarError('a size is not a safe number');
    return Number(n);
  }
  const text = buf.toString('latin1').replace(/\0.*$/s, '').trim();
  if (text === '') return 0;
  if (!/^[0-7]+$/.test(text)) throw new TarError('a header field is not octal');
  return parseInt(text, 8);
};
const str = (buf) => buf.toString('utf8').replace(/\0.*$/s, '');

function paxRecords(data) {
  const out = {};
  let i = 0;
  while (i < data.length) {
    const sp = data.indexOf(0x20, i);
    if (sp < 0) throw new TarError('a pax record is malformed');
    const len = Number(data.subarray(i, sp).toString('latin1'));
    if (!Number.isSafeInteger(len) || len < 4 || i + len > data.length) throw new TarError('a pax record is malformed');
    const rec = data.subarray(sp + 1, i + len - 1).toString('utf8');
    const eq = rec.indexOf('=');
    if (eq > 0) out[rec.slice(0, eq)] = rec.slice(eq + 1);
    i += len;
  }
  return out;
}

/** A tar entry name, normalised; throws unless it stays below the target. Returns '' for the root itself. */
export function cleanName(raw) {
  if (raw.includes('\0')) throw new TarError('a name holds a NUL byte');
  if (raw.startsWith('/')) throw new TarError(`a name is absolute: ${JSON.stringify(raw)}`);
  const parts = [];
  for (const p of raw.split('/')) {
    if (p === '' || p === '.') continue;
    if (p === '..') throw new TarError(`a name leaves the target: ${JSON.stringify(raw)}`);
    parts.push(p);
  }
  return parts.join('/');
}

/**
 * Parse a whole tar held in memory. Returns [{ name, type: 'file'|'dir'|'symlink', mode, data?, linkpath? }].
 * Anything but a regular file, a directory and (with `allowSymlinks`) a symbolic link is an error.
 */
export function parseTar(buf, { maxBytes = DEFAULT_LIMITS.maxBytes, maxEntries = DEFAULT_LIMITS.maxEntries, maxFileBytes = DEFAULT_LIMITS.maxFileBytes, allowSymlinks = false } = {}) {
  if (buf.length > maxBytes) throw new TarError(`the archive is larger than ${maxBytes} bytes`);
  const entries = [];
  entries.refused = [];
  let pos = 0;
  let pax = {};
  let longName = null;
  let longLink = null;
  while (pos + 512 <= buf.length) {
    const h = buf.subarray(pos, pos + 512);
    if (h.every((b) => b === 0)) break;
    let sum = 0;
    for (let i = 0; i < 512; i++) sum += i >= 148 && i < 156 ? 0x20 : h[i];
    if (sum !== octal(h.subarray(148, 156))) throw new TarError('a header has a wrong checksum');
    const type = String.fromCharCode(h[156] || 0x30);
    const size = octal(h.subarray(124, 136));
    const dataStart = pos + 512;
    if (dataStart + size > buf.length) throw new TarError('the archive ends inside an entry');
    const data = buf.subarray(dataStart, dataStart + size);
    pos = dataStart + Math.ceil(size / 512) * 512;
    if (type === 'x') {
      pax = { ...pax, ...paxRecords(data) };
      continue;
    }
    if (type === 'g') continue;
    if (type === 'L') {
      longName = str(data);
      continue;
    }
    if (type === 'K') {
      longLink = str(data);
      continue;
    }
    let name = pax.path ?? longName ?? `${str(h.subarray(345, 500)) ? `${str(h.subarray(345, 500))}/` : ''}${str(h.subarray(0, 100))}`;
    const link = pax.linkpath ?? longLink ?? str(h.subarray(157, 257));
    const realSize = pax.size !== undefined ? Number(pax.size) : size;
    pax = {};
    longName = null;
    longLink = null;
    const clean = cleanName(name);
    name = clean;
    if (entries.length >= maxEntries) throw new TarError(`the archive has more than ${maxEntries} entries`);
    const mode = octal(h.subarray(100, 108)) & 0o777;
    if (type === '0' || type === '7') {
      if (clean === '') throw new TarError('a file has no name');
      if (realSize > maxFileBytes) entries.refused.push({ name: clean, reason: `a file is larger than ${maxFileBytes} bytes` });
      else entries.push({ name: clean, type: 'file', mode, data: Buffer.from(data) });
    } else if (type === '5') {
      if (clean !== '') entries.push({ name: clean, type: 'dir', mode });
    } else if (type === '2' && allowSymlinks) {
      if (clean === '' || link.includes('\0')) throw new TarError('a link is malformed');
      entries.push({ name: clean, type: 'symlink', mode, linkpath: link });
    } else if (type === '2') {
      entries.refused.push({ name: clean, reason: 'a symbolic link is not allowed here' });
    } else {
      entries.refused.push({ name: clean, reason: `not a plain file (type ${JSON.stringify(type)})` });
    }
  }
  return entries;
}

/** The named regular files among entries, by exact top-level name. Anything named but not a regular file is `refused`. */
export function pickFiles(entries, names) {
  const wanted = new Set(names);
  const files = new Map();
  const refused = (entries.refused ?? []).filter((r) => wanted.has(r.name)).map((r) => r.name);
  for (const e of entries) {
    if (!wanted.has(e.name)) continue;
    if (e.type === 'file') files.set(e.name, e.data);
    else refused.push(e.name);
  }
  return { files, refused };
}

/** Write entries under `dest` (made if it is not there). The host owns what it writes; no link is followed. */
export function extractEntries(entries, dest) {
  fs.mkdirSync(dest, { recursive: true });
  const root = fs.realpathSync(dest);
  const ensureParents = (name) => {
    let cur = root;
    for (const part of path.posix.dirname(name).split('/')) {
      if (part === '.' || part === '') continue;
      cur = path.join(cur, part);
      let st = null;
      try {
        st = fs.lstatSync(cur);
      } catch {
        st = null;
      }
      if (st === null) fs.mkdirSync(cur, { mode: 0o755 });
      else if (st.isSymbolicLink()) throw new TarError(`a name goes through a link: ${JSON.stringify(name)}`);
      else if (!st.isDirectory()) throw new TarError(`a name goes through a file: ${JSON.stringify(name)}`);
    }
  };
  for (const e of entries) {
    ensureParents(e.name);
    const target = path.join(root, e.name);
    if (e.type === 'dir') {
      if (!fs.existsSync(target)) fs.mkdirSync(target, { mode: 0o755 });
    } else if (e.type === 'file') {
      fs.rmSync(target, { force: true });
      fs.writeFileSync(target, e.data, { mode: e.mode & 0o111 ? 0o755 : 0o644 });
    } else if (e.type === 'symlink') {
      fs.rmSync(target, { force: true });
      fs.symlinkSync(e.linkpath, target);
    }
  }
}
