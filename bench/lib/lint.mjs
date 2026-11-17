// The statement lint: a statement must not give away the solution. It warns when an identifier or a path
// that appears only in the gold diff's added lines (not in its context or removed lines) is in the statement.
// An "Interface" section is exempt: it is where the names the hidden tests call belong.

const IDENT = /[A-Za-z_][A-Za-z0-9_]{3,}/g;
const PATH = /[A-Za-z0-9_.-]+(?:\/[A-Za-z0-9_.-]+)+/g;

/** A distinctive identifier: snake_case, camelCase or PascalCase with an inner capital. Plain words are not. */
export function isDistinctive(token) {
  return token.includes('_') || /[a-z][A-Z]/.test(token) || /^[A-Z][a-z0-9]+[A-Z]/.test(token);
}

function tokensOf(text) {
  const idents = new Set((text.match(IDENT) ?? []).filter(isDistinctive));
  const paths = new Set(text.match(PATH) ?? []);
  return { idents, paths };
}

/** Split a unified diff into the text of its added lines and the text of its other lines (context, removed, headers). */
export function splitDiff(diff) {
  const added = [];
  const other = [];
  for (const line of String(diff).split('\n')) {
    if (line.startsWith('+++') || line.startsWith('---')) {
      // File headers: their paths are in the repository already (or are what the diff adds, which `+++` names).
      (line.startsWith('+++') ? added : other).push(line.slice(4).replace(/^[ab]\//, ''));
    } else if (line.startsWith('+')) added.push(line.slice(1));
    else if (line.startsWith('-')) other.push(line.slice(1));
    else if (line.startsWith(' ')) other.push(line.slice(1));
  }
  return { added: added.join('\n'), other: other.join('\n') };
}

/** Remove the "Interface" section (a heading containing the word, up to the next heading of the same or higher level). */
export function withoutInterface(statement) {
  const lines = String(statement).split('\n');
  const out = [];
  let skipping = null;
  for (const line of lines) {
    const h = /^(#{1,6})\s+(.*)$/.exec(line);
    if (h) {
      if (skipping !== null && h[1].length <= skipping) skipping = null;
      if (skipping === null && /\binterface\b/i.test(h[2])) {
        skipping = h[1].length;
        continue;
      }
    }
    if (skipping === null) out.push(line);
  }
  return out.join('\n');
}

const has = (text, token) => new RegExp(`(^|[^A-Za-z0-9_])${token.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}($|[^A-Za-z0-9_])`).test(text);

/** Warnings for a statement against the gold diff. Returns [{ kind: 'identifier' | 'path', token }]. */
export function lintStatement(statement, goldDiff) {
  const { added, other } = splitDiff(goldDiff);
  const a = tokensOf(added);
  const o = tokensOf(other);
  const body = withoutInterface(statement);
  const warnings = [];
  for (const t of [...a.idents].sort()) if (!o.idents.has(t) && has(body, t)) warnings.push({ kind: 'identifier', token: t });
  for (const t of [...a.paths].sort()) if (!o.paths.has(t) && has(body, t)) warnings.push({ kind: 'path', token: t });
  return warnings;
}

/**
 * The command a coder would find to check its own work in a repository at the base: a `test` script in `package.json`, a
 * `Cargo.toml`, a `go.mod`, a Makefile or justfile with a `test` target, a Python test configuration, or a test command its README
 * or CONTRIBUTING names. `read(path)` returns the file's text or null. Returns what was found, or null.
 *
 * An own task whose repository has none sends the agent to work with nothing to run: it ends "completed" and the hidden tests, not
 * the agent's say, decide. The lint refuses such a task so the owner adds the command to the repository's own files at the base
 * (they are in the image) or drops the task.
 */
export function findCheckCommand(read) {
  const text = (p) => read(p) ?? '';
  try {
    if (JSON.parse(text('package.json') || '{}').scripts?.test) return 'package.json scripts.test';
  } catch {
    // not JSON: keep looking
  }
  if (read('Cargo.toml') !== null) return 'Cargo.toml (cargo test)';
  if (read('go.mod') !== null) return 'go.mod (go test)';
  for (const f of ['Makefile', 'makefile', 'GNUmakefile', 'justfile', 'Justfile']) if (/^test\s*:/m.test(text(f))) return `${f} test target`;
  for (const f of ['pytest.ini', 'tox.ini', 'conftest.py']) if (read(f) !== null) return f;
  if (/\[tool\.pytest|\[pytest\]|\[tool\.tox/.test(`${text('pyproject.toml')}${text('setup.cfg')}`)) return 'Python test configuration';
  const said = /\b(cargo (?:nextest run|test)|npm (?:run )?test|pnpm (?:run )?test|yarn test|go test|pytest|make test)\b/.exec(`${text('README.md')}\n${text('CONTRIBUTING.md')}`);
  return said ? `${said[1]} (named in the README)` : null;
}
