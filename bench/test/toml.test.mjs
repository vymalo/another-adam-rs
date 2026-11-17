import assert from 'node:assert/strict';
import { test } from 'node:test';
import { TomlError, parseToml } from '../lib/toml.mjs';

test('the subset a task.toml uses is read', () => {
  const t = parseToml(`# a task
[agent]
network_mode = "no-network"   # trailing comment

[verifier]
network_mode = 'public'

[environment]
cpus = 2
memory_mb = 4_096
ratio = 1.5
big = 1e3
ok = true
tags = ["a", 'b',
  "c", # inside
]
limits = { cpu = 1, mem = "2g" }
dotted.key = "x"
"quoted key" = 1
`);
  assert.equal(t.agent.network_mode, 'no-network');
  assert.equal(t.verifier.network_mode, 'public');
  assert.deepEqual(t.environment, { cpus: 2, memory_mb: 4096, ratio: 1.5, big: 1000, ok: true, tags: ['a', 'b', 'c'], limits: { cpu: 1, mem: '2g' }, dotted: { key: 'x' }, 'quoted key': 1 });
});

test('strings: escapes, unicode, multi-line basic and literal', () => {
  const t = parseToml('a = "x\\ty\\u00e9\\\\"\nb = """\nfirst\nsecond \\\n   joined"""\nc = \'\'\'\nraw \\n\'\'\'\nd = \'C:\\path\'\n');
  assert.equal(t.a, 'x\tyé\\');
  assert.equal(t.b, 'first\nsecond joined');
  assert.equal(t.c, 'raw \\n');
  assert.equal(t.d, 'C:\\path');
});

test('arrays of tables and nested tables', () => {
  const t = parseToml('[[step]]\nname = "a"\n[[step]]\nname = "b"\n[x.y]\nz = 1\n[x.w]\nz = 2\n');
  assert.deepEqual(t.step, [{ name: 'a' }, { name: 'b' }]);
  assert.deepEqual(t.x, { y: { z: 1 }, w: { z: 2 } });
});

test('CRLF line endings are read', () => {
  assert.deepEqual(parseToml('a = 1\r\n[t]\r\nb = "x"\r\n'), { a: 1, t: { b: 'x' } });
});

const refuses = (text, re) => assert.throws(() => parseToml(text), (e) => e instanceof TomlError && re.test(e.message) && Number.isInteger(e.line), text);

test('what it cannot read is refused with a line number', () => {
  refuses('a = 1\na = 2\n', /set twice/);
  refuses('[t]\n[t]\n', /declared twice/);
  refuses('a = 1979-05-27\n', /not supported/);
  refuses('a = 0xFF\n', /not supported/);
  refuses('a = inf\n', /not supported/);
  refuses('a = "open\n', /not closed/);
  refuses('a = [1, 2\n', /not closed|expected/);
  refuses('a = 1 2\n', /unexpected text/);
  refuses('= 1\n', /key was expected/);
  refuses('a 1\n', /"=" was expected/);
  refuses('a = "\\q"\n', /bad escape/);
  refuses('a = 1__0\n', /not supported/);
  refuses('a = 1\n[a]\n', /not a table/);
  refuses('__proto__ = 1\n', /not allowed/);
  refuses('[t\nx = 1\n', /not closed/);
});

test('the line number is the line of the fault', () => {
  try {
    parseToml('a = 1\nb = 2\nc = ???\n');
    assert.fail('should refuse');
  } catch (e) {
    assert.ok(e instanceof TomlError);
    assert.equal(e.line, 3);
  }
});

test('a key named __proto__ cannot reach the prototype', () => {
  assert.throws(() => parseToml('[__proto__]\nx = 1\n'), TomlError);
  assert.equal({}.x, undefined);
});
