// The owner's private configuration: where it lives, what it may hold, and the guards.
//
// The file is read from $BENCH_CONFIG (default ~/.config/adam-bench/config.json) and is never in
// the repository. It names the endpoint, the model, the price table, the cap and the arms. It
// never holds the key: the key is read from the environment variable `keyEnv` names. Every error
// names a field and never quotes a value. A bad configuration exits with 78.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { usdToMicro } from './cost.mjs';

export const EXIT_CONFIG = 78;

export class ConfigError extends Error {
  constructor(field, message) {
    super(`${field}: ${message}`);
    this.name = 'ConfigError';
    this.field = field;
    this.exitCode = EXIT_CONFIG;
  }
}

/** The schema, written once: the validator and the README read the same table. */
export const SCHEMA = {
  endpoint: { type: 'url', required: true, doc: 'Base URL of the OpenAI-compatible endpoint, with its /v1; no credentials, query or fragment' },
  modelId: { type: 'string', required: true, doc: 'The model id the endpoint expects; sent upstream, never shown in the report by default' },
  modelLabel: { type: 'string', required: true, doc: 'The name the report shows for the model' },
  prices: {
    type: 'object',
    required: true,
    doc: 'USD per million tokens',
    fields: {
      inputPerMTok: { type: 'price', required: true },
      cachedInputPerMTok: { type: 'price', required: true },
      cacheWritePerMTok: { type: 'price', required: true },
      outputPerMTok: { type: 'price', required: true },
    },
  },
  capUSD: { type: 'money', required: true, doc: 'The most the whole benchmark may spend' },
  perRunBudgetUSD: { type: 'money', required: true, doc: 'What one run reserves before it starts' },
  arms: { type: 'arms', required: true, doc: 'Named JSON fragments merged into every request of the arm' },
  keyEnv: { type: 'envName', default: 'BENCH_MODEL_API_KEY', doc: 'The environment variable that holds the key' },
  concurrency: { type: 'int', min: 1, max: 8, default: 2, doc: 'Runs at once' },
  contextWindow: { type: 'int', min: 1024, max: 100_000_000, optional: true, doc: 'The model context window in tokens' },
  timeouts: {
    type: 'object',
    optional: true,
    doc: 'Meter timeouts toward the endpoint, in seconds',
    fields: {
      firstByteSeconds: { type: 'seconds', default: 300 },
      idleSeconds: { type: 'seconds', default: 120 },
    },
  },
  tasks: {
    type: 'object',
    optional: true,
    doc: 'Task sources and grading: the pinned upstream commit, the clones to mine, the workspace image, the Podman service',
    fields: {
      upstreamUrl: { type: 'string', default: 'https://github.com/scaleapi/SWE-bench_Pro-os.git' },
      upstreamCommit: { type: 'commit' },
      upstreamDir: { type: 'path', default: 'v2' },
      clones: { type: 'object' },
      workspaceImage: { type: 'imageByDigest' },
      selfRepo: { type: 'string', default: 'another-adam-rs' },
      seed: { type: 'string', default: '1' },
      podman: { type: 'object', fields: { connection: { type: 'string' }, url: { type: 'string' } } },
    },
  },
};

/** A request field an arm may not set: they would change what is being compared or break the relay. */
const FORBIDDEN_FRAGMENT_KEYS = new Set(['model', 'messages', 'tools', 'tool_choice', 'stream', 'stream_options', 'n']);
const SECRET_WORDS = new Set(['key', 'apikey', 'secret', 'token', 'password', 'passwd', 'pwd', 'credential', 'credentials', 'authorization', 'bearer', 'auth', 'cookie']);
// A secret-named field that holds a number is still a counter when the name also says what it counts.
const COUNTER_WORDS = new Set(['budget', 'limit', 'count', 'max', 'min', 'size', 'length', 'usage', 'ttl', 'timeout', 'seconds', 'ms']);
const KEY_PREFIXES = /^(sk-|sk_|pk-|ghp_|gho_|ghu_|ghs_|github_pat_|glpat-|xox[abprs]-|AKIA[0-9A-Z]{8}|AIza|eyJ[A-Za-z0-9_-]{10,}\.)/;

