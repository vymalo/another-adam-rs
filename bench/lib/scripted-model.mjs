// An OpenAI-compatible server for tests: stateless, fixed usage numbers, no model behind it.
// POST /v1/chat/completions answers a plain JSON completion, or an SSE stream when the request
// says `stream: true` (the last chunk, with empty `choices`, carries `usage`, as
// `stream_options.include_usage` asks). Behaviour is switched through `model.behavior`.
import http from 'node:http';
import { nextTurn } from './agent-script.mjs';

export const FIXED_USAGE = Object.freeze({ prompt: 1000, completion: 200, cached: 400, reasoning: 50, cacheWrite: 0 });

export function createScriptedModel(initial = {}) {
  const behavior = {
    usage: { ...FIXED_USAGE },
    omitUsage: false, // leave `usage` out of the answer (and of the last chunk)
    delayMs: 0, // before the first byte
    chunkDelayMs: 0, // between the chunks of a stream
    status: 200, // a status other than 200 answers an error body
    expectKey: null, // when set, a request without `Authorization: Bearer <it>` gets 401
    errorBody: null, // the body of a non-200 answer (a string, or a value sent as JSON); default a short error object
    errorHeaders: {}, // extra headers of a non-200 answer
    okHeaders: {}, // extra headers of a 200 answer (plain and stream)
    reset: false, // destroy the socket after reading the request
    onRequest: null, // called with the recorded request when it arrives
    padBytes: 0, // a plain answer's content is this long
    script: null, // 'agents': drive adam and OpenCode through the self-test tasks (agent-script.mjs)
    endless: false, // a stream that never ends: `endlessChunkBytes` of text every `endlessEveryMs`, honouring backpressure
    endlessChunkBytes: 100,
    endlessEveryMs: 5,
    holdBeforeDoneMs: 0, // the usage chunk is sent, then this long passes before `[DONE]` (a client that aborts in it aborts before the end)
    toolCalls: 0, // an answer that asks for this many tool calls at once (a number above 0), whatever the script says
    splitToolCalls: false, // a stream sends each tool call in a chunk of its own and the rest of its arguments in another, as OpenAI does
    sameIndexToolCalls: false, // a stream gives every tool call the index 0 and tells them apart by their ids, as some gateways do
    noToolCallIds: false, // a stream's tool calls carry no `id`, only their index
    okBody: null, // a 200 plain answer with exactly this body (a string), not a completion
    holdAfterDoneMs: 0, // a stream ends this long after `[DONE]` (a client that closes at `[DONE]` closes first)
    ...initial,
  };
  const requests = [];
  const timers = new Set();
  const stats = { sentBytes: 0, aborted: 0 }; // bytes an endless stream got into the socket; streams the client cut off

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
      behavior.onRequest?.(requests.at(-1));
      if (behavior.reset) return req.socket.destroy();
      const json = (status, value) => {
        const text = JSON.stringify(value);
        res.writeHead(status, { 'content-type': 'application/json', 'content-length': Buffer.byteLength(text), ...(status === 200 ? behavior.okHeaders : {}) });
        res.end(text);
      };
      if (req.method !== 'POST' || req.url !== '/v1/chat/completions') return json(404, { error: { message: 'not found' } });
      if (behavior.expectKey && req.headers.authorization !== `Bearer ${behavior.expectKey}`) {
        return json(401, { error: { message: 'bad key' } });
      }
      const respond = () => {
        if (behavior.status !== 200) {
          const body = behavior.errorBody ?? { error: { message: 'scripted failure' } };
          const text = typeof body === 'string' ? body : JSON.stringify(body);
          res.writeHead(behavior.status, { 'content-type': 'application/json', 'content-length': Buffer.byteLength(text), ...behavior.errorHeaders });
          return res.end(text);
        }
        if (typeof behavior.okBody === 'string' && body?.stream !== true) {
          res.writeHead(200, { 'content-type': 'application/json', 'content-length': Buffer.byteLength(behavior.okBody) });
          return res.end(behavior.okBody);
        }
        const base = { id: 'chatcmpl-scripted', object: 'chat.completion', created: 1, model: body?.model ?? 'unknown' };
        const turn = behavior.script === 'agents' ? nextTurn(body) : null;
        const several = behavior.toolCalls > 0 ? Array.from({ length: behavior.toolCalls }, (_, i) => ({ id: `call-${i + 1}`, type: 'function', function: { name: 'probe', arguments: JSON.stringify({ n: i + 1 }) } })) : null;
        const toolCall = turn?.tool ? [{ id: turn.tool.id, type: 'function', function: { name: turn.tool.name, arguments: JSON.stringify(turn.tool.args) } }] : several;
        if (body?.stream !== true) {
          const message = toolCall ? { role: 'assistant', content: null, tool_calls: toolCall } : { role: 'assistant', content: turn ? turn.text : 'done'.padEnd(behavior.padBytes, 'x') };
          const out = { ...base, choices: [{ index: 0, message, finish_reason: toolCall ? 'tool_calls' : 'stop' }] };
          if (!behavior.omitUsage) out.usage = usageObject();
          return json(200, out);
        }
        res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache', ...behavior.okHeaders });
        res.on('close', () => {
          if (!res.writableFinished) stats.aborted += 1;
        });
        const chunk = (choice) => ({ ...base, object: 'chat.completion.chunk', choices: [{ index: 0, ...choice }], usage: null });
        const toolCallFrames = (given) => {
          const calls = behavior.noToolCallIds ? given.map(({ id, ...c }) => c) : given;
          return behavior.splitToolCalls
            ? calls.flatMap((c, n) => {
                const index = behavior.sameIndexToolCalls ? 0 : n;
                const args = c.function.arguments;
                const half = Math.ceil(args.length / 2);
                return [
                  chunk({ delta: { ...(n === 0 ? { role: 'assistant' } : {}), tool_calls: [{ index, id: c.id, type: c.type, function: { name: c.function.name, arguments: args.slice(0, half) } }] } }),
                  chunk({ delta: { tool_calls: [{ index, function: { arguments: args.slice(half) } }] } }),
                ];
              })
            : [chunk({ delta: { role: 'assistant', tool_calls: calls.map((c, n) => ({ index: behavior.sameIndexToolCalls ? 0 : n, ...c })) } })];
        };
        const frames = toolCall
          ? [...toolCallFrames(toolCall), chunk({ delta: {}, finish_reason: 'tool_calls' })]
          : turn
            ? [chunk({ delta: { role: 'assistant', content: turn.text } }), chunk({ delta: {}, finish_reason: 'stop' })]
            : [chunk({ delta: { role: 'assistant', content: 'do' } }), chunk({ delta: { content: 'ne' }, finish_reason: 'stop' })];
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
          if (behavior.endless) return endless();
          if (behavior.holdBeforeDoneMs > 0 && !res.beforeDoneHeld) {
            res.beforeDoneHeld = true;
            const h = setTimeout(() => {
              timers.delete(h);
              next();
            }, behavior.holdBeforeDoneMs);
            return timers.add(h);
          }
          res.write('data: [DONE]\n\n');
          if (behavior.holdAfterDoneMs > 0) {
            const h = setTimeout(() => {
              timers.delete(h);
              res.end();
            }, behavior.holdAfterDoneMs);
            timers.add(h);
          } else res.end();
        };
        const endless = () => {
          if (res.destroyed) return;
          const text = `data: ${JSON.stringify({ choices: [{ index: 0, delta: { content: 'x'.repeat(behavior.endlessChunkBytes) } }], usage: null })}\n\n`;
          const ok = res.write(text, () => {
            stats.sentBytes += text.length;
          });
          const again = () => {
            const t = setTimeout(() => {
              timers.delete(t);
              endless();
            }, behavior.endlessEveryMs);
            timers.add(t);
          };
          if (ok) again();
          else res.once('drain', again);
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
    stats,
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
