// A runner on a real ledger, meter and scripted model, with agents that make their calls through the meter.
import fs from 'node:fs';
import path from 'node:path';
import { CALLS_PER_RUN } from '../lib/agent-script.mjs';
import { CapBook } from '../lib/cap.mjs';
import { normalizeConfig } from '../lib/config.mjs';
import { FakeContainers } from '../lib/containers.mjs';
import { priceCall } from '../lib/cost.mjs';
import { Ledger } from '../lib/ledger.mjs';
import { createMeter } from '../lib/meter.mjs';
import { HarnessCrash } from '../lib/runner.mjs';
import { FIXED_USAGE, createScriptedModel } from '../lib/scripted-model.mjs';
import { loadTask } from '../lib/task.mjs';
import { TEST_KEY, rawConfig, tmp } from './helpers.mjs';
import { makeTask } from './taskfix.mjs';

export const PER_CALL = priceCall({ input: 3_000_000, cachedInput: 300_000, cacheWrite: 3_750_000, output: 15_000_000 }, { input: FIXED_USAGE.prompt, cached: FIXED_USAGE.cached, cacheWrite: 0, output: FIXED_USAGE.completion });

/** An agent that makes `calls` model calls through the meter, as the real ones would, and ends as they end. */
export class FakeAgent {
  constructor(name, { origin, calls, crashAfter = {}, fail = {}, exit = null, stopFails = null }) {
    Object.assign(this, { name, origin, calls, crashAfter, fail, forcedExit: exit, stopFails });
    this.log = [];
  }

  async prepare(ctx) {
    this.log.push(['prepare', ctx.run]);
    if (this.fail.prepare?.(ctx.run)) throw new Error('the environment did not start');
  }

  async run(ctx) {
    this.log.push(['run', ctx.run, ctx.attempt]);
    for (let i = 1; i <= this.calls; i++) {
      const res = await fetch(ctx.meter.baseUrl.replace(/^http:\/\/meter-\d:8090/, this.origin) + '/chat/completions', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ model: 'x', messages: [{ role: 'user', content: 'hi' }] }) });
      await res.arrayBuffer();
      if (this.crashAfter[ctx.run] === i && ctx.attempt === 1) throw new HarnessCrash(`${ctx.run} after call ${i}`);
      if (res.status === 429) break;
    }
    if (this.forcedExit) return { exit: this.forcedExit, wallMs: 5 };
    return { exit: ctx.meter.refusal() ? 'budget_hit' : 'done', wallMs: 1000, selfReported: { input: 1, output: 1, calls: this.calls } };
  }

  async collect(ctx) {
    this.log.push(['collect', ctx.run]);
    const text = ctx.task.id.startsWith('wrong') ? 'goodbye' : 'hello';
    return { patch: Buffer.from(`diff --git a/hello.txt b/hello.txt\nnew file mode 100644\n--- /dev/null\n+++ b/hello.txt\n@@ -0,0 +1 @@\n+${text}\n`), notes: { by: this.name } };
  }

  async stop(ctx) {
    this.log.push(['stop', ctx.run]);
    if (this.stopFails?.(ctx.run)) throw Object.assign(new Error('the coder did not stop'), { reason: 'not-stopped' });
  }

  async cleanup(ctx) {
    this.log.push(['cleanup', ctx.run]);
  }
}

/** Everything a runner needs, on a real ledger, meter and scripted model. `reopen()` is a new start of the harness. */
export async function fixture(t, { capUSD = 10, perRunBudgetUSD = 3, prices, calls = { adam: CALLS_PER_RUN.adam, opencode: CALLS_PER_RUN.opencode }, agentOver = {}, taskNames = ['hello-1', 'fix-add-1', 'wrong-1'], verifier = 'none' } = {}) {
  const model = createScriptedModel({ expectKey: TEST_KEY });
  const { url } = await model.listen();
  t.after(() => model.close());
  const home = tmp(t);
  const config = normalizeConfig(rawConfig({ endpoint: url, capUSD, perRunBudgetUSD, ...(prices ? { prices } : {}) }));
  const taskRoot = tmp(t);
  const tasks = Object.fromEntries(taskNames.map((n) => [n, loadTask(makeTask(taskRoot, n, { 'task.toml': `[agent]\nnetwork_mode = "none"\n\n[verifier]\nnetwork_mode = "${verifier}"\n` }))]));
  const slotContainers = new FakeContainers();
  const grading = new FakeContainers({
    onRun: (spec) => {
      const logs = spec.mounts.find((m) => m.target === '/logs/verifier').source;
      const patch = spec.mounts.find((m) => m.target === '/patch/model.patch');
      const good = patch && fs.readFileSync(patch.source, 'utf8').includes('+hello');
      fs.writeFileSync(path.join(logs, 'reward.txt'), good ? '1\n' : '0\n');
      return { exitCode: 0 };
    },
  });
  const windows = [];
  const resets = [];
  const stack = {
    containers: () => slotContainers,
    graderContainers: () => grading,
    ports: { git: 1 },
    meterHost: (slot) => `meter-${slot}`,
    resetWork: async (slot) => resets.push(slot),
    pullWindow: async (slot, fn) => fn(),
    graderWindow: async (fn) => (windows.push('grader'), fn()),
  };
  const ledgerFile = path.join(home, 'ledger.jsonl');
  let ledger;
  let meter;
  const agents = { adam: null, opencode: null };
  const open = async () => {
    ledger = Ledger.open(ledgerFile);
    const book = CapBook.fromFold(config.capMicro, ledger.fold());
    meter = createMeter({ config, ledger, book, env: { [config.keyEnv]: TEST_KEY } });
    const { port } = await meter.listen();
    for (const name of ['adam', 'opencode']) agents[name] = new FakeAgent(name, { origin: `http://127.0.0.1:${port}`, calls: calls[name], ...agentOver });
    return { ledger, book, meter };
  };
  let current = await open();
  const deps = () => ({
    ledger,
    book: current.book,
    meter,
    stack,
    agents,
    config,
    home,
    prepareBase: async () => ({ dir: '/base/base.git', sha: 'a'.repeat(40), slug: 'demo', workdir: '/app' }),
    ...fixtureHooks,
  });
  const fixtureHooks = {};
  const stop = async () => {
    await meter.close();
    ledger.close();
  };
  t.after(stop);
  return {
    model,
    config,
    tasks,
    agents,
    windows,
    resets,
    slotContainers,
    grading,
    home,
    get ledger() {
      return ledger;
    },
    hooks: fixtureHooks,
    deps,
    /** A kill: the process is gone, the ledger file is what it is. A new start reads it. */
    async reopen() {
      await stop();
      current = await open();
      return deps();
    },
  };
}

export const spec = (f, name, agent, extra = {}) => ({ run: `${name}-${agent}-max`, task: f.tasks[name], agent, arm: 'max', slot: 1, ...extra });
export const history = (f, run) => f.ledger.fold().get(run).history;

