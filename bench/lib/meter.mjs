// The meter: the HTTP proxy that is the only way to the model, and the truth for cost and the cap.
//
// Each run gets a token in the path (`/r/<token>/v1/chat/completions`). A request:
//   1. only `POST /r/<token>/v1/chat/completions` is served; any other path, method or an unknown
//      token is a 404 and nothing is forwarded (`/responses` is not forwarded either);
//   2. `n` must be absent or exactly 1 (one completion per call is part of fairness; anything else
//      is a 400); the body gets the configured model id, the arm's fragment merged over it (the fragment
//      wins), `max_tokens` and `max_completion_tokens` clamped to 8192 (and `max_tokens` set when
//      neither is present), and on a stream `stream_options.include_usage` forced;
//   3. its worst case is checked against what is left of the run's reservation (cap.mjs); if it
//      does not fit, the answer is a 429 "bench budget reached" and nothing is forwarded;
//   4. a `pending` record with the worst case is written (fsynced) before the request leaves;
//   5. the request goes upstream with the key from the environment as `Authorization: Bearer`;
//      the client's own headers are not forwarded;
//   6. a 2xx answer is relayed (an SSE stream chunk by chunk, never buffered whole, and the
//      upstream is paused while the client does not read) and its usage is read (the last chunk of
//      a stream); the call is settled and the actual cost written. A non-2xx answer is never
//      relayed: the agent gets a fixed body (see `upstreamError`), the call costs nothing and the
//      upstream is destroyed.
// A call with no usable usage is charged its worst case and flagged, and so is a call cut at one
// of the LIMITS. A body, the key, the run token and the endpoint are never logged or written to
// the ledger.
import crypto from 'node:crypto';
import http from 'node:http';
import https from 'node:https';
import { StringDecoder } from 'node:string_decoder';
import { readKey } from './config.mjs';
import { estimateInputTokens, settleCall, worstCase } from './cost.mjs';

export const MAX_OUTPUT_TOKENS = 8192;
const PATH = /^\/r\/([A-Za-z0-9_-]{16,128})\/v1\/chat\/completions$/;
/** Per call, named in the README. A call over a limit is cut and charged its worst case. */
export const LIMITS = Object.freeze({
  maxRequestBytes: 8 * 1024 * 1024, // a request is a conversation, not a file
  maxResponseBytes: 64 * 1024 * 1024, // the whole answer, stream or not
  maxCallMs: 20 * 60 * 1000, // from the request to the last byte, however lively the stream
});
const MAX_SSE_LINE = 4 * 1024 * 1024;
const NEVER_CHARGED = new Set(['ECONNREFUSED', 'ENOTFOUND', 'EAI_AGAIN', 'ERR_INVALID_URL']);

const isObject = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);

/**
 * A short fingerprint of a request's system prompt: the main agent keeps one through a run, and a child session (a subagent) has
 * another, so the report can tell the calls of each apart without reading them. Never the text itself. null without a system message.
 */
export function promptId(body) {
  const sys = Array.isArray(body?.messages) ? body.messages.find((m) => m?.role === 'system' || m?.role === 'developer') : null;
  if (!sys) return null;
  const text = typeof sys.content === 'string' ? sys.content : JSON.stringify(sys.content);
  return crypto.createHash('sha256').update(text).digest('hex').slice(0, 10);
}

/** Merge `over` onto `base`: objects merge key by key, anything else is replaced; `over` wins. */
export function deepMerge(base, over) {
  const out = { ...base };
  for (const [k, v] of Object.entries(over)) {
    if (k === '__proto__' || k === 'constructor' || k === 'prototype') continue;
    out[k] = isObject(v) && isObject(out[k]) ? deepMerge(out[k], v) : structuredClone(v);
  }
  return out;
}

const clampTokens = (v, cap) => (typeof v === 'number' && Number.isFinite(v) && v >= 1 ? Math.min(Math.floor(v), cap) : cap);

