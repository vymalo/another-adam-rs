// A TOML reader for the subset a task's `task.toml` uses, with no dependency.
//
// Supported: comments, `[table]` and `[a.b]` headers, `[[array.of.tables]]`, bare, quoted and dotted keys,
// basic strings (with escapes), literal strings, multi-line strings of both kinds, integers (with `_`),
// floats, booleans, arrays (also over several lines) and inline tables. Not supported, and refused with the
// line number: dates and times, `inf`/`nan`, hex/octal/binary integers. Duplicate keys are refused.
export class TomlError extends Error {
  constructor(line, message) {
    super(`task.toml line ${line}: ${message}`);
    this.name = 'TomlError';
    this.line = line;
  }
}

const ESCAPES = { b: '\b', t: '\t', n: '\n', f: '\f', r: '\r', '"': '"', '\\': '\\' };

class Reader {
  constructor(text) {
    this.s = text.replace(/\r\n/g, '\n');
    this.i = 0;
  }
  get line() {
    let n = 1;
    for (let k = 0; k < this.i && k < this.s.length; k++) if (this.s[k] === '\n') n++;
    return n;
  }
  fail(msg) {
    throw new TomlError(this.line, msg);
  }
  peek() {
    return this.s[this.i];
  }
  eof() {
    return this.i >= this.s.length;
  }
  skipSpaces() {
    while (this.peek() === ' ' || this.peek() === '\t') this.i++;
  }
  skipComment() {
    if (this.peek() === '#') while (!this.eof() && this.peek() !== '\n') this.i++;
  }
  /** Spaces, comments and line breaks. */
  skipBlank() {
    for (;;) {
      this.skipSpaces();
      this.skipComment();
      if (this.peek() === '\n') this.i++;
      else return;
    }
  }
  endOfLine() {
    this.skipSpaces();
    this.skipComment();
    if (!this.eof() && this.peek() !== '\n') this.fail('unexpected text after a value');
    if (this.peek() === '\n') this.i++;
  }

  basicString() {
    const multi = this.s.startsWith('"""', this.i);
    this.i += multi ? 3 : 1;
    if (multi && this.peek() === '\n') this.i++;
    let out = '';
    for (;;) {
      if (this.eof()) this.fail('a string is not closed');
      const c = this.s[this.i];
      if (multi ? this.s.startsWith('"""', this.i) : c === '"') {
        this.i += multi ? 3 : 1;
        return out;
      }
      if (c === '\n' && !multi) this.fail('a string is not closed');
      if (c === '\\') {
        const e = this.s[this.i + 1];
        if (e !== undefined && Object.hasOwn(ESCAPES, e)) {
          out += ESCAPES[e];
          this.i += 2;
        } else if (e === 'u' || e === 'U') {
          const len = e === 'u' ? 4 : 8;
          const hex = this.s.slice(this.i + 2, this.i + 2 + len);
          if (!new RegExp(`^[0-9a-fA-F]{${len}}$`).test(hex)) this.fail('a bad unicode escape');
          const cp = parseInt(hex, 16);
          if (cp > 0x10ffff || (cp >= 0xd800 && cp <= 0xdfff)) this.fail('a bad unicode escape');
          out += String.fromCodePoint(cp);
          this.i += 2 + len;
        } else if (multi && /[ \t\n]/.test(e ?? '')) {
          this.i++;
          while (/[ \t\n]/.test(this.s[this.i] ?? 'x')) this.i++;
        } else this.fail('a bad escape in a string');
      } else {
        out += c;
        this.i++;
      }
    }
  }

  literalString() {
    const multi = this.s.startsWith("'''", this.i);
    this.i += multi ? 3 : 1;
    if (multi && this.peek() === '\n') this.i++;
    const end = this.s.indexOf(multi ? "'''" : "'", this.i);
    if (end < 0) this.fail('a string is not closed');
    const out = this.s.slice(this.i, end);
    if (!multi && out.includes('\n')) this.fail('a string is not closed');
    this.i = end + (multi ? 3 : 1);
    return out;
  }

  key() {
    const parts = [];
    for (;;) {
      this.skipSpaces();
      let k;
      if (this.peek() === '"') k = this.basicString();
      else if (this.peek() === "'") k = this.literalString();
      else {
        const m = /^[A-Za-z0-9_-]+/.exec(this.s.slice(this.i, this.i + 200));
        if (!m) this.fail('a key was expected');
        k = m[0];
        this.i += k.length;
      }
      parts.push(k);
      this.skipSpaces();
      if (this.peek() === '.') this.i++;
      else return parts;
    }
  }

