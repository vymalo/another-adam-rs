// An A2A 1.0 client for the coder (JSON-RPC over HTTP), and the loop that drives one task to its end.
//
// Polling decides. `SendStreamingMessage` starts the task and its stream is read for latency and for the
// per-call usage reports only; what the harness believes of the task is what `GetTask` says every
// `pollMs` (10 s). A task that is `input-required` gets one fixed reply, the same for every run; the
// second time it parks, or at the time limit, the harness sends `CancelTask`. A meter refusal ends the run
// too (it is a limit, like time). The bearer lives in a private field: it is in no error, no log line and
// no result.
export const USAGE_URI = 'https://agents.vymalo.com/a2a/extensions/usage/v1';
/** The one reply a parked task gets. Fixed: never built from the run, the task or the model's question. */
export const NUDGE = 'Nobody can answer questions in this run; decide yourself and finish.';

export class A2AError extends Error {
  constructor(message, status = null) {
    super(message);
    this.name = 'A2AError';
    this.status = status;
  }
}

const isLoopback = (h) => h === 'localhost' || h === '[::1]' || /^127(\.\d{1,3}){3}$/.test(h);

/** `TASK_STATE_INPUT_REQUIRED` -> `input-required`. */
export const normState = (s) => String(s ?? '').replace(/^TASK_STATE_/, '').toLowerCase().replace(/_/g, '-');
const TERMINAL = new Set(['completed', 'failed', 'canceled', 'rejected']);
export const REQUEST_TIMEOUT_MS = 15_000;
const EXIT_OF = { completed: 'done', failed: 'failed', canceled: 'canceled', rejected: 'rejected' };

export class A2AClient {
  #bearer;
  #fetch;
  #timeoutMs;

  constructor({ url, bearer, fetch = globalThis.fetch, requestTimeoutMs = REQUEST_TIMEOUT_MS }) {
    this.#timeoutMs = requestTimeoutMs;
    let u;
    try {
      u = new URL(url);
    } catch {
      throw new Error('the A2A url is not a URL');
    }
    if (u.username || u.password || u.search || u.hash) throw new Error('the A2A url holds credentials, a query or a fragment');
    if (u.protocol === 'http:' && !isLoopback(u.hostname)) throw new Error('the A2A url must be https or on the loopback: the bearer would cross the network in clear');
    if (typeof bearer !== 'string' || !/^[A-Za-z0-9._~+/=-]{8,}$/.test(bearer)) throw new Error('the bearer must be 8 or more plain characters');
    this.url = u.href;
    this.#bearer = bearer;
    this.#fetch = fetch;
  }

