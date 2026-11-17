// A TCP relay with one fixed target: the `meter` service of bench/compose.yaml. The harness's meter listens on the
// host; the stack's internal networks have no route there, so this process, the only member of both, forwards
// the bytes of a connection to the one address it was started with and to nothing else. It reads no byte of them.
//
//   node relay.mjs            RELAY_PORT (listen, default 8090), RELAY_TARGET (host:port, required)
import net from 'node:net';
import { fileURLToPath } from 'node:url';

/** Listen on `listenPort` and relay every connection to `target` ({ host, port }). Resolves to the server. */
export function startRelay({ listenPort, target, listenHost = '0.0.0.0', onError = () => {} }) {
  return new Promise((resolve, reject) => {
    const server = net.createServer((client) => {
      const upstream = net.connect({ host: target.host, port: target.port });
      const end = () => {
        client.destroy();
        upstream.destroy();
      };
      client.on('error', end);
      upstream.on('error', (e) => {
        onError(e.code ?? 'error');
        end();
      });
      client.on('close', end);
      upstream.on('close', end);
      client.pipe(upstream);
      upstream.pipe(client);
    });
    server.once('error', reject);
    server.listen(listenPort, listenHost, () => resolve(server));
  });
}

export function parseTarget(text) {
  const m = /^([A-Za-z0-9._-]+):(\d{1,5})$/.exec(text ?? '');
  if (!m || Number(m[2]) < 1 || Number(m[2]) > 65535) throw new Error('RELAY_TARGET must be host:port');
  return { host: m[1], port: Number(m[2]) };
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const target = parseTarget(process.env.RELAY_TARGET);
  const port = Number(process.env.RELAY_PORT ?? 8090);
  const server = await startRelay({ listenPort: port, target, onError: (code) => process.stderr.write(`relay: upstream ${code}\n`) });
  process.stdout.write(`relay: ${port} -> ${target.host}:${target.port}\n`);
  for (const s of ['SIGTERM', 'SIGINT']) process.on(s, () => server.close(() => process.exit(0)));
}
