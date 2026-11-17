// The meter: the HTTP proxy that is the only way to the model, and the truth for cost and the cap.
//
// Each run gets a token in the path (`/r/<token>/v1/chat/completions`). A request:
//   1. only `POST /r/<token>/v1/chat/completions` is served; any other path, method or an unknown
//      token is a 404 and nothing is forwarded (`/responses` is not forwarded either);
//   2. the body gets the configured model id, the arm's fragment merged over it (the fragment
//      wins), `max_tokens` and `max_completion_tokens` clamped to 8192 (and `max_tokens` set when
//      neither is present), and on a stream `stream_options.include_usage` forced;
//   3. its worst case is checked against what is left of the run's reservation (cap.mjs); if it
//      does not fit, the answer is a 429 "bench budget reached" and nothing is forwarded;
//   4. a `pending` record with the worst case is written (fsynced) before the request leaves;
//   5. the request goes upstream with the key from the environment as `Authorization: Bearer`;
//      the client's own headers are not forwarded;
//   6. the answer is relayed (an SSE stream chunk by chunk, never buffered whole) and its usage is
//      read (the last chunk of a stream); the call is settled and the actual cost written.
// A call with no usable usage is charged its worst case and flagged. A body, the key, the run
// token and the endpoint are never logged or written to the ledger.
import crypto from 'node:crypto';
import http from 'node:http';
import https from 'node:https';
import { StringDecoder } from 'node:string_decoder';
import { readKey } from './config.mjs';
import { estimateInputTokens, settleCall, worstCase } from './cost.mjs';

export const MAX_OUTPUT_TOKENS = 8192;
const PATH = /^\/r\/([A-Za-z0-9_-]{16,128})\/v1\/chat\/completions$/;
const MAX_REQUEST_BYTES = 32 * 1024 * 1024;
const MAX_RESPONSE_BYTES = 64 * 1024 * 1024;
const MAX_ERROR_BYTES = 1024 * 1024;
const MAX_SSE_LINE = 4 * 1024 * 1024;
const NEVER_CHARGED = new Set(['ECONNREFUSED', 'ENOTFOUND', 'EAI_AGAIN', 'ERR_INVALID_URL']);

const isObject = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);

/** Merge `over` onto `base`: objects merge key by key, anything else is replaced; `over` wins. */
export function deepMerge(base, over) {
  const out = { ...base };
  for (const [k, v] of Object.entries(over)) {
    if (k === '__proto__' || k === 'constructor' || k === 'prototype') continue;
    out[k] = isObject(v) && isObject(out[k]) ? deepMerge(out[k], v) : structuredClone(v);
  }
  return out;
}

const clampTokens = (v) => (typeof v === 'number' && Number.isFinite(v) && v >= 1 ? Math.min(Math.floor(v), MAX_OUTPUT_TOKENS) : MAX_OUTPUT_TOKENS);

/** The body the endpoint receives, and the most output it may produce. Throws a string on a bad body. */
export function prepareBody(body, fragment, modelId) {
  if (body.stream !== undefined && typeof body.stream !== 'boolean') throw 'stream must be a boolean';
  const out = deepMerge({ ...body, model: modelId }, fragment);
  const limits = [];
  for (const k of ['max_tokens', 'max_completion_tokens']) {
    if (k in out) {
      out[k] = clampTokens(out[k]);
      limits.push(out[k]);
    }
  }
  if (limits.length === 0) {
    out.max_tokens = MAX_OUTPUT_TOKENS;
    limits.push(MAX_OUTPUT_TOKENS);
  }
  const stream = out.stream === true;
  if (stream) out.stream_options = { ...(isObject(out.stream_options) ? out.stream_options : {}), include_usage: true };
  const n = Number.isSafeInteger(out.n) && out.n > 1 ? out.n : 1;
  return { body: out, stream, maxOutputTokens: Math.max(...limits) * n };
}

