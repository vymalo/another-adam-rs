#!/usr/bin/env node
// The command line of the benchmark harness. See bench/README.md.
//
//   bench config            check the configuration and print what may be shown of it
//   bench report            build report.md from the ledger
//   bench tasks <fetch|mine|lint|build|check|select>   task sources and the environment check
//   bench doctor [--start]  what the machine must have, and whether the coder starts in each selected task image
//   bench dry-run [--phase P] [--down]   the whole path on a scripted model, no money (needs a container runtime)
//   bench pilot | confirm | run | regrade   the pilot, the owner's confirmation, the full run, grading again
//
// Exit codes: 0 done, 1 a check failed, 64 usage, 78 bad configuration.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import { ConfigError, EXIT_CONFIG, assertOutsideRepo, benchHome, configPath, describeConfig, loadConfig } from '../lib/config.mjs';
import { doctor, renderDoctor } from '../lib/doctor.mjs';
import { BENCH_DIR, runDryRun } from '../lib/dryrun.mjs';
import { LedgerError, readLedger } from '../lib/ledger.mjs';
import { PINS } from '../lib/pins.mjs';
import { ReportError, buildReport } from '../lib/report.mjs';
import { Stack } from '../lib/stack.mjs';
import { runCli, loadSelected } from '../lib/run-cli.mjs';
import { paths, runTasks } from '../lib/tasks-cli.mjs';

const USAGE = `usage: bench <command>

  config                          check the configuration; print what may be shown of it
  report [--ledger FILE] [--out FILE] [--show-model-id]
                                  build the report from the ledger (default $BENCH_HOME/report.md)
  tasks fetch|mine|lint|build|check|select
                                  task sources, the environment check and selection (bench tasks --help)
  doctor [--start] [--keep-images]
                                  what the machine must have (Docker, disk, the coder image, the coder in each selected
                                  task image); --start brings the shared services up and runs a container on the host
  dry-run [--phase all|grades|budget|resume] [--down]
                                  both agents on three self-test tasks and a scripted model: grades, cost, budget, kill and resume
  pilot [--blocks N] [--keep-images]
                                  the first blocks of the plan (default 5) with the configuration's per-run budget; prints the
                                  projection and writes $BENCH_HOME/pilot-budget.json
  confirm [--over-cap]            the owner's yes to that file; run refuses to start without it
  run [--keep-images]             the full run: blocks, slots, resume, grading, then report.md
  regrade [--keep-images]         grade again the runs whose grading failed in the infrastructure
                                  (a public task's image is pulled by digest before its first run and removed after its last;
                                  --keep-images keeps every image, as before)

environment: BENCH_CONFIG (default ~/.config/adam-bench/config.json), BENCH_HOME (default ~/.local/share/adam-bench)
`;

function report(argv, env) {
  const { values } = parseArgs({ args: argv, options: { ledger: { type: 'string' }, out: { type: 'string' }, 'show-model-id': { type: 'boolean' } }, allowPositionals: false });
  const home = benchHome(env);
  assertOutsideRepo(home, 'BENCH_HOME');
  const ledgerFile = values.ledger ?? path.join(home, 'ledger.jsonl');
  const outFile = values.out ?? path.join(home, 'report.md');
  if (values.ledger) assertOutsideRepo(ledgerFile, '--ledger');
  if (values.out) assertOutsideRepo(outFile, '--out');
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
  fs.mkdirSync(path.dirname(outFile), { recursive: true });
  fs.writeFileSync(outFile, buildReport(records, opts));
  process.stdout.write(`${outFile}\n`);
  return 0;
}

async function doctorCommand(argv, env) {
  const { values } = parseArgs({ args: argv, options: { start: { type: 'boolean' }, 'keep-images': { type: 'boolean' } }, allowPositionals: false });
  const home = benchHome(env);
  assertOutsideRepo(home, 'BENCH_HOME');
  fs.mkdirSync(home, { recursive: true });
  const stack = new Stack({
    composeFile: path.join(BENCH_DIR, 'compose.yaml'),
    home,
    meterPort: Number(env.BENCH_METER_PORT ?? 18090),
    baseEnv: env,
  });
  // The selected tasks are what the coder probe runs on; a configuration that is absent or a selection that is not
  // made yet is a warning, never a reason for the doctor to fail.
  let tasks = [];
  let taskError = null;
  try {
    const config = loadConfig({ env });
    tasks = loadSelected(config, paths(env)).tasks;
  } catch (e) {
    if (env.BENCH_CONFIG || fs.existsSync(configPath(env))) taskError = e.message;
  }
  const result = await doctor({ home, pins: PINS, stack, start: values.start === true, tasks, taskError, containers: stack.hostContainers(), env, justInTime: values['keep-images'] !== true });
  process.stdout.write(`${renderDoctor(result)}\n`);
  return result.exitCode;
}

async function dryRunCommand(argv, env) {
  const { values } = parseArgs({ args: argv, options: { phase: { type: 'string' }, down: { type: 'boolean' } }, allowPositionals: false });
  const phase = values.phase ?? 'all';
  if (!['all', 'grades', 'budget', 'resume', 'resume-child'].includes(phase)) throw Object.assign(new Error('--phase is all, grades, budget or resume'), { code: 'ERR_PARSE_ARGS_INVALID_OPTION_VALUE' });
  return runDryRun({ phase, down: values.down === true, env, entry: fileURLToPath(import.meta.url) });
}

export async function main(argv, env = process.env) {
  const [command, ...rest] = argv;
  try {
    if (command === undefined || command === '--help' || command === '-h') {
      process.stdout.write(USAGE);
      return command === undefined ? 64 : 0;
    }
    if (command === 'config') {
      process.stdout.write(`${describeConfig(loadConfig({ env }))}\n`);
      return 0;
    }
    if (command === 'report') return report(rest, env);
    if (command === 'doctor') return await doctorCommand(rest, env);
    if (command === 'dry-run') return await dryRunCommand(rest, env);
    if (['pilot', 'confirm', 'run', 'regrade'].includes(command)) return await runCli(command, rest, { env });
    if (command === 'tasks') return await runTasks(rest, { env });
    process.stderr.write(`bench: unknown command ${JSON.stringify(command)}\n\n${USAGE}`);
    return 64;
  } catch (e) {
    if (e instanceof ConfigError) {
      process.stderr.write(`bench: configuration error: ${e.message}\n`);
      return EXIT_CONFIG;
    }
    if (e instanceof LedgerError || e instanceof ReportError || e?.code === 'ERR_PARSE_ARGS_UNKNOWN_OPTION' || e?.code === 'ERR_PARSE_ARGS_INVALID_OPTION_VALUE') {
      process.stderr.write(`bench: ${e.message}\n`);
      return e instanceof LedgerError || e instanceof ReportError ? 1 : 64;
    }
    throw e;
  }
}

const entry = process.argv[1] ? fs.realpathSync(process.argv[1]) : '';
if (entry === fileURLToPath(import.meta.url)) {
  process.exitCode = await main(process.argv.slice(2));
}
