// What the harness's errors carry besides a message: whether a retry can help, and the stderr of the command that failed.

/** An error that a retry cannot change (a repository the git of the image cannot read, a base that does not match its marker): the run is `Failed_infra` and final. */
export class PermanentError extends Error {
  retryable = false;
  constructor(message, { stderr } = {}) {
    super(message);
    this.name = 'PermanentError';
    if (stderr !== undefined) this.stderr = stderr;
  }
}

/** The stderr kept for a failed step, in the ledger line and in the run's directory: at most this many characters. */
export const MAX_STDERR_CHARS = 64 * 1024;
const MAX_SUMMARY_CHARS = 300;

/**
 * One line of what a failed command said, for the error message: the first `fatal:` or `error:` line (git names the path
 * there), then the last non-empty lines; empty lines dropped, the whole at most 300 characters. The full text goes to
 * `prepare-stderr.txt`, not here.
 */
export function summarize(stderr, { last = 2 } = {}) {
  const lines = String(stderr ?? '').split('\n').map((l) => l.trim()).filter(Boolean);
  const first = lines.find((l) => /^(fatal|error):/i.test(l)) ?? lines[0];
  const picked = [...new Set([first, ...lines.slice(-last)].filter((l) => l !== undefined))];
  const text = picked.join(' | ');
  return text.length > MAX_SUMMARY_CHARS ? `${text.slice(0, MAX_SUMMARY_CHARS - 1)}…` : text;
}

/** Attach the full stderr of the failed command to an error (the caller has taken the secrets out of it). Returns the error. */
export function withStderr(error, stderr) {
  error.stderr = String(stderr ?? '').slice(-MAX_STDERR_CHARS);
  return error;
}
