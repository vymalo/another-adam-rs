// The C library the pinned OpenCode binary needs, for a task image that has none of its own.
//
// The binary is a dynamically linked glibc executable (interpreter /lib64/ld-linux-x86-64.so.2) that also loads
// libstdc++ and libgcc_s, and resolves names through glibc's own resolver modules. A task image built on musl (Alpine: the
// SWE-bench Pro teleport images) has none of that: its shell answers "not found" (exit 127) for the binary itself. The harness
// copies the files from the pinned coder image (Debian, `Stack.ensureGlibc`) and mounts them, read-only, at the places a glibc
// system has them, only in an image that lacks the loader. Nothing else of the image changes and no environment variable is set,
// so the agent's own tools never see them. Not through a loader invoked by hand: the binary starts a private server from its own
// `process.execPath`, which would then be the loader (*verified 2026-11-20*: "Standalone server exited before reporting readiness").
// Without libgcc_s and libstdc++ the binary segfaults (Bun, after the database bootstrap); without the resolver modules the
// meter's name does not resolve (*verified 2026-11-20* in a musl image, bun 1.4.4 inside OpenCode 2.3.0).
import { summarize } from './errors.mjs';
import { runtimeUser } from './runtime.mjs';

/** An image that cannot run a shell: the runner records it as an infrastructure failure of reason `runtime`. */
export class ShellError extends Error {
  reason = 'runtime';
}

/** The loader the binary asks for (its ELF interpreter). */
export const NATIVE_LOADER = '/lib64/ld-linux-x86-64.so.2';
/** Where Debian (the coder image, *verified 2026-11-20* in debian:trixie-slim) keeps the libraries, and where its loader looks. */
export const GLIBC_LIB_DIR = '/usr/lib/x86_64-linux-gnu';
export const GLIBC_LOADER_FILE = 'ld-linux-x86-64.so.2';
export const GLIBC_LIB_FILES = ['libc.so.6', 'libm.so.6', 'libpthread.so.0', 'libdl.so.2', 'libgcc_s.so.1', 'libstdc++.so.6', 'libresolv.so.2', 'libnss_files.so.2', 'libnss_dns.so.2'];
/** Every file `Stack.ensureGlibc` copies out of the coder image. */
export const GLIBC_FILES = [GLIBC_LOADER_FILE, ...GLIBC_LIB_FILES];

const PLAIN = /^\/[A-Za-z0-9._/@+=-]*$/;

/**
 * Does this image lack the loader of the pinned binary? Asked of a container of the image that only tests for the file: true
 * for a musl image, which then needs `glibcMounts`. Throws when the image cannot run a shell at all.
 */
export async function lacksLoader({ containers, image, timeoutMs = 60_000 }) {
  const r = await containers.run({ image, network: 'none', user: runtimeUser(), script: `test -e ${NATIVE_LOADER}`, timeoutMs });
  if (r.exitCode === 0) return false;
  if (r.exitCode === 1) return true;
  throw new ShellError(`the task image could not run a shell (exit ${r.exitCode}): ${summarize(r.stderr)}`);
}

/** The read-only mounts of the runtime in `glibcDir`: the loader at its interpreter path and each library where glibc looks. Pure. */
export function glibcMounts(glibcDir) {
  if (!PLAIN.test(glibcDir)) throw new Error('the glibc directory is not a plain absolute path');
  return [
    { source: `${glibcDir}/${GLIBC_LOADER_FILE}`, target: NATIVE_LOADER, readOnly: true },
    ...GLIBC_LIB_FILES.map((f) => ({ source: `${glibcDir}/${f}`, target: `${GLIBC_LIB_DIR}/${f}`, readOnly: true })),
  ];
}
