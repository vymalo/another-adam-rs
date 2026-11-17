// A stand-in for the coder's A2A endpoint: a real HTTP server that answers JSON-RPC as the script says.
// `script` is one entry per GetTask, the last one repeating: { state, ts?, totals? }. A new task's stream
// carries the task and then ends (or stays open with `holdStream`), as a client that decides by polling sees it.
import http from 'node:http';

export const USAGE = 'https://agents.vymalo.com/a2a/extensions/usage/v1';
export const TASK_ID = 'task-1';

export async function fakeCoder({ script, bearer = 'test-bearer-0001', holdStream = false, getTaskStatus = null, cancelTo = 'canceled', failSend = false, hangGetTask = false, followUpHang = false }) {
  const calls = [];
  const sockets = new Set();
  let polls = 0;
  let cancelled = false;
  const server = http.createServer((req, res) => {
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      let rpc = null;
      try {
        rpc = JSON.parse(Buffer.concat(chunks).toString('utf8'));
      } catch {
        // not JSON
      }
      const auth = req.headers.authorization;
      calls.push({ method: rpc?.method, params: rpc?.params, auth, extensions: req.headers['a2a-extensions'], url: req.url });
      const json = (status, value) => {
        const text = JSON.stringify(value);
        res.writeHead(status, { 'content-type': 'application/json', 'content-length': Buffer.byteLength(text) });
        res.end(text);
      };
      if (auth !== `Bearer ${bearer}`) return json(401, { error: 'unauthenticated' });
      const ok = (result) => json(200, { jsonrpc: '2.0', id: rpc.id, result });
      if (rpc.method === 'GetTask') {
        if (hangGetTask) return; // accepts the connection and never answers
        if (getTaskStatus) {
          const s = getTaskStatus(polls++);
          if (s !== 200) return json(s, { error: 'boom' });
        } else polls++;
        const entry = cancelled ? { state: cancelTo } : script[Math.min(polls - 1, script.length - 1)];
        const task = {
          id: TASK_ID,
          contextId: 'ctx-1',
          status: { state: `TASK_STATE_${entry.state.toUpperCase().replace(/-/g, '_')}`, timestamp: entry.ts ?? '2026-11-17T10:00:00Z' },
        };
        if (entry.totals) task.metadata = { [USAGE]: { totals: entry.totals } };
        return ok(task);
      }
      if (rpc.method === 'CancelTask') {
        cancelled = true;
        return ok({ id: TASK_ID, status: { state: 'TASK_STATE_CANCELED' } });
      }
      if (rpc.method === 'SendStreamingMessage') {
        if (followUpHang && rpc.params.message.taskId) return; // the nudge is delivered, its answer is slow
        if (failSend && rpc.params.message.taskId) return json(200, { jsonrpc: '2.0', id: rpc.id, error: { code: -32602, message: 'nope' } });
        res.writeHead(200, { 'content-type': 'text/event-stream' });
        const frame = (result) => res.write(`data: ${JSON.stringify({ jsonrpc: '2.0', id: rpc.id, result })}\n\n`);
        frame({ task: { id: TASK_ID, contextId: 'ctx-1', status: { state: 'TASK_STATE_SUBMITTED' } } });
        frame({ statusUpdate: { taskId: TASK_ID, status: { state: 'TASK_STATE_WORKING', metadata: undefined }, metadata: { [USAGE]: { call: 'c1', inputTokens: 10, outputTokens: 2 } } } });
        if (!holdStream) res.end();
        return;
      }
      return json(404, { error: 'no such method' });
    });
  });
  server.on('connection', (s) => {
    sockets.add(s);
    s.on('close', () => sockets.delete(s));
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  return {
    url: `http://127.0.0.1:${server.address().port}/`,
    bearer,
    calls,
    polls: () => polls,
    of: (method) => calls.filter((c) => c.method === method),
    close: () =>
      new Promise((resolve) => {
        for (const s of sockets) s.destroy();
        server.close(() => resolve());
      }),
  };
}

/** A clock that only moves when `sleep` is called, and a sleep that lets the event loop run. */
export function fakeClock(start = 1_000_000) {
  let t = start;
  return {
    now: () => t,
    sleep: async (ms) => {
      t += ms;
      await new Promise((resolve) => setImmediate(resolve));
    },
    elapsed: () => t - start,
  };
}
