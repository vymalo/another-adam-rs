import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { CHECK_TOOLS, READ_OUTPUT_TOOL, compareNumbers, computeNumbers, directionOf, formatComparison, formatNumbers, ratioOf, verdictOf } from '../lib/numbers.mjs';
import { tmp } from './helpers.mjs';

const BENCH = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', 'bin', 'bench.mjs');

/**
 * A ledger with two agents over two tasks, made so each figure is a round number:
 *
 *   adam      6 settled calls over 2 tasks (3.00 per task); cache 3920 + 1800 of 4000 + 2000;
 *             3 named tool calls, one of them `run_checks`
 *   opencode  3 settled calls over 2 tasks (1.50 per task); cache 500 + 250 of 1000 + 500;
 *             3 named tool calls, one of them `run_checks`
 */
function records() {
  const out = [{ kind: 'setup', modelLabel: 'Example Model', seed: 1 }];
  const planned = (run, task, agent) => out.push({ kind: 'state', run, state: 'Planned', task, agent, arm: 'max', source: 'own', language: 'go' });
  const done = (run) => out.push({ kind: 'state', run, state: 'Done', result: { pass: true, grades: [1, 1, 1], flaky: false, exit: 'done', wallMs: 1000 } });
  const call = (run, n, { prompt = 'main', input = 1000, cached = 0, toolNames = [] } = {}) => {
    out.push({ kind: 'call', run, attempt: 1, call: n, phase: 'pending', worstMicro: 100, prompt });
    out.push({ kind: 'call', run, attempt: 1, call: n, phase: 'settled', costMicro: 1, usage: { input, cached, cacheWrite: 0, output: 1, reasoning: 0 }, usageMissing: false, ...(toolNames.length > 0 ? { toolNames } : {}) });
  };

  planned('T1-adam', 'T1', 'adam');
  call('T1-adam', 1, { cached: 980, toolNames: ['read_file'] });
  call('T1-adam', 2, { cached: 980, toolNames: ['run_checks'] });
  call('T1-adam', 3, { cached: 980, toolNames: ['write_file'] });
  call('T1-adam', 4, { cached: 980 });
  done('T1-adam');

  planned('T2-adam', 'T2', 'adam');
  call('T2-adam', 1, { cached: 900 });
  call('T2-adam', 2, { cached: 900 });
  done('T2-adam');

  planned('T1-opencode', 'T1', 'opencode');
  call('T1-opencode', 1, { input: 500, cached: 250, toolNames: ['read_file'] });
  call('T1-opencode', 2, { input: 500, cached: 250, toolNames: ['run_checks'] });
  done('T1-opencode');

  planned('T2-opencode', 'T2', 'opencode');
  call('T2-opencode', 1, { input: 500, cached: 250, toolNames: ['read_file'] });
  done('T2-opencode');

  return out;
}

test('the three numbers come out of the call records, per agent', () => {
  const n = computeNumbers(records());

  assert.deepEqual(n.runs, { finished: 4, total: 4 });
  const adam = n.agents.get('adam');
  assert.equal(adam.tasks, 2);
  assert.equal(adam.calls, 6);
  assert.equal(adam.callsPerTask, 3);
  assert.deepEqual({ input: adam.cache.input, cached: adam.cache.cached }, { input: 6000, cached: 5720 });
  assert.equal(adam.tools.named, 3);
  assert.equal(adam.tools.checks, 1);
  assert.equal(adam.tools.share, 1 / 3);

  const opencode = n.agents.get('opencode');
  assert.equal(opencode.callsPerTask, 1.5);
  assert.equal(opencode.cache.share, 750 / 1500);
  assert.equal(opencode.tools.share, 1 / 3);

  assert.equal(ratioOf(n, 'adam', 'opencode'), 2);
});

test('a row dropped changes the figure — this is the case that goes red when the fold stops counting a record', () => {
  const before = computeNumbers(records());
  const without = records();
  // Drop one settled call of a finished run: `T1-adam` call 2.
  const at = without.findIndex((r) => r.kind === 'call' && r.run === 'T1-adam' && r.call === 2 && r.phase === 'settled');
  assert.notEqual(at, -1);
  without.splice(at, 1);

  const after = computeNumbers(without);
  const adam = after.agents.get('adam');
  assert.equal(adam.calls, 5, 'the settled call of a finished run no longer counts');
  assert.equal(adam.callsPerTask, 2.5);
  assert.equal(adam.cache.cached, 5720 - 980);
  assert.equal(adam.tools.checks, 0);
  assert.notEqual(formatNumbers(after), formatNumbers(before));
});

