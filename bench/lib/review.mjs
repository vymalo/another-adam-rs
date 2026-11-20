// The blind quality review of the patches (bench/README.md, "Quality review"; decided by the owner 2026-11-20).
//
// Pass or fail says whether the hidden tests passed; this says how good the change is. Every Done run's patch is read by one
// reviewer (the configured model at reasoning `low`, three times, the median of each score kept; one rubric for every cell), which sees the task statement and the patch and
// never the agent, the arm or the run id: the runs are reviewed in a seeded order that mixes the cells. Files a tool left behind
// (caches) are cut out before the reviewer sees the patch and counted as stray files. The scores are appended to
// `$BENCH_HOME/reviews.jsonl`, one line per run and patch; a run already reviewed with the same patch and rubric is skipped. The
// spend is the reviewer's usage times the price table and counts against the cap with the ledger's.
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { normalizeUsage, priceCall, worstCase, estimateInputTokens } from './cost.mjs';

export const RUBRIC_VERSION = 2;
/**
 * The reviewer's reasoning and samples. At `medium`, `high` and `max` this model's reasoning ran past 32k tokens on a review and
 * no answer came (*verified 2026-11-20*, api.ai.camer.digital, deepseek-v4.1-flash-1017); at `low` it answers in 10 to 20 s but
 * two answers can differ by a point, so three are asked and the median of each score is kept.
 */
export const REVIEW_EFFORT = 'low';
export const SAMPLES = 3;
export const PARALLEL = 4;
export const CRITERIA = Object.freeze(['correctness', 'tests', 'design', 'scope', 'hygiene', 'docs']);
const MAX_PATCH_BYTES = 120_000;
// The reviewer reasons at max: its reasoning counts against this, so it is far above what the answer needs (at 4096 the
// reasoning used it all and no answer came, 2026-11-20).
const MAX_OUTPUT_TOKENS = 16_384;

/** Top-level or nested directories that only a tool writes: a file under one is never part of a change. */
const NOISE = /(^|\/)(\.mypy_cache|\.pytest_cache|__pycache__|node_modules|\.ruff_cache|\.tox|\.nox|\.cache|\.gradle|target\/debug|\.next|dist-newstyle)\//;

export const RUBRIC = `You review a code change made by an automated coding agent for a task. You see the task statement and the change as a unified diff. Judge only what is in front of you. Score each criterion from 1 (poor) to 5 (excellent):

- correctness: does the change do what the task asks, without bugs or regressions you can see (edge cases, error paths)?
- tests: are the new or changed tests meaningful for the change and its edge cases? 1 when none and some were needed.
- design: is the code well placed and structured, consistent with the code around it, without duplication?
- scope: does it change only what the task needs (no unrelated edits, no debris)?
- hygiene: no stray, generated or temporary files, no leftover debug output, clean diff.
- docs: comments, docstrings and changelog or docs entries where the project would expect them; 3 when none were needed.

Answer with one JSON object and nothing else: {"correctness":n,"tests":n,"design":n,"scope":n,"hygiene":n,"docs":n,"overall":n,"summary":"one sentence"}. "overall" is your own 1 to 5 judgement of the change as a pull request a maintainer would merge.`;

/** The patch without the files under tool caches: { patch, files, stray, truncated }. Pure. */
export function cleanPatch(text) {
  const parts = text.split(/(?=^diff --git )/m).filter((p) => p.trim() !== '');
  const files = [];
  let stray = 0;
  const kept = [];
  for (const p of parts) {
    const m = /^diff --git a\/(\S+) b\/(\S+)/.exec(p);
    const file = m ? m[2] : '?';
    if (NOISE.test(file)) {
      stray += 1;
      continue;
    }
    files.push(file);
    kept.push(p);
  }
  let patch = kept.join('');
  const truncated = Buffer.byteLength(patch) > MAX_PATCH_BYTES;
  if (truncated) patch = `${Buffer.from(patch).subarray(0, MAX_PATCH_BYTES).toString('utf8')}\n[... the rest of the diff is cut ...]\n`;
  return { patch, files, stray, truncated };
}