/** The body the endpoint receives, and the most output it may produce. Throws a string on a bad body. */
export function prepareBody(body, fragment, modelId, cap = MAX_OUTPUT_TOKENS) {
  if (body.stream !== undefined && typeof body.stream !== 'boolean') throw 'stream must be a boolean';
  if ('n' in body && body.n !== 1) throw 'n must be absent or 1: one completion per call';
  const out = deepMerge({ ...body, model: modelId, n: 1 }, fragment);
  out.n = 1;
  const limits = [];
  for (const k of ['max_tokens', 'max_completion_tokens']) {
    if (k in out) {
      out[k] = clampTokens(out[k], cap);
      limits.push(out[k]);
    }
  }
  if (limits.length === 0) {
    out.max_tokens = cap;
    limits.push(cap);
  }
  const stream = out.stream === true;
  if (stream) out.stream_options = { ...(isObject(out.stream_options) ? out.stream_options : {}), include_usage: true };
  return { body: out, stream, maxOutputTokens: Math.max(...limits) };
}

/** Count the bytes as they come; past the limit stop keeping them (the rest is read and dropped) and reject with a 413. */
function readBody(req, limit) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let size = 0;
    let over = false;
    req.on('data', (c) => {
      if (over) return;
      size += c.length;
      if (size > limit) {
        over = true;
        chunks.length = 0;
        reject(Object.assign(new Error('too large'), { status: 413 }));
        return;
      }
      chunks.push(c);
    });
    req.on('end', () => resolve(Buffer.concat(chunks)));
    req.on('error', (e) => reject(e));
    req.on('aborted', () => reject(new Error('aborted')));
  });
}

function sendJson(res, status, value) {
  if (res.headersSent) return res.destroy();
  const text = JSON.stringify(value);
  res.writeHead(status, { 'content-type': 'application/json', 'content-length': Buffer.byteLength(text) });
  res.end(text);
}

/** The status the agent sees for an upstream error: 429 stays (agents back off on it), 5xx and the odd are 502, other 4xx 400. */
const agentStatusFor = (upstream) => (upstream === 429 ? 429 : upstream >= 400 && upstream < 500 ? 400 : 502);

/** The only thing the agent learns of an upstream error: no body, header or URL of the endpoint. */
const upstreamError = (upstream) => ({ error: { message: 'upstream error', type: 'upstream_error', status: upstream } });

/**
 * The `agent` of an upstream request: a fresh connection for each call, except when a proxy is in use. Node applies
 * its env proxy (`NODE_USE_ENV_PROXY=1`, `HTTPS_PROXY`) to the default agent only, so `agent: false` would send every
 * call around the proxy; there the default agent is kept and the request says `connection: close`.
 */
export function upstreamAgent(env = process.env) {
  const proxy = env.HTTPS_PROXY ?? env.https_proxy;
  return env.NODE_USE_ENV_PROXY === '1' && proxy ? undefined : false;
}

/** After a 413 the rest of the upload is read and dropped (so the answer arrives); a client that goes on is cut after a while. */
function drainThenCut(req) {
  req.resume();
  const t = setTimeout(() => req.destroy(), 5000);
  t.unref();
  req.once('close', () => clearTimeout(t));
}

/** Longest tool name kept, in characters: a name is the upstream's text, and the ledger is not a place for it whole. */
const MAX_TOOL_NAME = 64;

/** Most tool names kept for one answer: the count is the whole of it, and a name is only for the calls seen first. */
const MAX_TOOL_NAMES = 64;

/** The tool name of a streamed or plain call, as the ledger keeps it: control and format characters dropped, cut to [`MAX_TOOL_NAME`]. */
export function toolName(name) {
  if (typeof name !== 'string') return null;
  const clean = name.replace(/[\p{Cc}\p{Cf}\u2028\u2029]/gu, '').trim();
  if (clean === '') return null;
  return clean.length > MAX_TOOL_NAME ? clean.slice(0, MAX_TOOL_NAME) : clean;
}

/** Reads `data:` lines of an SSE stream and keeps the last non-null `usage`, never the stream. */
class UsageSniffer {
  constructor() {
    this.decoder = new StringDecoder('utf8');
    this.partial = '';
    this.usage = null;
    this.callIds = new Map(); // the ids each tool call `index` of the stream has carried: a call is an index, or an id when a gateway gives every call the same index
    this.callNames = new Map(); // `index:id` (or `index:` when the call carried no id) -> its tool name, when the stream gave one
    this.done = false; // the stream said `[DONE]`: it is over, and the usage it carried is the whole of it
    this.gaveUp = false;
  }