  #headers() {
    return { authorization: `Bearer ${this.#bearer}`, 'content-type': 'application/json', 'a2a-extensions': USAGE_URI };
  }

  async #post(method, params, id, signal = AbortSignal.timeout(this.#timeoutMs)) {
    const body = JSON.stringify({ jsonrpc: '2.0', id, method, params });
    try {
      return await this.#fetch(this.url, { method: 'POST', headers: this.#headers(), body, redirect: 'error', signal });
    } catch {
      throw new A2AError(`${method}: the request failed`); // no cause: a transport error may carry the headers
    }
  }

  async rpc(method, params) {
    const res = await this.#post(method, params, '1');
    if (!res.ok) throw new A2AError(`${method}: HTTP ${res.status}`, res.status);
    let body;
    try {
      body = await res.json();
    } catch {
      throw new A2AError(`${method}: the answer is not JSON`);
    }
    if (body?.error) throw new A2AError(`${method}: error ${Number.isFinite(body.error.code) ? body.error.code : 'unknown'}`, Number.isFinite(body.error.code) ? body.error.code : null);
    return body?.result ?? null;
  }

  getTask(id) {
    return this.rpc('GetTask', { id });
  }

  cancelTask(id) {
    return this.rpc('CancelTask', { id });
  }

  /**
   * Start (or, with `taskId`, continue) a task and read its stream in the background.
   * Returns { taskId: Promise<string|null>, accepted: Promise<boolean>, done: Promise<{events, usageCalls}> }.
   * Nothing here throws: a stream that fails resolves `accepted` false and `done` with what was read.
   */
  sendStreaming({ text, messageId, taskId = null, onEvent = () => {} }) {
    const message = { messageId, role: 'ROLE_USER', parts: [{ text }], ...(taskId ? { taskId } : {}) };
    const controller = new AbortController();
    const seen = { events: 0, usageCalls: new Set() };
    let resolveTask;
    let resolveAccepted;
    const task = new Promise((r) => (resolveTask = r));
    const accepted = new Promise((r) => (resolveAccepted = r));
    const handle = (frame) => {
      const result = frame?.result;
      if (frame?.error || !result) return resolveAccepted(false);
      seen.events += 1;
      resolveAccepted(true);
      const id = result.task?.id ?? result.statusUpdate?.taskId ?? result.artifactUpdate?.taskId ?? null;
      if (id) resolveTask(id);
      const call = result.statusUpdate?.metadata?.[USAGE_URI]?.call;
      if (call !== undefined) seen.usageCalls.add(String(call));
      onEvent(frame);
    };
    const done = (async () => {
      try {
        const res = await this.#post('SendStreamingMessage', { message }, '1', controller.signal);
        if (!res.ok || !res.body) return;
        if (!/text\/event-stream/i.test(res.headers?.get?.('content-type') ?? 'text/event-stream')) {
          handle(await res.json().catch(() => null));
          return;
        }
        let buffer = '';
        const decoder = new TextDecoder();
        for await (const chunk of res.body) {
          buffer += decoder.decode(chunk, { stream: true });
          let at;
          while ((at = buffer.indexOf('\n')) !== -1) {
            const line = buffer.slice(0, at).replace(/\r$/, '');
            buffer = buffer.slice(at + 1);
            if (!line.startsWith('data:')) continue;
            try {
              handle(JSON.parse(line.slice(5)));
            } catch {
              // a line that is not JSON is ignored: the stream is for latency
            }
          }
        }
      } catch {
        // the stream failing is not the task failing: polling decides
      } finally {
        resolveAccepted(false);
        resolveTask(null);
      }
      return seen;
    })();
    return { taskId: task, accepted, done, seen, abort: () => controller.abort() };
  }
}

const realSleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** `promise`, or `fallback` after `ms` of real time; the timer is cleared either way, so none is left to hold the process. */
function withTimeout(promise, ms, fallback) {
  let timer;
  const late = new Promise((resolve) => {
    timer = setTimeout(() => resolve(fallback), ms);
  });
  return Promise.race([promise, late]).finally(() => clearTimeout(timer));
}

const totalsOf = (task, uri = USAGE_URI) => task?.metadata?.[uri]?.totals;

/** What the agent says it used, from the task's totals (every model and provider added up). Shown, never trusted. */
export function selfReportedFrom(task, calls = null) {
  const totals = totalsOf(task);
  if (!Array.isArray(totals)) return calls === null ? null : { input: 0, output: 0, cached: 0, calls };
  const sum = (k) => totals.reduce((s, t) => s + (Number.isFinite(t?.[k]) ? t[k] : 0), 0);
  return { input: sum('inputTokens'), output: sum('outputTokens'), cached: sum('cachedInputTokens'), calls };
}

/**
 * Drive one task to its end. Returns
 * { exit, taskId, finalState, nudges, polls, selfReported, cancelFailed, streamEvents }.
 * `exit`: done | failed | canceled | rejected | timeout | budget_hit | stopped_by_cap | input_required | infra.
 *
 * @param {A2AClient} opts.client
 * @param {() => (null|'budget'|'cap')} [opts.refusal]  what the meter says of this run, read at every poll
 */
