// `bench doctor`: what the machine must have for the stack, and which fallback to use when nested Podman does not work.
//
// The Podman service of bench/compose.yaml is a container that runs containers. It was proven under rootful Docker
// in CI (dev/podman/README.md) and never under rootless Docker, so this is the first thing to check on the
// owner's desktop. Each check is { name, status: 'ok' | 'warn' | 'fail', detail, fix? }; the exit code is 1 when any
// check fails. Nothing is started unless `start` is set.
import fs from 'node:fs';
import { runCommand } from './stack.mjs';
import { coderRef } from './pins.mjs';

export const MIN_FREE_GB = 30;
export const WARN_FREE_GB = 100;
const GB = 1024 ** 3;

export const FALLBACKS = [
  'fallback 1: a rootless Podman on the host (not inside Docker), its socket mounted into the coder and the harness; the tasks run in it',
  'fallback 2: the coder\'s run pods on a k3d cluster (deploy/coder, RUN_ENVIRONMENT=kubernetes)',
];

const ok = (name, detail) => ({ name, status: 'ok', detail });
const warn = (name, detail, fix) => ({ name, status: 'warn', detail, ...(fix ? { fix } : {}) });
const fail = (name, detail, fix) => ({ name, status: 'fail', detail, ...(fix ? { fix } : {}) });

const readSysctl = (file, readFile) => {
  try {
    return readFile(file, 'utf8').trim();
  } catch {
    return null;
  }
};

/**
 * @param {object} o
 * @param {string} o.home       $BENCH_HOME
 * @param {object} o.pins
 * @param {(file: string, args: string[]) => Promise<{exitCode: number, stdout: string, stderr: string}>} [o.exec]
 * @param {object} [o.stack]    a Stack; with `start` it is brought up and the nested checks run
 * @param {boolean} [o.start]
 * @returns {Promise<{checks: object[], exitCode: number, fallback: string|null}>}
 */