test('a run that did not finish is not in the numbers, and a child session is out of the cache share', () => {
  const rs = records();
  // An unfinished run: its calls must not move any figure.
  rs.push({ kind: 'state', run: 'T9-adam', state: 'Planned', task: 'T9', agent: 'adam', arm: 'max' });
  rs.push({ kind: 'call', run: 'T9-adam', attempt: 1, call: 1, phase: 'pending', worstMicro: 100, prompt: 'main' });
  rs.push({ kind: 'call', run: 'T9-adam', attempt: 1, call: 1, phase: 'settled', costMicro: 1, usage: { input: 1000, cached: 1000, cacheWrite: 0, output: 1, reasoning: 0 }, usageMissing: false });
  const n = computeNumbers(rs);
  assert.equal(n.agents.get('adam').calls, 6);
  assert.equal(n.agents.get('adam').cache.share, 5720 / 6000);
  assert.equal(n.runs.total, 5);

  // A subagent: another prompt fingerprint. It counts as a call, never as the fixed prompt's cache.
  const withChild = records();
  withChild.push({ kind: 'call', run: 'T2-adam', attempt: 1, call: 3, phase: 'pending', worstMicro: 100, prompt: 'child' });
  withChild.push({ kind: 'call', run: 'T2-adam', attempt: 1, call: 3, phase: 'settled', costMicro: 1, usage: { input: 1000, cached: 0, cacheWrite: 0, output: 1, reasoning: 0 }, usageMissing: false });
  const c = computeNumbers(withChild).agents.get('adam');
  assert.equal(c.calls, 7);
  assert.deepEqual({ input: c.cache.input, cached: c.cache.cached }, { input: 6000, cached: 5720 });
});

test('the check tool is the coder\'s run_checks, and the basis says time is not in the ledger', () => {
  assert.deepEqual([...CHECK_TOOLS], ['run_checks']);
  const n = computeNumbers(records());
  assert.match(n.basis.checksShare, /NOT a share of tool time/);
  const text = formatNumbers(n, { ledger: 'example.jsonl' });
  assert.match(text, /adam \| 2 \| 6 \| 3 \| 0 \(0\) \| 3000 \| 95\.3% \(5720 \/ 6000\) \| 33\.3% \(1 \/ 3\)/);
  assert.match(text, /Calls per task, adam over opencode: 2\.00x/);
});

test('bench numbers prints the table for a ledger, and refuses an empty one', (t) => {
  const dir = tmp(t);
  const file = path.join(dir, 'ledger.jsonl');
  fs.writeFileSync(file, `${records().map((r) => JSON.stringify(r)).join('\n')}\n`);

  const ok = spawnSync(process.execPath, [BENCH, 'numbers', '--ledger', file], { encoding: 'utf8' });
  assert.equal(ok.status, 0, ok.stderr);
  assert.match(ok.stdout, /adam \| 2 \| 6 \| 3 /);
  assert.match(ok.stdout, /opencode \| 2 \| 3 \| 1\.50 /);

  const empty = path.join(dir, 'empty.jsonl');
  fs.writeFileSync(empty, '');
  const none = spawnSync(process.execPath, [BENCH, 'numbers', '--ledger', empty], { encoding: 'utf8' });
  assert.equal(none.status, 1);
  assert.match(none.stderr, /no records/);
});

/**
 * A ledger from a short description: each run is `{ run, task, agent, done = true, calls }`, each
 * call `{ input = 1000, cached = 0, toolNames = [], prompt = 'main', attempt = 1, missing, failed }`.
 * A run's attempts are opened by a `Planned` record each, as the harness writes them.
 */
function ledger(runs) {
  const out = [{ kind: 'setup', modelLabel: 'Example Model', seed: 1 }];
  for (const r of runs) {
    const attempts = Math.max(1, ...r.calls.map((c) => c.attempt ?? 1));
    const counters = new Map();
    for (let attempt = 1; attempt <= attempts; attempt++) {
      out.push({ kind: 'state', run: r.run, state: 'Planned', ...(r.task === undefined ? {} : { task: r.task }), agent: r.agent, arm: 'max' });
      for (const c of r.calls.filter((x) => (x.attempt ?? 1) === attempt)) {
        const n = (counters.get(attempt) ?? 0) + 1;
        counters.set(attempt, n);
        out.push({ kind: 'call', run: r.run, attempt, call: n, phase: 'pending', worstMicro: 100, prompt: c.prompt ?? 'main' });
        const usage = c.missing ? null : { input: c.input ?? 1000, cached: c.cached ?? 0, cacheWrite: 0, output: 1, reasoning: 0 };
        const names = c.toolNames ?? [];
        out.push({ kind: 'call', run: r.run, attempt, call: n, phase: c.failed ? 'failed' : 'settled', costMicro: 1, usage, usageMissing: c.missing === true, ...(names.length > 0 ? { toolNames: names } : {}) });
      }
      if (attempt < attempts) out.push({ kind: 'state', run: r.run, state: 'Interrupted' });
    }
    if (r.done !== false) out.push({ kind: 'state', run: r.run, state: 'Done', result: { pass: true, grades: [1, 1, 1], flaky: false, exit: 'done', wallMs: 1000 } });
  }
  return out;
}

