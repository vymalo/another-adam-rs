// Shared by the tests. Made-up values only: nothing here is a real endpoint, model, price or key.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

export const TEST_KEY = 'example-key-for-tests-0001';
export const MODEL_ID = 'example-model';

/** A per-test temporary directory, outside the repository. */
export function tmp(t) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'adam-bench-test-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  return dir;
}

/** A raw (file-shaped) configuration. Prices in USD per million tokens. */
export function rawConfig(overrides = {}) {
  return {
    endpoint: 'https://models.example.test/v1',
    modelId: MODEL_ID,
    modelLabel: 'Example Model',
    prices: { inputPerMTok: 3, cachedInputPerMTok: 0.3, cacheWritePerMTok: 3.75, outputPerMTok: 15 },
    contextWindow: 131072,
    capUSD: 10,
    perRunBudgetUSD: 3,
    arms: { max: { reasoning_effort: 'high' }, off: { reasoning_effort: 'none' } },
    ...overrides,
  };
}

/** A made-up work tree: a directory with a `.git` entry. */
export function fakeRepo(t) {
  const dir = tmp(t);
  fs.mkdirSync(path.join(dir, '.git'));
  return dir;
}