function readBody(req, limit) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let size = 0;
    req.on('data', (c) => {
      size += c.length;
      if (size > limit) {
        reject(Object.assign(new Error('too large'), { status: 413 }));
        req.destroy();
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

/** Reads `data:` lines of an SSE stream and keeps the last non-null `usage`, never the stream. */
class UsageSniffer {
  constructor() {
    this.decoder = new StringDecoder('utf8');
    this.partial = '';
    this.usage = null;
    this.gaveUp = false;
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
    if (data === '' || data === '[DONE]') return;
    try {
      const chunk = JSON.parse(data);
      if (isObject(chunk) && isObject(chunk.usage)) this.usage = chunk.usage;
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
 */
export function createMeter({ config, ledger, book, env = process.env, log = () => {} }) {
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
      parsed = JSON.parse((await readBody(req, MAX_REQUEST_BYTES)).toString('utf8'));
      if (!isObject(parsed)) throw new Error('not an object');
    } catch (e) {
      return sendJson(res, e.status === 413 ? 413 : 400, { error: { message: 'bad request' } });
    }
    let prepared;
    try {
      prepared = prepareBody(parsed, run.fragment, config.modelId);
    } catch (reason) {
      return sendJson(res, 400, { error: { message: typeof reason === 'string' ? reason : 'bad request' } });
    }
    const payload = Buffer.from(JSON.stringify(prepared.body));
    const worstMicro = worstCase(config.prices, { inputTokens: estimateInputTokens(payload.length), maxOutputTokens: prepared.maxOutputTokens });
    const call = ++run.calls;
    const base = { kind: 'call', run: run.id, attempt: run.attempt, call };

    const verdict = book.beginCall(run.id, worstMicro);
    if (!verdict.ok) {
      run.refusal = verdict.reason === 'cap' ? 'cap' : 'budget';
      ledger.append({ ...base, phase: 'refused', reason: verdict.reason, worstMicro });
      log({ event: 'refused', run: run.id, call, reason: verdict.reason });
      return sendJson(res, 429, { error: { message: 'bench budget reached', type: 'bench_budget', code: 'budget_reached' } });
    }
    try {
      ledger.append({ ...base, phase: 'pending', worstMicro, stream: prepared.stream });
    } catch {
      book.endCall(run.id, worstMicro, 0);
      return sendJson(res, 500, { error: { message: 'ledger unavailable' } });
    }
    await forward({ run, base, payload, stream: prepared.stream, worstMicro, req, res });
  }

  function forward({ run, base, payload, stream, worstMicro, req, res }) {
    return new Promise((resolve) => {
      let done = false;
      let clientGone = false;
      let upstreamReq = null;
      let upstreamRes = null;
      let timer = null;
      const clear = () => clearTimeout(timer);
      const arm = (ms) => {
        clear();
        timer = setTimeout(() => upstreamReq?.destroy(Object.assign(new Error('timeout'), { code: 'BENCH_TIMEOUT' })), ms);
      };

      // Settle exactly once: write the actual cost, free the worst case, then answer.
      const settle = ({ rawUsage, status, reason, free = false }) => {
        if (done) return false;
        done = true;
        clear();
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

      res.on('close', () => {
        if (res.writableFinished) return;
        clientGone = true;
        upstreamReq?.destroy();
        settle({ rawUsage: null, reason: 'client_abort' });
        resolve();
      });

      const options = {
        protocol: upstream.protocol,
        hostname: upstream.hostname,
        port: upstream.port || undefined,
        path: upstreamPath,
        method: 'POST',
        headers: {
          authorization: `Bearer ${key}`,
          'content-type': 'application/json',
          'content-length': payload.length,
          accept: stream ? 'text/event-stream' : 'application/json',
          'accept-encoding': 'identity',
          'user-agent': 'adam-bench-meter',
        },
      };
      upstreamReq = transport.request(options, (ur) => {
        upstreamRes = ur;
        const status = ur.statusCode ?? 502;
        const contentType = ur.headers['content-type'] ?? 'application/json';
        arm(config.timeouts.idleMs);

        if (status < 200 || status >= 300) {
          const chunks = [];
          let size = 0;
          ur.on('data', (c) => {
            arm(config.timeouts.idleMs);
            size += c.length;
            if (size <= MAX_ERROR_BYTES) chunks.push(c);
          });
          ur.on('end', () => {
            if (clientGone) return resolve();
            // An HTTP error before any answer is not billed.
            settle({ rawUsage: null, status, free: true });
            if (!res.headersSent) {
              const body = Buffer.concat(chunks);
              res.writeHead(status, { 'content-type': contentType, 'content-length': body.length });
              res.end(body);
            }
            resolve();
          });
          ur.on('error', () => fail('upstream_error'));
          return;
        }

        if (stream && /text\/event-stream/i.test(contentType)) {
          const sniffer = new UsageSniffer();
          res.writeHead(status, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
          res.flushHeaders();
          ur.on('data', (c) => {
            arm(config.timeouts.idleMs);
            sniffer.push(c);
            res.write(c);
          });
          ur.on('end', () => {
            sniffer.end();
            if (clientGone) return resolve();
            settle({ rawUsage: sniffer.usage, status });
            res.end();
            resolve();
          });
          ur.on('error', () => fail('upstream_error'));
          return;
        }

        const chunks = [];
        let size = 0;
        ur.on('data', (c) => {
          arm(config.timeouts.idleMs);
          size += c.length;
          if (size > MAX_RESPONSE_BYTES) return fail('response_too_large');
          chunks.push(c);
        });
        ur.on('end', () => {
          if (clientGone || done) return resolve();
          const body = Buffer.concat(chunks);
          let usage = null;
          try {
            const parsedBody = JSON.parse(body.toString('utf8'));
            if (isObject(parsedBody) && isObject(parsedBody.usage)) usage = parsedBody.usage;
          } catch {
            // not JSON: usage stays missing
          }
          settle({ rawUsage: usage, status });
          res.writeHead(status, { 'content-type': contentType, 'content-length': body.length });
          res.end(body);
          resolve();
        });
        ur.on('error', () => fail('upstream_error'));
      });

      // After the request was sent, a failure may still have been billed: charge the worst case.
      function fail(reason, code) {
        if (done) return resolve();
        const free = NEVER_CHARGED.has(code ?? '');
        settle({ rawUsage: null, reason: code === 'BENCH_TIMEOUT' ? 'timeout' : reason, free });
        if (!res.headersSent) sendJson(res, code === 'BENCH_TIMEOUT' ? 504 : 502, { error: { message: 'upstream unavailable' } });
        else res.destroy();
        resolve();
      }
      upstreamReq.on('error', (e) => fail('upstream_error', e.code));
      arm(config.timeouts.firstByteMs);
      upstreamReq.end(payload);
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