test('read_output calls and input tokens per task come from the settled calls of the finished runs', () => {
  assert.equal(READ_OUTPUT_TOOL, 'read_output');
  const n = computeNumbers(
    ledger([
      {
        run: 'T1.adam.max',
        task: 'T1',
        agent: 'adam',
        calls: [
          { input: 1000, toolNames: ['run', 'read_output'] },
          { input: 3000, toolNames: ['read_output'] },
          // A child session: another fingerprint. Its input is the task's; it is not the fixed prompt's cache.
          { input: 2000, prompt: 'child' },
          // A failed call is neither a settled call nor input.
          { input: 9000, failed: true, toolNames: ['read_output'] },
        ],
      },
      { run: 'T2.adam.max', task: 'T2', agent: 'adam', calls: [{ input: 2000 }, { missing: true, toolNames: ['read_output'] }] },
      // Not finished: nothing of it counts.
      { run: 'T3.adam.max', task: 'T3', agent: 'adam', done: false, calls: [{ input: 50000, toolNames: ['read_output', 'read_output'] }] },
    ]),
  );
  const adam = n.agents.get('adam');
  assert.equal(adam.tasks, 2);
  assert.equal(adam.calls, 5);
  assert.equal(adam.readOutput, 3);
  assert.equal(adam.readOutputPerTask, 1.5);
  assert.deepEqual(adam.input, { tokens: 8000, missing: 1 });
  assert.equal(adam.inputPerTask, 4000);
  assert.equal(adam.cache.input, 6000, 'the child session is out of the fixed prompt');

  const text = formatNumbers(n, { ledger: 'example.jsonl' });
  assert.match(text, /\| adam \| 2 \| 5 \| 2\.50 \| 1\.50 \(3\) \| 4000 \|/);
  assert.match(text, /adam: 1 settled call\(s\) without usage add nothing to the input tokens, which are therefore a lower bound/);
  assert.match(text, /read_output calls \/ task: .*the ledger keeps no tool arguments/);
});

test('a resumed run counts the calls of every attempt: the meter numbers each attempt\'s calls from 1', () => {
  const n = computeNumbers(
    ledger([
      {
        run: 'T1.adam.max',
        task: 'T1',
        agent: 'adam',
        calls: [{ input: 1000 }, { input: 1000, toolNames: ['read_output'] }, { attempt: 2, input: 500, toolNames: ['read_output'] }],
      },
    ]),
  );
  const adam = n.agents.get('adam');
  assert.equal(adam.calls, 3);
  assert.equal(adam.readOutput, 2);
  assert.equal(adam.input.tokens, 2500);
});

/**
 * Before: adam on T1, T2 and T3, with no read_output and big inputs. After: adam on T1, T2 and T4,
 * with read_output calls and smaller inputs, and opencode on T1 (in this ledger only), and a finished
 * run with no task id. Only adam's T1 and T2 are comparable.
 */
function beforeAfter() {
  const before = ledger([
    { run: 'T1.adam.max', task: 'T1', agent: 'adam', calls: [{ input: 10000, cached: 9000 }, { input: 10000, cached: 9000 }, { input: 10000, cached: 9000 }] },
    { run: 'T2.adam.max', task: 'T2', agent: 'adam', calls: [{ input: 10000, cached: 9000 }] },
    { run: 'T3.adam.max', task: 'T3', agent: 'adam', calls: [{ input: 99999, toolNames: ['read_output'] }] },
  ]);
  const after = ledger([
    { run: 'T1.adam.max', task: 'T1', agent: 'adam', calls: [{ input: 6000, cached: 5700 }, { input: 6000, cached: 5700, toolNames: ['read_output'] }, { input: 6000, cached: 5700, toolNames: ['read_output'] }] },
    { run: 'T2.adam.max', task: 'T2', agent: 'adam', calls: [{ input: 6000, cached: 5700, toolNames: ['read_output'] }, { input: 6000, cached: 5700 }] },
    { run: 'T4.adam.max', task: 'T4', agent: 'adam', calls: [{ input: 99999, toolNames: ['read_output', 'read_output'] }] },
    { run: 'T1.opencode.max', task: 'T1', agent: 'opencode', calls: [{ input: 1000 }] },
    { run: 'old-run', agent: 'adam', calls: [{ input: 77777 }] },
  ]);
  return { before, after };
}

