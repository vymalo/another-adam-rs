// Does the Podman service accept the CPU and memory limits of a task? Its containers have no cgroups of their own
// (`cgroups="disabled"`, dev/podman/README.md), so it may refuse `--cpus` and `--memory`. The harness asks once, with a
// container that does nothing: first without the limits (if that fails the service is down, which is not a refusal of
// limits), then with them. `bench doctor --start` asks the same question with the same flags; a driver uses the answer:
// limits accepted are passed, limits refused are not, and the run record says the limits are the service's own, which
// are the same for both agents.
export async function probeLimits({ containers, image, cpus, memoryMb }) {
  const bare = await containers.run({ image, script: 'true', network: 'none', timeoutMs: 120_000 });
  if (bare.exitCode !== 0) return { accepted: false, runtimeFailed: true, detail: bare.stderr.trim().split('\n').slice(-2).join(' | ') };
  const limited = await containers.run({ image, script: 'true', network: 'none', cpus, memoryMb, timeoutMs: 120_000 });
  return limited.exitCode === 0 ? { accepted: true, runtimeFailed: false, detail: '' } : { accepted: false, runtimeFailed: false, detail: limited.stderr.trim().split('\n').slice(-2).join(' | ') };
}