function nameWords(name) {
  return name
    .replace(/([a-z0-9])([A-Z])/g, '$1_$2')
    .toLowerCase()
    .split(/[^a-z0-9]+/)
    .filter(Boolean);
}

const isSecretName = (name) => nameWords(name).some((w) => SECRET_WORDS.has(w));

function entropy(s) {
  const counts = new Map();
  for (const ch of s) counts.set(ch, (counts.get(ch) ?? 0) + 1);
  let h = 0;
  for (const n of counts.values()) h -= (n / s.length) * Math.log2(n / s.length);
  return h;
}

/**
 * Does a string look like a key? A heuristic: a known prefix, long hex, or (with `dense`) long
 * dense base64-like text. Real model ids look like the last, so `dense` is off for them.
 */
export function looksLikeKey(s, { dense = true } = {}) {
  if (typeof s !== 'string') return false;
  const t = s.trim();
  if (KEY_PREFIXES.test(t)) return true;
  if (/^[0-9a-fA-F]{32,}$/.test(t)) return true;
  if (dense && /^[A-Za-z0-9_+=-]{32,}$/.test(t) && /[0-9]/.test(t) && /[A-Za-z]/.test(t) && entropy(t) >= 4.0) return true;
  return false;
}

const isEmptyValue = (v) => v === undefined || v === null || v === '' || (Array.isArray(v) && v.length === 0) || (isObject(v) && Object.keys(v).length === 0);

/** The density rule applies everywhere but to the model id, its label and anything inside an arm. */
const densityApplies = (where) => !(where === 'modelId' || where === 'modelLabel' || where === 'arms' || where.startsWith('arms.') || where.startsWith('arms['));

/** Walk every value but the endpoint and keyEnv; refuse a secret-named field that holds anything and any key-like string. */
function scanForSecrets(value, where, skip) {
  if (typeof value === 'string') {
    if (looksLikeKey(value, { dense: densityApplies(where) })) throw new ConfigError(where, 'holds a value that looks like a key; the key belongs only in the environment variable named by keyEnv');
    return;
  }
  if (Array.isArray(value)) {
    value.forEach((v, i) => scanForSecrets(v, `${where}[${i}]`, skip));
    return;
  }
  if (value && typeof value === 'object') {
    for (const [name, v] of Object.entries(value)) {
      const here = where ? `${where}.${name}` : name;
      if (skip.has(here)) continue;
      const counter = typeof v === 'number' && nameWords(name).some((w) => COUNTER_WORDS.has(w));
      if (isSecretName(name) && !isEmptyValue(v) && !counter) {
        throw new ConfigError(here, 'a field named like a secret holds a value; the key belongs only in the environment variable named by keyEnv');
      }
      scanForSecrets(v, here, skip);
    }
  }
}

function checkUnknown(obj, fields, where) {
  for (const name of Object.keys(obj)) {
    if (!Object.hasOwn(fields, name)) throw new ConfigError(where ? `${where}.${name}` : name, 'unknown field');
  }
}

const isObject = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);

function checkFragment(value, where) {
  if (Array.isArray(value)) return value.forEach((v, i) => checkFragment(v, `${where}[${i}]`));
  if (!isObject(value)) return;
  for (const [name, v] of Object.entries(value)) {
    if (name === '__proto__' || name === 'constructor' || name === 'prototype') throw new ConfigError(`${where}.${name}`, 'field name not allowed');
    checkFragment(v, `${where}.${name}`);
  }
}

function isLoopbackHost(hostname) {
  const h = hostname.toLowerCase();
  return h === 'localhost' || h === '[::1]' || /^127(\.\d{1,3}){1,3}$/.test(h);
}

function parseEndpoint(value) {
  let url;
  try {
    url = new URL(value);
  } catch {
    throw new ConfigError('endpoint', 'not a valid URL');
  }
  if (url.protocol !== 'http:' && url.protocol !== 'https:') throw new ConfigError('endpoint', 'must be http or https');
  if (url.protocol === 'http:' && !isLoopbackHost(url.hostname)) throw new ConfigError('endpoint', 'must be https unless the host is on the loopback (127.x, localhost, ::1): the key would cross the network in clear');
  if (url.username || url.password) throw new ConfigError('endpoint', 'must not hold credentials; the key is read from the environment');
  if (url.search || url.hash) throw new ConfigError('endpoint', 'must not hold a query or a fragment');
  if (!url.hostname) throw new ConfigError('endpoint', 'has no host');
  return `${url.origin}${url.pathname}`.replace(/\/+$/, '');
}