test('the comparison counts only the tasks both ledgers finished, per agent, and lists the rest', () => {
  const { before, after } = beforeAfter();
  const cmp = compareNumbers(after, before);

  assert.deepEqual([...cmp.agents.keys()], ['adam'], 'opencode finished nothing in the baseline');
  const adam = cmp.agents.get('adam');
  assert.deepEqual(adam.tasks, ['T1', 'T2']);
  // Before: 4 calls over 2 tasks, no read_output, 40000 input. After: 5 calls, 3 read_output, 30000 input.
  assert.equal(adam.before.callsPerTask, 2);
  assert.equal(adam.after.callsPerTask, 2.5);
  assert.equal(adam.before.readOutputPerTask, 0);
  assert.equal(adam.after.readOutputPerTask, 1.5);
  assert.equal(adam.before.inputPerTask, 20000);
  assert.equal(adam.after.inputPerTask, 15000);
  assert.equal(adam.before.cache.share, 0.9);
  assert.equal(adam.after.cache.share, 0.95);
  assert.deepEqual(adam.verdict, { readOutput: 'up', input: 'down', rule: verdictOf(adam.before, adam.after).rule });
  assert.match(adam.verdict.rule, /^weigh:/);

  assert.deepEqual(Object.fromEntries(cmp.leftOut.agents), {
    adam: { onlyBefore: ['T3'], onlyAfter: ['T4'] },
    opencode: { onlyBefore: [], onlyAfter: ['T1'] },
  });
  assert.deepEqual(cmp.leftOut.untasked, { before: 0, after: 1 });

  // A task finished in one ledger only moves nothing: drop T3 and T4 and the figures are the same.
  const trimmed = compareNumbers(
    after.filter((r) => r.run !== 'T4.adam.max'),
    before.filter((r) => r.run !== 'T3.adam.max'),
  );
  assert.deepEqual(trimmed.agents.get('adam').after, adam.after);
  assert.deepEqual(trimmed.agents.get('adam').before, adam.before);
});

test('the verdict states decision 5\'s case from which way each figure moved, with no threshold', () => {
  const f = (readOutputPerTask, inputPerTask) => ({ readOutputPerTask, inputPerTask });
  assert.match(verdictOf(f(0, 100), f(1, 99)).rule, /^weigh:/);
  assert.match(verdictOf(f(0, 100), f(0.01, 100)).rule, /^against on_error: .*goes back to full/);
  assert.match(verdictOf(f(0, 100), f(1, 101)).rule, /^against on_error:/);
  assert.match(verdictOf(f(1, 100), f(1, 99)).rule, /^for on_error:/);
  assert.match(verdictOf(f(1, 100), f(0, 50)).rule, /^for on_error:/);
  assert.match(verdictOf(f(1, 100), f(1, 100)).rule, /^no saving:/);
  assert.match(verdictOf(f(null, 100), f(1, 100)).rule, /^not decidable/);
  assert.equal(directionOf(1, 2), 'up');
  assert.equal(directionOf(2, 1), 'down');
  assert.equal(directionOf(1, 1), 'same');
  assert.equal(directionOf(undefined, 1), null);
});

