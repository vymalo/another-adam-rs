// `bench doctor`: what the machine must have for the stack, and whether the coder starts in each selected task image.
//
// The bench runs its containers on the host's Docker (lib/containers.mjs): every image is in one store, and a task
// image is run as it is. The coder's binaries are static (musl, ADR 0053 decision 11), so the image's C library does
// not matter: a Debian bookworm task image starts the coder as well as a trixie one. The probe runs the coder with
// no configuration as uid 10001 in the image: exit 78 means it starts (the coder refuses an empty configuration,
// which is what a start is); a loader error, a missing interpreter or an architecture the image cannot run means it
// does not, and such an image is refused for both arms. Each check is { name, status: 'ok' | 'warn' | 'fail', detail,
// fix? }; the exit code is 1 when any check fails. Nothing is started unless `start` is set.
import fs from 'node:fs';
import { dockerEnv } from './containers.mjs';
import { probeLimits } from './limits.mjs';
import { coderRef } from './pins.mjs';
import { RUNTIME, runtimeLimits, runtimeUser } from './runtime.mjs';
import { runCommand } from './stack.mjs';

// The limits the probe asks the runtime to enforce: the arms' own (lib/runtime.mjs).
const PROBE = runtimeLimits(RUNTIME);

/**
 * The dry run needs about 20 GB (the coder image once, the task images and their layers); the full run about 100. The images
 * already on the machine count as free: the check passes when `free + the size of the coder and selected task images present`
 * reaches the floor (a task image's size includes its shared base, so a few shared layers are counted more than once).
 */
export const MIN_FREE_GB = 20;
export const WARN_FREE_GB = 100;
const GB = 1024 ** 3;

/**
 * Does this Node's `http.request`/`https.request` (what the meter uses) take a proxy from the environment under NODE_USE_ENV_PROXY=1?
 * *Verified 2026-11-17* in Node's changelogs (doc/changelogs/CHANGELOG_V24.md and CHANGELOG_V22.md, "http,https: add built-in proxy
 * support in http/https.request and Agent", #58980): first in 24.5.0 (2025-07-31) and 22.21.0 (2025-10-20). `fetch()` has had it since
 * 24.0.0, which is not what the meter uses.
 */
export function envProxyInRequest(version) {
  const [major, minor] = version.split('.').map(Number);
  if (major >= 25) return true;
  if (major === 24) return minor >= 5;
  if (major === 22) return minor >= 21;
  return false;
}

const ok = (name, detail) => ({ name, status: 'ok', detail });
const warn = (name, detail, fix) => ({ name, status: 'warn', detail, ...(fix ? { fix } : {}) });
const fail = (name, detail, fix) => ({ name, status: 'fail', detail, ...(fix ? { fix } : {}) });