/** Validate a parsed configuration object (no file, no environment) and return the normalised one. */
export function normalizeConfig(raw) {
  if (!isObject(raw)) throw new ConfigError('(file)', 'must be a JSON object');
  scanForSecrets(raw, '', new Set(['endpoint', 'keyEnv', 'tasks.upstreamCommit']));
  checkUnknown(raw, SCHEMA, '');
  for (const [name, spec] of Object.entries(SCHEMA)) {
    if (spec.required && raw[name] === undefined) throw new ConfigError(name, 'is required');
  }

  const text = (name) => {
    const v = raw[name];
    if (typeof v !== 'string' || v.trim() === '' || v.length > 200 || /[\r\n]/.test(v)) throw new ConfigError(name, 'must be a short single-line string');
    return v.trim();
  };

  const endpoint = parseEndpoint(raw.endpoint);
  const modelId = text('modelId');
  const modelLabel = text('modelLabel');

  if (!isObject(raw.prices)) throw new ConfigError('prices', 'must be an object');
  checkUnknown(raw.prices, SCHEMA.prices.fields, 'prices');
  const price = (name) => {
    const micro = usdToMicro(raw.prices[name], 1e6);
    if (micro === null) throw new ConfigError(`prices.${name}`, 'must be a number of USD per million tokens, 0 or more');
    return micro;
  };
  const prices = {
    input: price('inputPerMTok'),
    cachedInput: price('cachedInputPerMTok'),
    cacheWrite: price('cacheWritePerMTok'),
    output: price('outputPerMTok'),
  };

  const money = (name) => {
    const micro = usdToMicro(raw[name], 1e6);
    if (micro === null || micro === 0) throw new ConfigError(name, 'must be a number of USD above 0');
    return micro;
  };
  const capMicro = money('capUSD');
  const perRunBudgetMicro = money('perRunBudgetUSD');
  if (perRunBudgetMicro > capMicro) throw new ConfigError('perRunBudgetUSD', 'must not exceed capUSD');

  if (!isObject(raw.arms) || Object.keys(raw.arms).length === 0) throw new ConfigError('arms', 'must be an object with at least one arm');
  const arms = {};
  for (const [name, fragment] of Object.entries(raw.arms)) {
    if (!/^[a-z][a-z0-9_-]{0,31}$/.test(name)) throw new ConfigError(`arms.${name}`, 'an arm name is lowercase letters, digits, - and _, up to 32');
    if (!isObject(fragment)) throw new ConfigError(`arms.${name}`, 'must be a JSON object');
    checkFragment(fragment, `arms.${name}`);
    for (const k of Object.keys(fragment)) {
      if (FORBIDDEN_FRAGMENT_KEYS.has(k)) throw new ConfigError(`arms.${name}.${k}`, 'an arm may not set this request field');
    }
    arms[name] = fragment;
  }

  const keyEnv = raw.keyEnv === undefined ? SCHEMA.keyEnv.default : raw.keyEnv;
  if (typeof keyEnv !== 'string' || !/^[A-Z_][A-Z0-9_]{0,63}$/.test(keyEnv)) throw new ConfigError('keyEnv', 'must be the NAME of an environment variable, such as BENCH_MODEL_API_KEY');

  const int = (name, spec) => {
    const v = raw[name];
    if (v === undefined) return spec.default ?? null;
    if (!Number.isSafeInteger(v) || v < spec.min || v > spec.max) throw new ConfigError(name, `must be a whole number from ${spec.min} to ${spec.max}`);
    return v;
  };
  const concurrency = int('concurrency', SCHEMA.concurrency);
  const contextWindow = int('contextWindow', SCHEMA.contextWindow);

  const t = raw.timeouts ?? {};
  if (!isObject(t)) throw new ConfigError('timeouts', 'must be an object');
  checkUnknown(t, SCHEMA.timeouts.fields, 'timeouts');
  const seconds = (name) => {
    const v = t[name] ?? SCHEMA.timeouts.fields[name].default;
    if (typeof v !== 'number' || !Number.isFinite(v) || v <= 0 || v > 3600) throw new ConfigError(`timeouts.${name}`, 'must be a number of seconds from above 0 to 3600');
    return Math.round(v * 1000);
  };
  const timeouts = { firstByteMs: seconds('firstByteSeconds'), idleMs: seconds('idleSeconds') };

  const tasks = normalizeTasks(raw.tasks);
  return { endpoint, modelId, modelLabel, prices, capMicro, perRunBudgetMicro, arms, keyEnv, concurrency, contextWindow, timeouts, tasks };
}

