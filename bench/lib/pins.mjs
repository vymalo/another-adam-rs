// The pins of the two agents, read from bench/pins.json: the one place they live.
import fs from 'node:fs';
import { fileURLToPath } from 'node:url';

export const PINS_FILE = fileURLToPath(new URL('../pins.json', import.meta.url));

const fail = (what) => {
  throw new Error(`pins: ${what}`);
};

/** Refuse a pin that is loose: a moving tag, a short digest, a range, a weak hash, a path in a name. */
export function validatePins(pins) {
  const c = pins?.coder;
  if (!c || !/^[a-z0-9][a-z0-9._/-]*$/.test(c.image ?? '')) fail('coder.image is not a plain image name');
  if (!/^sha-[0-9a-f]{7}$/.test(c.tag ?? '')) fail('coder.tag must be sha-<7 hex digits>, never a moving tag');
  if (!/^sha256:[0-9a-f]{64}$/.test(c.digest ?? '')) fail('coder.digest must be sha256:<64 hex digits>');
  if (!/^[0-9a-f]{40}$/.test(c.revision ?? '') || c.revision.slice(0, 7) !== c.tag.slice(4)) fail('coder.revision must be the 40-digit commit the tag names');
  const o = pins?.opencode;
  if (!o || !/^\d+\.\d+\.\d+$/.test(o.version ?? '')) fail('opencode.version must be one exact version');
  if (!/^opencode-[a-z0-9-]+$/.test(o.package ?? '')) fail('opencode.package must be a platform package of OpenCode');
  if (!/^sha512-[A-Za-z0-9+/]{86}==$/.test(o.integrity ?? '')) fail('opencode.integrity must be a sha512 integrity');
}

export function loadPins(file = PINS_FILE) {
  const pins = JSON.parse(fs.readFileSync(file, 'utf8'));
  validatePins(pins);
  return pins;
}

export const PINS = loadPins();

/** The coder image as compose and Podman name it: tag and digest. */
export const coderRef = (pins = PINS) => `${pins.coder.image}:${pins.coder.tag}@${pins.coder.digest}`;
