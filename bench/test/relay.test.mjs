import assert from 'node:assert/strict';
import net from 'node:net';
import { test } from 'node:test';
import { parseTarget, startRelay } from '../lib/relay.mjs';

const listen = (handler) =>
  new Promise((resolve) => {
    const s = net.createServer(handler);
    s.listen(0, '127.0.0.1', () => resolve(s));
  });

const talk = (port, text) =>
  new Promise((resolve, reject) => {
    const c = net.connect(port, '127.0.0.1', () => c.write(text));
    const chunks = [];
    c.on('data', (d) => chunks.push(d));
    c.on('close', () => resolve(Buffer.concat(chunks).toString()));
    c.on('error', reject);
    setTimeout(() => c.end(), 100);
  });

test('the relay forwards the bytes of a connection to its one target and the answer back', async (t) => {
  const seen = [];
  const target = await listen((s) => {
    s.on('data', (d) => {
      seen.push(d.toString());
      s.write(`echo:${d}`);
    });
  });
  t.after(() => target.close());
  const relay = await startRelay({ listenPort: 0, listenHost: '127.0.0.1', target: { host: '127.0.0.1', port: target.address().port } });
  t.after(() => relay.close());
  assert.equal(await talk(relay.address().port, 'hello'), 'echo:hello');
  assert.deepEqual(seen, ['hello']);
});

test('a target that is down closes the connection and reports the code, never the address', async (t) => {
  const errors = [];
  const free = await listen(() => {});
  const port = free.address().port;
  await new Promise((r) => free.close(r));
  const relay = await startRelay({ listenPort: 0, listenHost: '127.0.0.1', target: { host: '127.0.0.1', port }, onError: (c) => errors.push(c) });
  t.after(() => relay.close());
  assert.equal(await talk(relay.address().port, 'x'), '');
  assert.deepEqual(errors, ['ECONNREFUSED']);
});

test('an idle connection is closed after the idle time, and no more than the cap are held', async (t) => {
  const target = await listen(() => {});
  t.after(() => target.close());
  const relay = await startRelay({ listenPort: 0, listenHost: '127.0.0.1', target: { host: '127.0.0.1', port: target.address().port }, idleMs: 300, maxConnections: 2 });
  t.after(() => relay.close());
  const port = relay.address().port;
  const closed = (c) => new Promise((resolve) => c.on('close', () => resolve(true)));
  const within = (p, ms) => Promise.race([p, new Promise((r) => setTimeout(() => r(false), ms))]);
  const a = net.connect(port, '127.0.0.1');
  assert.equal(await within(closed(a), 1500), true, 'the idle connection was closed');
  const held = [net.connect(port, '127.0.0.1'), net.connect(port, '127.0.0.1')];
  await new Promise((r) => setTimeout(r, 10));
  const third = net.connect(port, '127.0.0.1');
  third.on('error', () => {});
  assert.equal(await within(closed(third), 150), true, 'a third connection over the cap is closed');
  for (const c of held) c.destroy();
});

test('the target is host:port and nothing else', () => {
  assert.deepEqual(parseTarget('host.docker.internal:18090'), { host: 'host.docker.internal', port: 18090 });
  for (const bad of [undefined, '', 'host', 'host:0', 'host:70000', 'a b:1', 'http://x:1', 'host:1/path']) assert.throws(() => parseTarget(bad), /host:port/, String(bad));
});