export async function driveTask({
  client,
  text,
  messageId,
  limitMs,
  pollMs = 10_000,
  graceMs = 60_000,
  nudgeGraceMs = 120_000,
  maxPollFailures = 6,
  refusal = () => null,
  now = Date.now,
  sleep = realSleep,
  log = () => {},
}) {
  const started = now();
  const stream = client.sendStreaming({ text, messageId });
  let taskId = null;
  let polls = 0;
  let nudges = 0;
  let nudgedTs = null;
  let nudgedAt = 0;
  let failures = 0;
  let cancelFailed = false;
  let lastTask = null;
  let decided = null; // the time to the decision, before any grace
  const mark = () => (decided ??= now() - started);
  let nudgeStream = null;
  const out = (exit, finalState = null) => ({
    elapsedMs: mark(),
    exit,
    taskId,
    finalState,
    nudges,
    polls,
    cancelFailed,
    selfReported: selfReportedFrom(lastTask, stream.seen.usageCalls.size),
    streamEvents: stream.seen.events,
    lastTask,
    ...(stream.abort(), nudgeStream?.abort(), {}),
  });

  // The task id comes from the stream's first event. No id before the limit: the run never started.
  const first = await withTimeout(stream.taskId, Math.min(limitMs, 60_000), 'late');
  if (first === 'late' || first === null) {
    log({ event: 'no_task' });
    return out('infra');
  }
  taskId = first;
  log({ event: 'started' });

  const cancel = async (exit) => {
    mark();
    try {
      await client.cancelTask(taskId);
    } catch {
      cancelFailed = true;
    }
    // Learn the state it ended in, and its totals; a cancel that is slow is waited for, up to the grace.
    const until = now() + graceMs;
    let state = null;
    for (;;) {
      try {
        lastTask = await client.getTask(taskId);
        state = normState(lastTask?.status?.state);
        if (TERMINAL.has(state)) break;
      } catch {
        // the final read failing leaves the state unknown
      }
      if (now() >= until) break;
      await sleep(Math.min(1000, Math.max(1, until - now())));
    }
    log({ event: 'cancelled', exit });
    return out(exit, state);
  };

  for (;;) {
    const elapsed = now() - started;
    if (elapsed >= limitMs) return cancel('timeout');

    let task = null;
    try {
      task = await client.getTask(taskId);
      polls += 1;
      failures = 0;
      lastTask = task;
    } catch {
      polls += 1;
      failures += 1;
      log({ event: 'poll_failed', n: failures });
      if (failures >= maxPollFailures) return out('infra');
      await sleep(Math.min(pollMs, Math.max(1, limitMs - (now() - started))));
      continue;
    }
    const state = normState(task?.status?.state);
    if (state === 'auth-required') return cancel('infra'); // nobody can sign in: the harness's, not the agent's, trouble
    const refused = refusal();
    const why = refused === 'cap' ? 'stopped_by_cap' : refused ? 'budget_hit' : null;

    if (TERMINAL.has(state)) {
      const exit = why && state !== 'completed' ? why : EXIT_OF[state];
      log({ event: 'ended', state });
      return out(exit, state);
    }
    if (why) return cancel(why);

    if (state === 'input-required') {
      const ts = task?.status?.timestamp ?? null;
      if (nudges === 0) {
        // One nudge, ever: sent once by the state, never again because an answer was slow or lost.
        nudges = 1;
        nudgedTs = ts;
        nudgedAt = now();
        nudgeStream = client.sendStreaming({ text: NUDGE, messageId: `${messageId}-nudge`, taskId });
        log({ event: 'nudged' });
      } else if (ts !== nudgedTs) {
        log({ event: 'parked_again' });
        return cancel('input_required');
      } else if (now() - nudgedAt >= nudgeGraceMs) {
        log({ event: 'nudge_unanswered' });
        return cancel('input_required');
      }
    }
    await sleep(Math.min(pollMs, Math.max(1, limitMs - (now() - started))));
  }
}