/** The reviewer's request body. Pure. */
export function reviewRequest({ modelId, statement, cleaned }) {
  const user = `# Task statement\n\n${statement.trim()}\n\n# Files changed (${cleaned.files.length})\n\n${cleaned.files.map((f) => `- ${f}`).join('\n') || '(none)'}\n\n# Diff\n\n\`\`\`diff\n${cleaned.patch}\`\`\``;
  return { model: modelId, messages: [{ role: 'system', content: RUBRIC }, { role: 'user', content: user }], max_tokens: MAX_OUTPUT_TOKENS, reasoning_effort: REVIEW_EFFORT, response_format: { type: 'json_object' }, stream: false };
}

/** The scores from the reviewer's answer, or throws. Pure. */
export function parseScores(content) {
  const m = /\{[\s\S]*\}/.exec(String(content ?? ''));
  if (!m) throw new Error('the reviewer gave no JSON object');
  const j = JSON.parse(m[0]);
  const out = {};
  for (const k of [...CRITERIA, 'overall']) {
    const n = Number(j[k]);
    if (!Number.isInteger(n) || n < 1 || n > 5) throw new Error(`the reviewer gave no score 1 to 5 for ${k}`);
    out[k] = n;
  }
  out.summary = typeof j.summary === 'string' ? j.summary.slice(0, 400) : '';
  return out;
}

/** A seeded order of the runs, so a reviewer that drifts over time does not drift along one cell. Pure. */
export function mixedOrder(items, seed = 'review') {
  const key = (x) => crypto.createHash('sha256').update(`${seed}:${x}`).digest('hex');
  return [...items].sort((a, b) => (key(a.run) < key(b.run) ? -1 : 1));
}

export const reviewsFile = (home) => path.join(home, 'reviews.jsonl');

export function readReviews(home) {
  const file = reviewsFile(home);
  if (!fs.existsSync(file)) return [];
  return fs
    .readFileSync(file, 'utf8')
    .split('\n')
    .filter((l) => l.trim() !== '')
    .map((l) => JSON.parse(l));
}

/**
 * Reviews every Done run that has a patch and no review yet. `runs` are folded runs; `statementOf(task)` gives a task's statement;
 * `patchOf(run)` the patch text or null; `send(body)` posts to the endpoint and returns the parsed JSON. `budgetMicro` is what is left
 * under the cap: a review whose worst case does not fit is not sent, and the reviewing stops there.
 */