/** The `tasks` block: task sources and grading. Every field optional; the commands that need one say which. */
function normalizeTasks(raw) {
  const f = SCHEMA.tasks.fields;
  const out = { upstreamUrl: f.upstreamUrl.default, upstreamCommit: null, upstreamDir: f.upstreamDir.default, clones: {}, workspaceImage: null, selfRepo: f.selfRepo.default, seed: f.seed.default, podman: {} };
  if (raw === undefined) return out;
  if (!isObject(raw)) throw new ConfigError('tasks', 'must be an object');
  checkUnknown(raw, f, 'tasks');
  const str = (name) => {
    const v = raw[name];
    if (typeof v !== 'string' || v === '' || v.length > 500 || /[\s\0]/.test(v)) throw new ConfigError(`tasks.${name}`, 'must be a short string with no space');
    return v;
  };
  if (raw.upstreamUrl !== undefined) out.upstreamUrl = str('upstreamUrl');
  if (raw.upstreamCommit !== undefined) {
    out.upstreamCommit = str('upstreamCommit');
    if (!/^[0-9a-f]{40}$/.test(out.upstreamCommit)) throw new ConfigError('tasks.upstreamCommit', 'must be a full 40-digit commit id');
  }
  if (raw.upstreamDir !== undefined) {
    out.upstreamDir = str('upstreamDir');
    if (out.upstreamDir.startsWith('/') || out.upstreamDir.split('/').includes('..')) throw new ConfigError('tasks.upstreamDir', 'must be a relative path');
  }
  if (raw.clones !== undefined) {
    if (!isObject(raw.clones)) throw new ConfigError('tasks.clones', 'must be an object of name to absolute path');
    for (const [name, p] of Object.entries(raw.clones)) {
      if (!/^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$/.test(name)) throw new ConfigError(`tasks.clones.${name}`, 'a clone name is plain letters, digits, . - and _');
      if (typeof p !== 'string' || !path.isAbsolute(p)) throw new ConfigError(`tasks.clones.${name}`, 'must be an absolute path');
      out.clones[name] = p;
    }
  }
  if (raw.workspaceImage !== undefined) {
    out.workspaceImage = str('workspaceImage');
    if (!/^[a-z0-9][a-z0-9._:/-]*@sha256:[0-9a-f]{64}$/i.test(out.workspaceImage)) throw new ConfigError('tasks.workspaceImage', 'must name the image by digest (name@sha256:...)');
  }
  if (raw.selfRepo !== undefined) out.selfRepo = str('selfRepo');
  if (raw.seed !== undefined) out.seed = String(typeof raw.seed === 'number' ? raw.seed : str('seed'));
  if (raw.podman !== undefined) {
    if (!isObject(raw.podman)) throw new ConfigError('tasks.podman', 'must be an object');
    checkUnknown(raw.podman, f.podman.fields, 'tasks.podman');
    for (const k of ['connection', 'url']) {
      if (raw.podman[k] === undefined) continue;
      const v = raw.podman[k];
      if (typeof v !== 'string' || v === '' || /[\s\0]/.test(v)) throw new ConfigError(`tasks.podman.${k}`, 'must be a short string with no space');
      out.podman[k] = v;
    }
  }
  return out;
}

/** What may be shown about a configuration: no URL path, no id, no prices. */
export function describeConfig(config) {
  const host = new URL(config.endpoint).host;
  return [
    `model label: ${config.modelLabel}`,
    `endpoint host: ${host}`,
    `arms: ${Object.keys(config.arms).join(', ')}`,
    `key variable: ${config.keyEnv}`,
    `concurrency: ${config.concurrency}`,
  ].join('\n');
}