test('the comparison prints each figure before and after, the change, the verdict and what was left out', () => {
  const { before, after } = beforeAfter();
  const text = formatComparison(compareNumbers(after, before), { ledger: 'after.jsonl', against: 'before.jsonl' });
  assert.match(text, /^# bench numbers — after\.jsonl against before\.jsonl$/m);
  assert.match(text, /^## adam: 2 task\(s\) finished in both \(2 run\(s\) before, 2 after\)$/m);
  assert.match(text, /^\| calls \/ task \| 2 \| 2\.50 \| \+0\.50 \(\+25\.0%\) \|$/m);
  assert.match(text, /^\| read_output calls \/ task \| 0 \(0\) \| 1\.50 \(3\) \| \+1\.50 \|$/m);
  assert.match(text, /^\| input tokens \/ task \| 20000 \| 15000 \| -5000 \(-25\.0%\) \|$/m);
  assert.match(text, /^\| cache share of the fixed prompt \| 90\.0% \| 95\.0% \| \+5\.0 pt \|$/m);
  assert.match(text, /^Decision 5 \(ADR 0088\): read_output calls per task up, input tokens per task down — weigh: /m);
  assert.match(text, /^- adam: 1 only after \(T4\); 1 only before \(T3\)$/m);
  assert.match(text, /^- opencode: 1 only after \(T1\)$/m);
  assert.match(text, /^- finished runs with no task id, which cannot be matched: 0 before, 1 after$/m);
  assert.doesNotMatch(text, /99999|77777/);
});

/** Two ledgers on disk, outside the repository. */
function writeLedgers(dir) {
  const { before, after } = beforeAfter();
  const write = (name, rs) => {
    const file = path.join(dir, name);
    fs.writeFileSync(file, `${rs.map((r) => JSON.stringify(r)).join('\n')}\n`);
    return file;
  };
  return { before: write('before.jsonl', before), after: write('after.jsonl', after) };
}

test('bench numbers --against prints the comparison, and refuses ledgers with no task in common', (t) => {
  const dir = tmp(t);
  const files = writeLedgers(dir);
  const ok = spawnSync(process.execPath, [BENCH, 'numbers', '--ledger', files.after, '--against', files.before], { encoding: 'utf8' });
  assert.equal(ok.status, 0, ok.stderr);
  assert.match(ok.stdout, /## adam: 2 task\(s\) finished in both/);
  assert.match(ok.stdout, /\| input tokens \/ task \| 20000 \| 15000 \| -5000 \(-25\.0%\) \|/);

  const other = path.join(dir, 'other.jsonl');
  fs.writeFileSync(other, `${ledger([{ run: 'T9.adam.max', task: 'T9', agent: 'adam', calls: [{}] }]).map((r) => JSON.stringify(r)).join('\n')}\n`);
  const none = spawnSync(process.execPath, [BENCH, 'numbers', '--ledger', files.after, '--against', other], { encoding: 'utf8' });
  assert.equal(none.status, 1);
  assert.match(none.stderr, /no agent finished the same task in both ledgers/);

  const empty = path.join(dir, 'empty.jsonl');
  fs.writeFileSync(empty, '');
  const nothing = spawnSync(process.execPath, [BENCH, 'numbers', '--ledger', files.after, '--against', empty], { encoding: 'utf8' });
  assert.equal(nothing.status, 1);
  assert.match(nothing.stderr, /\(--against\) has no records/);
});

test('bench numbers is read-only: no configuration, no network, no write, no child process', (t) => {
  const dir = tmp(t);
  const files = writeLedgers(dir);
  const ledgersBefore = [files.after, files.before].map((f) => fs.readFileSync(f, 'utf8'));
  // A configuration that fails to parse: a command that read it would exit 78.
  const config = path.join(dir, 'config.json');
  fs.writeFileSync(config, '{ not json');
  // Any attempt to reach the network throws in the child.
  const guard = path.join(dir, 'no-network.mjs');
  fs.writeFileSync(
    guard,
    [
      "import dns from 'node:dns';",
      "import net from 'node:net';",
      "const no = (what) => () => { throw new Error(`the network was used: ${what}`); };",
      "net.Socket.prototype.connect = no('net.Socket.connect');",
      "dns.lookup = no('dns.lookup');",
      "dns.promises.lookup = no('dns.promises.lookup');",
      "globalThis.fetch = no('fetch');",
      '',
    ].join('\n'),
  );
  const env = { PATH: process.env.PATH, HOME: dir, BENCH_CONFIG: config, BENCH_HOME: path.join(dir, 'home') };
  for (const args of [['numbers', '--ledger', files.after], ['numbers', '--ledger', files.after, '--against', files.before]]) {
    // The permission model: files may be read, nothing written, no child process started.
    const r = spawnSync(process.execPath, ['--permission', '--allow-fs-read=*', '--import', guard, BENCH, ...args], { encoding: 'utf8', env });
    assert.equal(r.status, 0, `${args.join(' ')}: ${r.stderr}`);
    assert.doesNotMatch(r.stderr, /configuration|network|ERR_ACCESS_DENIED/);
  }
  assert.deepEqual([files.after, files.before].map((f) => fs.readFileSync(f, 'utf8')), ledgersBefore, 'the ledgers are not touched');
  assert.equal(fs.existsSync(path.join(dir, 'home')), false, 'BENCH_HOME is not created');

  // And the module itself: it imports the ledger reader and nothing else.
  const source = fs.readFileSync(path.join(path.dirname(BENCH), '..', 'lib', 'numbers.mjs'), 'utf8');
  assert.deepEqual([...source.matchAll(/^import .* from '([^']+)';$/gm)].map((m) => m[1]), ['./ledger.mjs']);
});