export async function doctor({ home, pins, exec = runCommand, readFile = fs.readFileSync, statfs = fs.statfsSync, nodeVersion = process.versions.node, stack = null, start = false, slots = 2 }) {
  const checks = [];
  const run = async (file, args) => {
    try {
      return await exec(file, args, {});
    } catch {
      return { exitCode: 127, stdout: '', stderr: '' };
    }
  };

  const major = Number(nodeVersion.split('.')[0]);
  checks.push(major >= 22 ? ok('node', `Node ${nodeVersion} (CI runs 24)`) : fail('node', `Node ${nodeVersion} is too old`, 'install Node 22 or newer (24 is what CI uses)'));

  const compose = await run('docker', ['compose', 'version', '--short']);
  checks.push(compose.exitCode === 0 ? ok('docker compose', compose.stdout.trim()) : fail('docker compose', 'docker or its compose plugin is not available', 'install Docker Engine with the compose plugin'));

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
    if (rootless) checks.push(warn('rootless docker', 'nested Podman under rootless Docker was never proven', `if the nested check below fails, use ${FALLBACKS[0]}`));
  } else {
    checks.push(fail('docker daemon', 'the daemon does not answer', 'start Docker, or fix DOCKER_HOST / the docker context'));
  }

  // Unprivileged user namespaces: the service runs a rootless Podman, which makes one.
  const apparmor = readSysctl('/proc/sys/kernel/apparmor_restrict_unprivileged_userns', readFile);
  if (apparmor === '1') checks.push(fail('sysctl kernel.apparmor_restrict_unprivileged_userns', 'is 1: a rootless Podman cannot make a user namespace', 'sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0  (dev/podman/README.md)'));
  else checks.push(ok('sysctl kernel.apparmor_restrict_unprivileged_userns', apparmor === null ? 'not present on this kernel' : `is ${apparmor}`));
  const clone = readSysctl('/proc/sys/kernel/unprivileged_userns_clone', readFile);
  if (clone === '0') checks.push(fail('sysctl kernel.unprivileged_userns_clone', 'is 0', 'sudo sysctl -w kernel.unprivileged_userns_clone=1'));
  const maxUserns = readSysctl('/proc/sys/user/max_user_namespaces', readFile);
  if (maxUserns !== null && Number(maxUserns) <= 0) checks.push(fail('sysctl user.max_user_namespaces', 'is 0', 'sudo sysctl -w user.max_user_namespaces=15000'));

  // Disk: the images (several GB each) are in Docker's root and in the Podman services' storage volumes.
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
    else if (freeGb < MIN_FREE_GB) checks.push(fail(name, `${freeGb.toFixed(0)} GB free in ${dir}, less than ${MIN_FREE_GB}`, 'free disk space; the full run needs about 100 GB for its images'));
    else if (freeGb < WARN_FREE_GB) checks.push(warn(name, `${freeGb.toFixed(0)} GB free in ${dir}`, `about ${WARN_FREE_GB} GB is comfortable for the 100 task images`));
    else checks.push(ok(name, `${freeGb.toFixed(0)} GB free`));
  }

  // The coder image, present by the digest the pins name.
  const ref = coderRef(pins);
  const image = await run('docker', ['image', 'inspect', '--format', '{{json .RepoDigests}}', ref]);
  if (image.exitCode !== 0) checks.push(warn('coder image', `${ref} is not on this machine`, `docker pull ${ref}`));
  else {
    let digests = [];
    try {
      digests = JSON.parse(image.stdout);
    } catch {
      digests = [];
    }
    checks.push(digests.some((d) => d.endsWith(`@${pins.coder.digest}`)) ? ok('coder image', `present by digest ${pins.coder.digest.slice(0, 19)}...`) : fail('coder image', `a different image has the tag ${pins.coder.tag}`, `docker pull ${ref}`));
  }

  // The nested part, which needs the stack.
  if (stack && start && checks.every((c) => c.status !== 'fail')) {
    try {
      await stack.up();
      checks.push(ok('stack', 'the shared services and the Podman services are up'));
      for (let slot = 1; slot <= Math.min(slots, stack.slots); slot++) {
        const containers = stack.containers(slot);
        try {
          await stack.pullWindow(slot, () => containers.pull(ref));
          const r = await containers.run({ image: ref, script: 'true', network: 'none', timeoutMs: 120_000 });
          checks.push(r.exitCode === 0 ? ok(`nested Podman, slot ${slot}`, 'a container ran inside the service') : fail(`nested Podman, slot ${slot}`, `a container did not run (exit ${r.exitCode}): ${r.stderr.trim().split('\n').slice(-2).join(' | ')}`, `use ${FALLBACKS[0]}; then ${FALLBACKS[1]}`));
        } catch (e) {
          checks.push(fail(`Podman service, slot ${slot}`, e.message, `use ${FALLBACKS[0]}; then ${FALLBACKS[1]}`));
        }
      }
    } catch (e) {
      checks.push(fail('stack', e.message, `use ${FALLBACKS[0]}; then ${FALLBACKS[1]}`));
    }
  } else if (stack && !start) {
    checks.push(warn('nested Podman', 'not checked: the stack is not started', 'bench doctor --start brings it up and runs a container in each Podman service'));
  }

  const failed = checks.filter((c) => c.status === 'fail');
  const nested = failed.some((c) => /nested Podman|Podman service|^stack$|apparmor|userns|namespaces/.test(c.name));
  return { checks, exitCode: failed.length > 0 ? 1 : 0, fallback: nested ? FALLBACKS[0] : null };
}

/** The lines `bench doctor` prints. */
export function renderDoctor({ checks, fallback }) {
  const lines = checks.map((c) => `${c.status.toUpperCase().padEnd(4)} ${c.name}: ${c.detail}${c.fix ? `\n     fix: ${c.fix}` : ''}`);
  if (fallback) lines.push('', `Nested Podman does not work here. Use ${fallback}; then ${FALLBACKS[1]}.`);
  return lines.join('\n');
}
