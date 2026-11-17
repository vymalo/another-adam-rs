#!/usr/bin/env node
// The command line of the benchmark harness. See bench/README.md.
//
//   bench config            check the configuration and print what may be shown of it
//   bench report            build report.md from the ledger
//   bench doctor | tasks | pilot | run | regrade   not built yet (stubs, exit 2)
//
// Exit codes: 0 done, 2 not built yet, 64 usage, 78 bad configuration.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import { ConfigError, EXIT_CONFIG, assertOutsideRepo, benchHome, configPath, describeConfig, loadConfig } from '../lib/config.mjs';
import { LedgerError, readLedger } from '../lib/ledger.mjs';
import { buildReport } from '../lib/report.mjs';

const STUBS = { doctor: 3, tasks: 2, pilot: 4, run: 4, regrade: 4 };
const USAGE = `usage: bench <command>

  config                          check the configuration; print what may be shown of it
  report [--ledger FILE] [--out FILE] [--show-model-id]
                                  build the report from the ledger (default $BENCH_HOME/report.md)
  doctor | tasks | pilot | run | regrade
                                  not built yet

environment: BENCH_CONFIG (default ~/.config/adam-bench/config.json), BENCH_HOME (default ~/.local/share/adam-bench)
`;

function report(argv, env) {
  const { values } = parseArgs({ args: argv, options: { ledger: { type: 'string' }, out: { type: 'string' }, 'show-model-id': { type: 'boolean' } }, allowPositionals: false });
  const home = benchHome(env);
  assertOutsideRepo(home, 'BENCH_HOME');
  const ledgerFile = values.ledger ?? path.join(home, 'ledger.jsonl');
  const { records, warnings } = readLedger(ledgerFile);
  for (const w of warnings) process.stderr.write(`bench: warning: ${w}\n`);
  if (records.length === 0) {
    process.stderr.write('bench: the ledger has no records; nothing to report\n');
    return 1;
  }
  // The configuration is optional for a report, but a configuration that exists must be sound.
  const opts = { showModelId: values['show-model-id'] === true };
  if (env.BENCH_CONFIG || fs.existsSync(configPath(env))) {
    const config = loadConfig({ env });
    opts.modelLabel = config.modelLabel;
    opts.capMicro = config.capMicro;
    if (opts.showModelId) opts.modelId = config.modelId;
  } else if (opts.showModelId) {
    throw new ConfigError('--show-model-id', 'needs the configuration, which holds the model id');
  }
  const outFile = values.out ?? path.join(home, 'report.md');
  fs.mkdirSync(path.dirname(outFile), { recursive: true });
  fs.writeFileSync(outFile, buildReport(records, opts));
  process.stdout.write(`${outFile}\n`);
  return 0;
}

export function main(argv, env = process.env) {
  const [command, ...rest] = argv;
  try {
    if (command === undefined || command === '--help' || command === '-h') {
      process.stdout.write(USAGE);
      return command === undefined ? 64 : 0;
    }
    if (command in STUBS) {
      process.stderr.write(`bench: ${command} is not built yet (pull request ${STUBS[command]})\n`);
      return 2;
    }
    if (command === 'config') {
      process.stdout.write(`${describeConfig(loadConfig({ env }))}\n`);
      return 0;
    }
    if (command === 'report') return report(rest, env);
    process.stderr.write(`bench: unknown command ${JSON.stringify(command)}\n\n${USAGE}`);
    return 64;
  } catch (e) {
    if (e instanceof ConfigError) {
      process.stderr.write(`bench: configuration error: ${e.message}\n`);
      return EXIT_CONFIG;
    }
    if (e instanceof LedgerError || e?.code === 'ERR_PARSE_ARGS_UNKNOWN_OPTION' || e?.code === 'ERR_PARSE_ARGS_INVALID_OPTION_VALUE') {
      process.stderr.write(`bench: ${e.message}\n`);
      return e instanceof LedgerError ? 1 : 64;
    }
    throw e;
  }
}

const entry = process.argv[1] ? fs.realpathSync(process.argv[1]) : '';
if (entry === fileURLToPath(import.meta.url)) {
  process.exitCode = main(process.argv.slice(2));
}