  /** The tool calls the stream has begun: for each index the distinct ids it carried (a later fragment has none), or one when it carried none. */
  get toolCalls() {
    let n = 0;
    for (const ids of this.callIds.values()) n += Math.max(1, ids.size);
    return n;
  }

  /** The names of those calls, in the stream's order, when the stream gave them: one per call. `callNames` is capped at [`MAX_TOOL_NAMES`], so this is too. */
  get toolNames() {
    const names = [];
    for (const [index, ids] of this.callIds) {
      const keys = ids.size === 0 ? [`${index}:`] : [...ids].map((id) => `${index}:${id}`);
      for (const key of keys) {
        const name = this.callNames.get(key);
        if (name) names.push(name);
      }
    }
    return names;
  }

  push(chunk) {
    if (this.gaveUp) return;
    this.partial += this.decoder.write(chunk);
    let at;
    while ((at = this.partial.indexOf('\n')) !== -1) {
      this.line(this.partial.slice(0, at));
      this.partial = this.partial.slice(at + 1);
    }
    if (this.partial.length > MAX_SSE_LINE) {
      this.gaveUp = true;
      this.partial = '';
    }
  }

  end() {
    this.partial += this.decoder.end();
    if (this.partial) this.line(this.partial);
    this.partial = '';
  }

  line(text) {
    const line = text.replace(/\r$/, '');
    if (!line.startsWith('data:')) return;
    const data = line.slice(5).trim();
    if (data === '[DONE]') this.done = true;
    if (data === '' || data === '[DONE]') return;
    try {
      const chunk = JSON.parse(data);
      if (isObject(chunk) && isObject(chunk.usage)) this.usage = chunk.usage;
      for (const choice of Array.isArray(chunk?.choices) ? chunk.choices : []) {
        const calls = choice?.delta?.tool_calls;
        if (!Array.isArray(calls)) continue;
        for (const c of calls) {
          if (!Number.isInteger(c?.index) || c.index < 0) continue;
          const ids = this.callIds.get(c.index) ?? new Set();
          const id = typeof c.id === 'string' && c.id !== '' ? c.id : null;
          if (id) ids.add(id);
          this.callIds.set(c.index, ids);
          const name = toolName(c?.function?.name);
          if (name && this.callNames.size < MAX_TOOL_NAMES) {
            // A later fragment carries no id: its name (when it repeats one) belongs to the last id of that index.
            const known = id ?? [...ids].at(-1) ?? '';
            this.callNames.set(`${c.index}:${known}`, name);
          }
          if (id && this.callNames.size < MAX_TOOL_NAMES) {
            // A name can come before the id it belongs to (a gateway that opens the call, then names it).
            const pending = this.callNames.get(`${c.index}:`);
            if (pending && !this.callNames.has(`${c.index}:${id}`)) {
              this.callNames.set(`${c.index}:${id}`, pending);
            }
          }
        }
      }
    } catch {
      // not JSON: ignored
    }
  }
}

/**
 * @param {object} opts
 * @param {object} opts.config  normalised configuration (config.mjs)
 * @param {object} opts.ledger  an open Ledger
 * @param {object} opts.book    a CapBook
 * @param {object} [opts.env]   where the key is read from
 * @param {(event: object) => void} [opts.log]  receives fixed-shape events: no body, key or URL
 * @param {object} [opts.limits]  overrides of LIMITS (tests)
 */
