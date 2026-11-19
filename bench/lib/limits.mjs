// Does the host runtime accept the CPU and memory limits the harness asks for? A rootless Docker does; a daemon that
// cannot enforce cgroups may refuse. The harness asks once, with a container that does nothing: first without the
// limits (if that fails the runtime is down, which is not a refusal of limits), then with them. `bench doctor` asks
// the same question with the flags the arms use. The arms take their limits from one setting (lib/runtime.mjs), so
// both get the same; the answer is only a report, never a reason to give one arm more than the other.
export async function probeLimits({ containers, image, cpus, memoryMb }) {
  const bare = await containers.run({ image, script: 'true', network: 'none', timeoutMs: 120_000 });
  if (bare.exitCode !== 0) return { accepted: false, runtimeFailed: true, detail: bare.stderr.trim().split('\n').slice(-2).join(' | ') };
  const limited = await containers.run({ image, script: 'true', network: 'none', cpus, memoryMb, timeoutMs: 120_000 });
  return limited.exitCode === 0 ? { accepted: true, runtimeFailed: false, detail: '' } : { accepted: false, runtimeFailed: false, detail: limited.stderr.trim().split('\n').slice(-2).join(' | ') };
}