export async function reviewRuns({ runs, statementOf, patchOf, send, config, home, budgetMicro, out = () => {}, now = () => new Date().toISOString() }) {
  // Only a review that gave scores is done: one that failed is tried again.
  const done = new Set(readReviews(home).filter((r) => r.scores).map((r) => `${r.run}:${r.patchSha}:${r.rubric}`));
  let spent = 0;
  let reviewed = 0;
  let skipped = 0;
  const failed = [];
  const todo = [];
  for (const v of mixedOrder(runs.filter((r) => r.state === 'Done'))) {
    const text = patchOf(v);
    if (text === null) {
      skipped += 1;
      continue;
    }
    const patchSha = crypto.createHash('sha256').update(text).digest('hex').slice(0, 16);
    if (done.has(`${v.run}:${patchSha}:${RUBRIC_VERSION}`)) {
      skipped += 1;
      continue;
    }
    todo.push({ v, text, patchSha });
  }
  let stop = false;
  const one = async ({ v, text, patchSha }) => {
    const cleaned = cleanPatch(text);
    const body = reviewRequest({ modelId: config.modelId, statement: statementOf(v.task) ?? '(the statement is not available)', cleaned });
    const worst = SAMPLES * worstCase(config.prices, { inputTokens: estimateInputTokens(Buffer.byteLength(JSON.stringify(body))), maxOutputTokens: MAX_OUTPUT_TOKENS });
    if (stop || spent + worst > budgetMicro) {
      if (!stop) out(`review: stopped, the next review could pass the cap (${failed.length} failed, ${reviewed} reviewed)`);
      stop = true;
      return;
    }
    spent += worst; // reserved, settled below
    const samples = [];
    const errors = [];
    let costMicro = 0;
    const usages = [];
    for (let i = 0; i < SAMPLES; i++) {
      let answer;
      try {
        answer = await send(body);
      } catch (e) {
        errors.push(e.message);
        costMicro += worst / SAMPLES;
        continue;
      }
      const usage = normalizeUsage(answer.usage);
      // No usage is charged at the worst case, as the meter does.
      costMicro += usage ? priceCall(config.prices, usage) : worst / SAMPLES;
      if (usage) usages.push(usage);
      try {
        samples.push(parseScores(answer.choices?.[0]?.message?.content));
      } catch (e) {
        errors.push(e.message);
      }
    }
    costMicro = Math.round(costMicro);
    spent += costMicro - worst;
    if (samples.length === 0) {
      failed.push({ run: v.run, error: errors[0] ?? 'no answer' });
      fs.appendFileSync(reviewsFile(home), `${JSON.stringify({ t: now(), run: v.run, patchSha, rubric: RUBRIC_VERSION, error: errors[0] ?? 'no answer', costMicro })}\n`);
      return;
    }
    const scores = medianScores(samples);
    fs.appendFileSync(reviewsFile(home), `${JSON.stringify({ t: now(), run: v.run, task: v.task, agent: v.agent, arm: v.arm, patchSha, rubric: RUBRIC_VERSION, effort: REVIEW_EFFORT, samples: samples.length, files: cleaned.files.length, stray: cleaned.stray, truncated: cleaned.truncated, scores, all: samples.map((x) => x.overall), costMicro, usage: usages })}\n`);
    reviewed += 1;
  };
  const queue = [...todo];
  await Promise.all(Array.from({ length: Math.min(PARALLEL, queue.length) }, async () => {
    while (queue.length > 0 && !stop) await one(queue.shift());
  }));
  return { reviewed, skipped, failed, spentMicro: spent };
}

/** Per criterion, the median of the samples' scores (the lower middle for an even count); the summary of the sample closest to it. Pure. */
export function medianScores(samples) {
  const out = {};
  for (const k of [...CRITERIA, 'overall']) {
    const xs = samples.map((x) => x[k]).sort((a, b) => a - b);
    out[k] = xs[Math.floor((xs.length - 1) / 2)];
  }
  const pick = samples.find((x) => x.overall === out.overall) ?? samples[0];
  out.summary = pick.summary;
  return out;
}

/** The scores per cell, the latest valid review of each run: [{ agent, arm, n, mean: {criterion: x}, stray }]. Pure. */
export function scoresByCell(reviews, runs) {
  const latest = new Map();
  for (const r of reviews) if (r.scores) latest.set(r.run, r);
  const byRun = new Map(runs.map((v) => [v.run, v]));
  const cells = new Map();
  for (const r of latest.values()) {
    const v = byRun.get(r.run);
    if (!v || v.state !== 'Done') continue;
    const key = `${v.agent} / ${v.arm}`;
    if (!cells.has(key)) cells.set(key, { agent: v.agent, arm: v.arm, list: [] });
    cells.get(key).list.push(r);
  }
  return [...cells.values()]
    .sort((a, b) => (a.agent + a.arm < b.agent + b.arm ? -1 : 1))
    .map(({ agent, arm, list }) => {
      const mean = {};
      for (const k of [...CRITERIA, 'overall']) mean[k] = list.reduce((s, r) => s + r.scores[k], 0) / list.length;
      return { agent, arm, n: list.length, mean, stray: list.reduce((s, r) => s + (r.stray ?? 0), 0), costMicro: list.reduce((s, r) => s + (r.costMicro ?? 0), 0) };
    });
}