function realpathOrSelf(p) {
  try {
    return fs.realpathSync(p);
  } catch {
    return path.resolve(p);
  }
}

/** The work-tree roots a configuration may not live in: those of this checkout and of the working directory. */
export function repoRoots(cwd = process.cwd()) {
  const here = path.dirname(fileURLToPath(import.meta.url));
  const roots = new Set();
  for (const start of [here, cwd]) {
    let dir = realpathOrSelf(start);
    for (;;) {
      if (fs.existsSync(path.join(dir, '.git'))) {
        roots.add(dir);
        break;
      }
      const parent = path.dirname(dir);
      if (parent === dir) break;
      dir = parent;
    }
  }
  return [...roots];
}

/** Inside `root`, or `root` itself: a relative path that is neither `..`, nor starts with `../`, nor is absolute. */
function isInside(root, real) {
  const rel = path.relative(root, real);
  return !(rel === '..' || rel.startsWith(`..${path.sep}`) || path.isAbsolute(rel));
}

/** The nearest directory at or above `file` (symlinks resolved, missing parts skipped) that has a `.git`, file or directory. */
export function enclosingWorkTree(file) {
  let p = path.resolve(file);
  while (!fs.existsSync(p)) {
    const up = path.dirname(p);
    if (up === p) return null;
    p = up;
  }
  let dir = realpathOrSelf(p);
  try {
    if (!fs.statSync(dir).isDirectory()) dir = path.dirname(dir);
  } catch {
    dir = path.dirname(dir);
  }
  for (;;) {
    if (fs.existsSync(path.join(dir, '.git'))) return dir;
    const parent = path.dirname(dir);
    if (parent === dir) return null;
    dir = parent;
  }
}

/**
 * Refuse a path inside a work tree: any directory above it (symlinks resolved) with a `.git`, and
 * any of `roots` (the harness's own checkout and the working directory by default).
 */
export function assertOutsideRepo(file, field, roots = repoRoots()) {
  const inside = () => new ConfigError(field, 'is inside the repository; keep it outside, where it cannot be committed');
  if (enclosingWorkTree(file)) throw inside();
  const real = realpathOrSelf(file);
  for (const root of roots) if (isInside(realpathOrSelf(root), real)) throw inside();
}

export function configPath(env = process.env) {
  return env.BENCH_CONFIG || path.join(os.homedir(), '.config', 'adam-bench', 'config.json');
}

export function benchHome(env = process.env) {
  return env.BENCH_HOME || path.join(os.homedir(), '.local', 'share', 'adam-bench');
}

/** Read, guard and validate the configuration file. `roots` is for tests. */
export function loadConfig({ env = process.env, roots } = {}) {
  const file = configPath(env);
  const where = env.BENCH_CONFIG ? 'BENCH_CONFIG' : 'config file';
  assertOutsideRepo(file, where, roots);
  let text;
  try {
    text = fs.readFileSync(file, 'utf8');
  } catch (e) {
    throw new ConfigError(where, e.code === 'ENOENT' ? `no such file: ${file}` : `cannot be read (${e.code ?? 'error'})`);
  }
  let raw;
  try {
    raw = JSON.parse(text);
  } catch {
    throw new ConfigError(where, 'is not valid JSON');
  }
  const config = normalizeConfig(raw);
  const key = env[config.keyEnv];
  if (typeof key === 'string' && key.length >= 8 && text.includes(key)) {
    throw new ConfigError(where, `holds the value of ${config.keyEnv}; remove it from the file`);
  }
  config.file = file;
  return config;
}

/** The key, from the environment variable the configuration names. Never from a file. */
export function readKey(config, env = process.env) {
  const key = env[config.keyEnv];
  if (typeof key !== 'string' || key === '') throw new ConfigError('keyEnv', `the environment variable ${config.keyEnv} is not set`);
  if (/[\u0000-\u0020\u007f]/.test(key)) {
    throw new ConfigError('keyEnv', `the environment variable ${config.keyEnv} holds a space or a control character (a line break, a tab); a key has none`);
  }
  return key;
}
