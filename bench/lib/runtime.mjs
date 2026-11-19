// The host runtime the whole bench shares: one store, one user for both arms, and one set of CPU and memory limits.
//
// The bench runs its containers on the host's Docker (ADR 0048, amended 2026-11-19): every image is in the host's
// store once, and a container is a host container. Both arms run as uid 10001 and get `cpus` and `memoryMb` from
// here, so a difference between the two is never the runtime's. `configFingerprint` records this object, and a
// change in it changes the digest a pilot is bound to.
export const RUNTIME = Object.freeze({
  host: 'docker',
  store: 'host',
  coder: 'files-from-image',
  uid: 10001,
  gid: 10001,
  cpus: 4,
  memoryMb: 12 * 1024,
});

/** `10001:10001`, as `docker run --user` takes it. */
export const runtimeUser = (runtime = RUNTIME) => `${runtime.uid}:${runtime.gid}`;

/** The limits of one arm, as a RunSpec takes them; both arms get these, never the task's own. */
export const runtimeLimits = (runtime = RUNTIME) => ({ cpus: runtime.cpus, memoryMb: runtime.memoryMb });
