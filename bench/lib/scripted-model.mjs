// An OpenAI-compatible server for tests: stateless, fixed usage numbers, no model behind it.
// POST /v1/chat/completions answers a plain JSON completion, or an SSE stream when the request
// says `stream: true` (the last chunk, with empty `choices`, carries `usage`, as
// `stream_options.include_usage` asks). Behaviour is switched through `model.behavior`.
import http from 'node:http';

export const FIXED_USAGE = Object.freeze({ prompt: 1000, completion: 200, cached: 400, reasoning: 50, cacheWrite: 0 });

export function createScriptedModel(initial = {}) {
  const behavior = {
    usage: { ...FIXED_USAGE },
    omitUsage: false, // leave `usage` out of the answer (and of the last chunk)
    delayMs: 0, // before the first byte
    chunkDelayMs: 0, // between the chunks of a stream
    status: 200, // a status other than 200 answers an error body
    expectKey: null, // when set, a request without `Authorization: Bearer <it>` gets 401
    ...initial,
  };
  const requests = [];
  const timers = new Set();

  const usageObject = () => ({
    prompt_tokens: behavior.usage.prompt,
    completion_tokens: behavior.usage.completion,
    total_tokens: behavior.usage.prompt + behavior.usage.completion,
    prompt_tokens_details: { cached_tokens: behavior.usage.cached, cache_write_tokens: behavior.usage.cacheWrite },
    completion_tokens_details: { reasoning_tokens: behavior.usage.reasoning },
  });

  const server = http.createServer((req, res) => {
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      let body = null;
      try {
        body = JSON.parse(Buffer.concat(chunks).toString('utf8'));
      } catch {
        // not JSON: recorded as null
      }
      requests.push({ method: req.method, url: req.url, headers: { ...req.headers }, body });
      const json = (status, value) => {
        const text = JSON.stringify(value);
        res.writeHead(status, { 'content-type': 'application/json', 'content-length': Buffer.byteLength(text) });
        res.end(text);
      };
      if (req.method !== 'POST' || req.url !== '/v1/chat/completions') return json(404, { error: { message: 'not found' } });
      if (behavior.expectKey && req.headers.authorization !== `Bearer ${behavior.expectKey}`) {
        return json(401, { error: { message: 'bad key' } });
      }
      const respond = () => {
        if (behavior.status !== 200) return json(behavior.status, { error: { message: 'scripted failure' } });
        const base = { id: 'chatcmpl-scripted', object: 'chat.completion', created: 1, model: body?.model ?? 'unknown' };
        if (body?.stream !== true) {
          const out = { ...base, choices: [{ index: 0, message: { role: 'assistant', content: 'done' }, finish_reason: 'stop' }] };
          if (!behavior.omitUsage) out.usage = usageObject();
          return json(200, out);
        }
        res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
        const frames = [
          { ...base, object: 'chat.completion.chunk', choices: [{ index: 0, delta: { role: 'assistant', content: 'do' } }], usage: null },
          { ...base, object: 'chat.completion.chunk', choices: [{ index: 0, delta: { content: 'ne' }, finish_reason: 'stop' }], usage: null },
        ];
        if (!behavior.omitUsage && body?.stream_options?.include_usage === true) {
          frames.push({ ...base, object: 'chat.completion.chunk', choices: [], usage: usageObject() });
        }
        let i = 0;
        const next = () => {
          if (i < frames.length) {
            res.write(`data: ${JSON.stringify(frames[i++])}\n\n`);
            const t = setTimeout(() => {
              timers.delete(t);
              next();
            }, behavior.chunkDelayMs);
            return timers.add(t);
          }
          res.write('data: [DONE]\n\n');
          res.end();
        };
        next();
      };
      if (behavior.delayMs > 0) {
        const t = setTimeout(() => {
          timers.delete(t);
          respond();
        }, behavior.delayMs);
        timers.add(t);
      } else respond();
    });
  });

  return {
    behavior,
    requests,
    server,
    listen: () =>
      new Promise((resolve) => {
        server.listen(0, '127.0.0.1', () => resolve({ port: server.address().port, url: `http://127.0.0.1:${server.address().port}/v1` }));
      }),
    close: () =>
      new Promise((resolve) => {
        for (const t of timers) clearTimeout(t);
        server.closeAllConnections();
        server.close(() => resolve());
      }),
  };
}