export function createMeter({ config, ledger, book, env = process.env, log = () => {}, limits: limitOverrides = {} }) {
  const limits = { ...LIMITS, ...limitOverrides };
  const key = readKey(config, env);
  const upstream = new URL(config.endpoint);
  const transport = upstream.protocol === 'https:' ? https : http;
  const upstreamPath = `${upstream.pathname.replace(/\/+$/, '')}/chat/completions`;
  const runs = new Map(); // token -> run
  const byId = new Map(); // run id -> run
  let broken = false;

  const server = http.createServer((req, res) => {
    handle(req, res).catch((e) => {
      log({ event: 'error', code: e?.code ?? 'internal' });
      sendJson(res, 500, { error: { message: 'meter error' } });
    });
  });
  server.requestTimeout = 120_000;
  server.headersTimeout = 30_000;
  server.keepAliveTimeout = 5_000;

  async function handle(req, res) {
    if (broken) {
      req.resume();
      return sendJson(res, 503, { error: { message: 'meter stopped' } });
    }
    const m = PATH.exec(req.url ?? '');
    const run = m ? runs.get(m[1]) : undefined;
    if (req.method !== 'POST' || !m || !run) {
      req.resume();
      return sendJson(res, 404, { error: { message: 'not found' } });
    }
    let parsed;
    try {
      const declared = Number(req.headers['content-length']);
      if (Number.isFinite(declared) && declared > limits.maxRequestBytes) throw Object.assign(new Error('too large'), { status: 413 });
      parsed = JSON.parse((await readBody(req, limits.maxRequestBytes)).toString('utf8'));
      if (!isObject(parsed)) throw new Error('not an object');
    } catch (e) {
      if (e.status === 413) {
        sendJson(res, 413, { error: { message: 'request too large' } });
        return drainThenCut(req);
      }
      return sendJson(res, 400, { error: { message: 'bad request' } });
    }
    let prepared;
    try {
      // `limits.outputTokens` (default 8192), the same for every agent.
      prepared = prepareBody(parsed, run.fragment, config.modelId, config.limits?.outputTokens ?? MAX_OUTPUT_TOKENS);
    } catch (reason) {
      return sendJson(res, 400, { error: { message: typeof reason === 'string' ? reason : 'bad request' } });
    }
    const payload = Buffer.from(JSON.stringify(prepared.body));
    const worstMicro = worstCase(config.prices, { inputTokens: estimateInputTokens(payload.length), maxOutputTokens: prepared.maxOutputTokens });
    const callNo = ++run.calls;
    const base = { kind: 'call', run: run.id, attempt: run.attempt, call: callNo };

    const verdict = book.beginCall(run.id, worstMicro);
    if (!verdict.ok) {
      run.refusal = verdict.reason === 'cap' ? 'cap' : 'budget';
      ledger.append({ ...base, phase: 'refused', reason: verdict.reason, worstMicro });
      log({ event: 'refused', run: run.id, call: callNo, reason: verdict.reason });
      return sendJson(res, 429, { error: { message: 'bench budget reached', type: 'bench_budget', code: 'budget_reached' } });
    }
    try {
      // What was sent, for the report: the effective reasoning setting (the arm's fragment wins over the agent's own, which is kept
      // too), the size of the request and how many tools it offered.
      const sent = { effort: prepared.body.reasoning_effort ?? null, askedEffort: parsed.reasoning_effort ?? null, bytes: payload.length, tools: Array.isArray(prepared.body.tools) ? prepared.body.tools.length : 0, prompt: promptId(prepared.body) };
      ledger.append({ ...base, phase: 'pending', worstMicro, stream: prepared.stream, ...sent });
    } catch {
      book.endCall(run.id, worstMicro, 0);
      return sendJson(res, 500, { error: { message: 'ledger unavailable' } });
    }
    const call = makeSettler({ run, base, worstMicro });
    try {
      await forward({ call, payload, stream: prepared.stream, req, res });
    } catch (e) {
      // forward answers its own failures; if one still escapes, the call must not stay pending
      call.settle({ rawUsage: null, reason: 'internal', free: !call.sent });
      throw e;
    }
  }

  /** One settlement per call: write the actual cost, free the worst case. `sent` says the request left. */
  function makeSettler({ run, base, worstMicro }) {
    const call = { done: false, sent: false, worstMicro, base, run };
    call.settle = ({ rawUsage, status, reason, free = false, toolCalls, toolNames }) => {
      if (call.done) return false;
      call.done = true;
      let outcome;
      if (free) outcome = { costMicro: 0, usage: null, usageMissing: false };
      else outcome = settleCall({ prices: config.prices, rawUsage, worstMicro });
      try {
        ledger.append({
          ...base,
          phase: free ? 'failed' : 'settled',
          costMicro: outcome.costMicro,
          usage: outcome.usage,
          usageMissing: outcome.usageMissing,
          ...(status === undefined ? {} : { status }),
          ...(reason ? { reason } : {}),
          // How many tool calls the answer asked for (calls per model turn): only when the answer was read.
          ...(!free && Number.isInteger(toolCalls) ? { toolCalls } : {}),
          // And their names, for the calls by tool: only the ones the answer named.
          ...(!free && Array.isArray(toolNames) && toolNames.length > 0 ? { toolNames } : {}),
        });
      } catch {
        // The truth cannot be written: stop serving rather than spend unrecorded.
        broken = true;
        log({ event: 'ledger_error' });
      } finally {
        book.endCall(run.id, worstMicro, outcome.costMicro);
      }
      log({ event: 'call', run: run.id, call: base.call, status: status ?? 0, costMicro: outcome.costMicro, usageMissing: outcome.usageMissing });
      return true;
    };
    return call;
  }

  function forward({ call, payload, stream, req, res }) {
    return new Promise((resolve) => {
      let clientGone = false;
      let upstreamReq = null;
      let cut = null; // why the meter itself ended the call: 'timeout' or 'response_too_large'
      let received = 0;
      let idleTimer = null;
      let callTimer = null;
      let sniffer = null; // what the stream said about usage so far: a client that closes at [DONE] is charged by it
      const clear = () => {
        clearTimeout(idleTimer);
        clearTimeout(callTimer);
      };
      const cutNow = (reason) => {
        cut ??= reason;
        upstreamReq?.destroy(Object.assign(new Error(reason), { code: 'BENCH_CUT' }));
      };
      const arm = (ms) => {
        clearTimeout(idleTimer);
        idleTimer = setTimeout(() => cutNow('timeout'), ms);
      };
      const settle = (outcome) => {
        const first = call.settle(outcome);
        if (first) clear();
        return first;
      };

      res.on('close', () => {
        if (res.writableFinished) return;
        clientGone = true;
        upstreamReq?.destroy();
        sniffer?.end();
        if (sniffer?.done && sniffer.usage) settle({ rawUsage: sniffer.usage, status: 200, toolCalls: sniffer.toolCalls, toolNames: sniffer.toolNames });
        else settle({ rawUsage: null, reason: 'client_abort' });
        resolve();
      });

      // After the request was sent, a failure may still have been billed: charge the worst case.
      function fail(reason, code) {
        if (call.done) return resolve();
        const free = !cut && NEVER_CHARGED.has(code ?? '');
        settle({ rawUsage: null, reason: cut ?? reason, free });
        if (!res.headersSent) sendJson(res, cut === 'timeout' ? 504 : 502, { error: { message: 'upstream unavailable' } });
        else res.destroy();
        resolve();
      }

      try {
        const options = {
          protocol: upstream.protocol,
          hostname: upstream.hostname,
          port: upstream.port || undefined,
          path: upstreamPath,
          method: 'POST',
          // One connection for each call, without losing the proxy: an endpoint or a proxy that closes an idle socket
          // (Node's own default is 5 s) must not turn the next call into a failed one charged its worst case. Node
          // applies its env proxy (`NODE_USE_ENV_PROXY=1`) only to the default agent, so `agent: false` would go round
          // it; with a proxy the default agent is kept and `connection: close` ends each socket after its call.
          agent: upstreamAgent(env),
          headers: {
            authorization: `Bearer ${key}`,
            'content-type': 'application/json',
            'content-length': payload.length,
            accept: stream ? 'text/event-stream' : 'application/json',
            'accept-encoding': 'identity',
            connection: 'close',
            'user-agent': 'adam-bench-meter',
          },
        };
        upstreamReq = transport.request(options, (ur) => {
          ur.on('error', () => fail('upstream_error'));
          const status = ur.statusCode ?? 502;
          const contentType = ur.headers['content-type'] ?? 'application/json';
          arm(config.timeouts.idleMs);

          if (status < 200 || status >= 300) {
            // Never relayed, never read, never billed: a truncated error body costs 0, as any non-2xx.
            if (clientGone) return resolve();
            settle({ rawUsage: null, status, free: true });
            if (!res.headersSent) {
              const text = JSON.stringify(upstreamError(status));
              res.writeHead(agentStatusFor(status), { 'content-type': 'application/json', 'content-length': Buffer.byteLength(text) });
              res.end(text);
            }
            upstreamReq.destroy();
            return resolve();
          }

          const count = (c) => {
            arm(config.timeouts.idleMs);
            received += c.length;
            if (received > limits.maxResponseBytes) {
              cutNow('response_too_large');
              return false;
            }
            return true;
          };

          if (stream && /text\/event-stream/i.test(contentType)) {
            sniffer = new UsageSniffer();
            res.writeHead(status, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
            res.flushHeaders();
            ur.on('data', (c) => {
              if (!count(c)) return;
              sniffer.push(c);
              if (!res.write(c)) {
                ur.pause();
                res.once('drain', () => ur.resume());
              }
            });
            ur.on('end', () => {
              sniffer.end();
              if (clientGone) return resolve();
              settle({ rawUsage: sniffer.usage, status, toolCalls: sniffer.toolCalls, toolNames: sniffer.toolNames });
              res.end();
              resolve();
            });
            return;
          }

          const chunks = [];
          ur.on('data', (c) => {
            if (count(c)) chunks.push(c);
          });
          ur.on('end', () => {
            if (clientGone || call.done) return resolve();
            const body = Buffer.concat(chunks);
            let usage = null;
            let toolCalls;
            let toolNames;
            try {
              const parsedBody = JSON.parse(body.toString('utf8'));
              if (isObject(parsedBody) && isObject(parsedBody.usage)) usage = parsedBody.usage;
              const choice = Array.isArray(parsedBody?.choices) ? parsedBody.choices[0] : null;
              if (isObject(choice?.message) && Array.isArray(choice.message.tool_calls)) {
                toolCalls = choice.message.tool_calls.length;
                toolNames = choice.message.tool_calls.map((c) => toolName(c?.function?.name)).filter((n) => n !== null).slice(0, MAX_TOOL_NAMES);
              } else if (isObject(choice?.message)) {
                toolCalls = 0;
              }
            } catch {
              // not JSON: usage stays missing
            }
            settle({ rawUsage: usage, status, toolCalls, toolNames });
            res.writeHead(status, { 'content-type': contentType, 'content-length': body.length });
            res.end(body);
            resolve();
          });
        });
        upstreamReq.on('error', (e) => fail('upstream_error', e.code));
        arm(config.timeouts.firstByteMs);
        callTimer = setTimeout(() => cutNow('timeout'), limits.maxCallMs);
        upstreamReq.end(payload);
        call.sent = true;
      } catch (e) {
        // Thrown before anything left (a header the transport refuses, say): nothing was billed.
        log({ event: 'error', code: e?.code ?? 'internal' });
        upstreamReq?.destroy();
        settle({ rawUsage: null, reason: 'internal', free: true });
        sendJson(res, 500, { error: { message: 'meter error' } });
        resolve();
      }
    });
  }

  let address = null;
  return {
    server,
    /** Listen on loopback by default. Resolves to the port. */
    listen: (port = 0, host = '127.0.0.1') =>
      new Promise((resolve, reject) => {
        server.once('error', reject);
        server.listen(port, host, () => {
          address = { host, port: server.address().port };
          resolve(address);
        });
      }),
    /** Open a run: its token goes in the base URL the agent is configured with. */
    openRun({ run, attempt = 1, arm: armName }) {
      const fragment = config.arms[armName];
      if (!fragment) throw new Error(`no such arm: ${armName}`);
      if (byId.has(run) && !byId.get(run).closed) throw new Error(`run ${run} is already open`);
      if (!address) throw new Error('the meter is not listening');
      const token = crypto.randomBytes(24).toString('base64url');
      const entry = { id: run, attempt, fragment, calls: 0, refusal: null, closed: false };
      runs.set(token, entry);
      byId.set(run, entry);
      return { token, baseUrl: `http://${address.host}:${address.port}/r/${token}/v1` };
    },
    /** Close a run: its token stops working. */
    closeRun(run) {
      const entry = byId.get(run);
      if (entry) {
        entry.closed = true;
        for (const [token, e] of runs) if (e === entry) runs.delete(token);
      }
      book.close(run);
    },
    /** What the meter saw of a run: calls made, and whether it refused one (budget or global cap). */
    status(run) {
      const e = byId.get(run);
      return e ? { calls: e.calls, refusal: e.refusal, closed: e.closed } : null;
    },
    close: () =>
      new Promise((resolve) => {
        server.closeAllConnections();
        server.close(() => resolve());
      }),
  };
}