const LOADER_ERROR = /error while loading shared libraries|GLIBC_[0-9.]+.*not found|version `[^']+' not found|No such file or directory|exec format error|not found/i;

/**
 * Run the coder binary with no configuration, as uid 10001, in `image`: exit 78 means it starts (it refuses the empty
 * configuration, which is a start), a loader error means the image cannot load it. Returns { starts, exitCode, detail }.
 */
export async function coderProbe({ containers, image, filesDir, timeoutMs = 120_000 }) {
  const r = await containers.run({
    image,
    network: 'none',
    user: runtimeUser(),
    mounts: [{ source: filesDir, target: '/opt/adam/bin', readOnly: true }],
    entrypoint: '/opt/adam/bin/adam-coder',
    timeoutMs,
  });
  const text = `${r.stderr ?? ''}\n${r.stdout ?? ''}`.trim().split('\n').filter(Boolean).slice(-2).join(' | ');
  if (r.exitCode === 78) return { starts: true, exitCode: 78, detail: 'exit 78: the coder starts and refuses an empty configuration' };
  if (r.exitCode === 126 || r.exitCode === 127 || LOADER_ERROR.test(text)) return { starts: false, exitCode: r.exitCode, detail: `exit ${r.exitCode}: ${text || 'the binary could not be loaded'}` };
  return { starts: null, exitCode: r.exitCode, detail: `exit ${r.exitCode}: ${text || 'no output'}` };
}

/** The image's distribution, best effort: `PRETTY_NAME` from `/etc/os-release`, or null when it cannot be read. */
export async function osRelease({ containers, image, timeoutMs = 60_000 }) {
  const r = await containers.run({ image, network: 'none', entrypoint: '/bin/cat', command: ['/etc/os-release'], timeoutMs });
  if (r.exitCode !== 0) return null;
  const m = /^PRETTY_NAME="?([^"\n]+)"?$/m.exec(r.stdout ?? '');
  return m ? m[1] : null;
}

/**
 * @param {object} o
 * @param {string} o.home       $BENCH_HOME
 * @param {object} o.pins
 * @param {Array} [o.tasks]     the selected tasks: one probe per task image
 * @param {object} [o.containers] what the probes run on (a HostContainers); the stack's when `stack` is given
 * @param {(file: string, args: string[]) => Promise<{exitCode: number, stdout: string, stderr: string}>} [o.exec]
 * @param {object} [o.stack]    a Stack; with `start` it is brought up and a container is run on the host
 * @param {boolean} [o.start]
 * @param {object} [o.env]      the environment to read (default: the stack's `baseEnv`, or `process.env` without a stack)
 * @returns {Promise<{checks: object[], exitCode: number}>}
 */
export async function doctor({ home, pins, exec = runCommand, statfs = fs.statfsSync, nodeVersion = process.versions.node, arch = process.arch, stack = null, start = false, slots = 2, tasks = [], taskError = null, containers = null, env = null }) {
  // A stack carries the environment it was built with (`baseEnv`), which is what its compose calls and host containers
  // already use. The doctor's own docker calls must use the same one, never the ambient environment: a caller that
  // injects a stack (the tests do, and `bench doctor` builds it from `main`'s env) would otherwise get two lists.
  const baseEnv = env ?? stack?.baseEnv ?? process.env;
  const checks = [];
  const run = async (file, args) => {
    try {
      return await exec(file, args, { env: dockerEnv(baseEnv) });
    } catch {
      return { exitCode: 127, stdout: '', stderr: '' };
    }
  };

  const major = Number(nodeVersion.split('.')[0]);
  checks.push(major >= 22 ? ok('node', `Node ${nodeVersion} (CI runs 22 and 24)`) : fail('node', `Node ${nodeVersion} is too old`, 'install Node 22 or newer (CI runs 22 and 24)'));

  // The meter reaches the endpoint with Node's own http client, which uses a proxy from the environment only when asked to.
  const proxy = baseEnv.HTTPS_PROXY ?? baseEnv.https_proxy;
  if (proxy) {
    if (!envProxyInRequest(nodeVersion)) {
      checks.push(warn('proxy', `HTTPS_PROXY is set but Node ${nodeVersion}'s https.request ignores it, so the meter would reach the endpoint directly`, 'use Node 22.21.0 or newer, or 24.5.0 or newer, and start the harness with NODE_USE_ENV_PROXY=1'));
    } else {
      checks.push(baseEnv.NODE_USE_ENV_PROXY === '1' ? ok('proxy', 'HTTPS_PROXY is set and NODE_USE_ENV_PROXY=1: the meter reaches the endpoint through it') : warn('proxy', 'HTTPS_PROXY is set but the meter would reach the endpoint directly', 'start the harness with NODE_USE_ENV_PROXY=1 if the endpoint is reached through that proxy'));
    }
  }

  // The binary of OpenCode that is mounted is a native linux-x64 executable linked to glibc.
  checks.push(arch === 'x64' ? ok('machine', 'x64: the OpenCode binary of the pins runs here') : fail('machine', `${arch}: the OpenCode binary of the pins is linux-x64`, 'run the benchmark on an x64 Linux machine; a task image with musl (Alpine) cannot run it either, and the environment check says so'));

  const compose = await run('docker', ['compose', 'version', '--short']);
  const composeVersion = /^v?(\d+)\.(\d+)/.exec(compose.stdout.trim());
  if (compose.exitCode !== 0) checks.push(fail('docker compose', 'docker or its compose plugin is not available', 'install Docker Engine with the compose plugin'));
  else if (!composeVersion || Number(composeVersion[1]) < 2 || (Number(composeVersion[1]) === 2 && Number(composeVersion[2]) < 24)) checks.push(fail('docker compose', `${compose.stdout.trim()} is older than 2.24: compose.yaml's \`env_file: required: false\` needs 2.24`, 'upgrade the compose plugin to 2.24 or newer'));
  else checks.push(ok('docker compose', compose.stdout.trim()));

  let info = null;
  const daemon = await run('docker', ['info', '--format', '{{json .}}']);
  if (daemon.exitCode === 0) {
    try {
      info = JSON.parse(daemon.stdout);
    } catch {
      info = null;
    }
  }
  if (info) {
    const rootless = (info.SecurityOptions ?? []).some((o) => /rootless/.test(o));
    checks.push(ok('docker daemon', `${info.ServerVersion ?? 'unknown version'}, ${rootless ? 'rootless' : 'rootful'}, cgroup v${info.CgroupVersion ?? '?'}`));
    if (rootless) checks.push(warn('rootless docker', 'a container runs as a subuid on the host; the work directory is 0777, so uid 10001 can write it', 'none if the dry run passes; a bind-mounted directory the harness owns must stay writable by uid 10001'));
  } else {
    checks.push(fail('docker daemon', 'the daemon does not answer', 'start Docker, or fix DOCKER_HOST / the docker context'));
  }

  // The images already here are space this run does not need to find: their `.Size` counts toward the floor.
  const ref = coderRef(pins);
  const image = await run('docker', ['image', 'inspect', '--format', '{{json .RepoDigests}}', ref]);
  let presentBytes = 0;
  for (const imageRef of [ref, ...tasks.map((t) => t.image?.ref ?? t.image?.name)]) {
    if (!imageRef) continue;
    const size = await run('docker', ['image', 'inspect', '--format', '{{.Size}}', imageRef]);
    const bytes = size.exitCode === 0 ? Number(size.stdout.trim()) : 0;
    if (Number.isFinite(bytes) && bytes > 0) presentBytes += bytes;
  }
  const presentGb = presentBytes / GB;

  // Disk: the images are in Docker's root once (the coder image is the big one), and the results are in $BENCH_HOME.
  const dirs = [['results ($BENCH_HOME)', home], ...(info?.DockerRootDir ? [['Docker root', info.DockerRootDir]] : [])];
  for (const [label, dir] of dirs) {
    let freeGb = null;
    try {
      const s = statfs(dir);
      freeGb = (s.bavail * s.bsize) / GB;
    } catch {
      freeGb = null;
    }
    const name = `free disk, ${label}`;
    if (freeGb === null) checks.push(warn(name, `${dir} cannot be read`, 'create it, or run as a user that can read it'));
    else if (freeGb + presentGb < MIN_FREE_GB) checks.push(fail(name, `${freeGb.toFixed(1)} GB free in ${dir} and ${presentGb.toFixed(1)} GB of the images already present, less than ${MIN_FREE_GB}`, 'free disk space; the dry run needs about 20 GB (images already present count) and the full run about 100 GB'));
    else if (freeGb + presentGb < WARN_FREE_GB) checks.push(warn(name, `${freeGb.toFixed(0)} GB free in ${dir}${presentGb > 0 ? ` and ${presentGb.toFixed(0)} GB of images already present` : ''}`, `about ${WARN_FREE_GB} GB is comfortable for the 100 task images`));
    else checks.push(ok(name, `${freeGb.toFixed(0)} GB free`));
  }

  // The coder image, present by the digest the pins name.
  let coderPresent = false;
  if (image.exitCode !== 0) checks.push(warn('coder image', `${ref} is not on this machine`, `docker pull ${ref}`));
  else {
    let digests = [];
    try {
      digests = JSON.parse(image.stdout);
    } catch {
      digests = [];
    }
    coderPresent = digests.some((d) => d.endsWith(`@${pins.coder.digest}`));
    checks.push(coderPresent ? ok('coder image', `present by digest ${pins.coder.digest.slice(0, 19)}...`) : fail('coder image', `a different image has the tag ${pins.coder.tag}`, `docker pull ${ref}`));
  }

  // What a killed harness leaves: an env file holds a bearer and a run token.
  if (stack?.removeStaleEnvFiles) {
    const gone = stack.removeStaleEnvFiles();
    checks.push(gone.length > 0 ? warn('stale env files', `removed ${gone.join(', ')} (a killed harness left them)`) : ok('stale env files', 'none'));
  }

  // Per selected task image: does the coder start there? The static coder starts in any x86-64 Linux image; an image
  // that cannot run it (a loader error, no /bin/sh for the probe's entry, another architecture) is refused for both arms.
  const probe = containers ?? stack?.hostContainers?.() ?? null;
  if (taskError) checks.push(warn('task images', `the selected tasks could not be read: ${taskError}`, 'fix the configuration and the lock files (bench tasks select)'));
  else if (tasks.length === 0) checks.push(warn('task images', 'none selected: nothing to probe', 'bench tasks select, then bench doctor'));
  else if (!coderPresent) checks.push(warn('task images', 'not probed: the coder image is not on this machine', `docker pull ${ref}`));
  else if (!probe) checks.push(warn('task images', 'not probed: the doctor has no container runtime', 'run bench doctor with the stack'));
  else {
    let filesDir = null;
    let filesError = null;
    try {
      filesDir = await stack.ensureCoderFiles();
    } catch (e) {
      filesError = e.message;
    }
    if (!filesDir) checks.push(warn('task images', `not probed: the coder's binaries could not be read from the image (${filesError})`, `docker pull ${ref}`));
    else {
      for (const task of tasks) {
        const imageRef = task.image?.ref ?? task.image?.name;
        const label = `task image ${task.id}`;
        const distro = await osRelease({ containers: probe, image: imageRef }).catch(() => null);
        const p = await coderProbe({ containers: probe, image: imageRef, filesDir }).catch((e) => ({ starts: null, exitCode: null, detail: e.message }));
        const where = distro ? `${distro}; ` : '';
        if (p.starts === true) checks.push(ok(label, `${where}the coder starts (${p.detail})`));
        else if (p.starts === false) checks.push(fail(label, `${where}the coder does not start (${p.detail}); the image is refused for both arms`, 'use a task image whose loader and architecture can run a static x86-64 binary, or fix the image'));
        else checks.push(warn(label, `${where}the coder's start is unclear (${p.detail})`, 'run a container of the image by hand and read the error'));
      }
    }
  }

  // The host runtime itself, which needs the stack.
  if (stack && start && checks.every((c) => c.status !== 'fail')) {
    try {
      await stack.up();
      checks.push(ok('stack', 'the shared services are up'));
      const targets = Array.from({ length: Math.min(slots, stack.slots) }, (_, i) => ({ label: `slot ${i + 1}`, containers: stack.containers(i + 1) }));
      targets.push({ label: 'grader', containers: stack.graderContainers() });
      for (const { label, containers: c } of targets) {
        const r = await c.run({ image: ref, script: 'true', network: 'none', timeoutMs: 120_000 });
        checks.push(r.exitCode === 0 ? ok(`host runtime, ${label}`, 'a container of the coder image ran') : fail(`host runtime, ${label}`, `a container did not run (exit ${r.exitCode}): ${r.stderr.trim().split('\n').slice(-2).join(' | ')}`, 'check the Docker daemon and its storage'));
      }
      const p = await probeLimits({ containers: stack.containers(1), image: ref, ...PROBE });
      checks.push(p.accepted ? ok('runtime limits', `the runtime takes --cpus ${PROBE.cpus} and --memory ${PROBE.memoryMb} MiB`) : warn('runtime limits', `the runtime refuses the limits (${p.detail}): both arms still get the same setting`, 'fix the daemon, or accept that the limits are not enforced'));
    } catch (e) {
      checks.push(fail('stack', e.message, 'read the error above; bench doctor --start starts the shared services'));
    }
  } else if (stack && !start) {
    checks.push(warn('host runtime', 'not checked: the stack is not started', 'bench doctor --start brings it up and runs a container of the coder image'));
  }

  const failed = checks.filter((c) => c.status === 'fail');
  return { checks, exitCode: failed.length > 0 ? 1 : 0 };
}

/** The lines `bench doctor` prints. */
export function renderDoctor({ checks }) {
  return checks.map((c) => `${c.status.toUpperCase().padEnd(4)} ${c.name}: ${c.detail}${c.fix ? `\n     fix: ${c.fix}` : ''}`).join('\n');
}