  value() {
    const c = this.peek();
    if (c === '"') return this.basicString();
    if (c === "'") return this.literalString();
    if (c === '[') return this.array();
    if (c === '{') return this.inlineTable();
    const m = /^[^\s,\]}#]+/.exec(this.s.slice(this.i, this.i + 200));
    if (!m) this.fail('a value was expected');
    const t = m[0];
    this.i += t.length;
    if (t === 'true') return true;
    if (t === 'false') return false;
    if (/^[+-]?(0|[1-9][0-9_]*)$/.test(t) && !/__|_$/.test(t)) return Number(t.replace(/_/g, ''));
    if (/^[+-]?(0|[1-9][0-9_]*)(\.[0-9_]+)?([eE][+-]?[0-9_]+)?$/.test(t) && /[.eE]/.test(t) && !/__|_$|_[.eE]|[.eE]_/.test(t)) return Number(t.replace(/_/g, ''));
    this.i -= t.length;
    return this.fail(`this value is not supported: ${JSON.stringify(t.slice(0, 20))}`);
  }

  array() {
    this.i++;
    const out = [];
    for (;;) {
      this.skipBlank();
      if (this.peek() === ']') {
        this.i++;
        return out;
      }
      if (this.eof()) this.fail('an array is not closed');
      out.push(this.value());
      this.skipBlank();
      if (this.peek() === ',') this.i++;
      else if (this.peek() !== ']') this.fail('a "," or "]" was expected in an array');
    }
  }

  inlineTable() {
    this.i++;
    const out = {};
    this.skipSpaces();
    if (this.peek() === '}') {
      this.i++;
      return out;
    }
    for (;;) {
      this.skipSpaces();
      const k = this.key();
      this.skipSpaces();
      if (this.peek() !== '=') this.fail('"=" was expected');
      this.i++;
      this.skipSpaces();
      assign(out, k, this.value(), this);
      this.skipSpaces();
      if (this.peek() === ',') this.i++;
      else if (this.peek() === '}') {
        this.i++;
        return out;
      } else this.fail('a "," or "}" was expected in an inline table');
    }
  }
}

const isTable = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);

function descend(root, path, reader) {
  let node = root;
  for (const part of path) {
    if (part === '__proto__') reader.fail('a key name is not allowed');
    let next = Object.hasOwn(node, part) ? node[part] : undefined;
    if (next === undefined) next = node[part] = {};
    if (Array.isArray(next)) next = next[next.length - 1];
    if (!isTable(next)) reader.fail(`${JSON.stringify(part)} is not a table`);
    node = next;
  }
  return node;
}

function assign(node, path, value, reader) {
  const parent = descend(node, path.slice(0, -1), reader);
  const last = path[path.length - 1];
  if (last === '__proto__') reader.fail('a key name is not allowed');
  if (Object.hasOwn(parent, last)) reader.fail(`the key ${JSON.stringify(last)} is set twice`);
  parent[last] = value;
}

export function parseToml(text) {
  const r = new Reader(String(text));
  const root = {};
  let current = root;
  const declared = new Set();
  for (;;) {
    r.skipBlank();
    if (r.eof()) return root;
    if (r.peek() === '[') {
      const array = r.s.startsWith('[[', r.i);
      r.i += array ? 2 : 1;
      const path = r.key();
      r.skipSpaces();
      if (!r.s.startsWith(array ? ']]' : ']', r.i)) r.fail('a table header is not closed');
      r.i += array ? 2 : 1;
      r.endOfLine();
      const id = path.join('\u0000');
      if (array) {
        const parent = descend(root, path.slice(0, -1), r);
        const last = path[path.length - 1];
        if (!Object.hasOwn(parent, last)) parent[last] = [];
        if (!Array.isArray(parent[last])) r.fail(`${JSON.stringify(last)} is not an array of tables`);
        current = {};
        parent[last].push(current);
      } else {
        if (declared.has(id)) r.fail(`the table [${path.join('.')}] is declared twice`);
        declared.add(id);
        current = descend(root, path, r);
      }
      continue;
    }
    const k = r.key();
    r.skipSpaces();
    if (r.peek() !== '=') r.fail('"=" was expected');
    r.i++;
    r.skipSpaces();
    assign(current, k, r.value(), r);
    r.endOfLine();
  }
}
