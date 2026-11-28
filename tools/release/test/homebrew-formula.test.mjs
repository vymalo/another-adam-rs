// Golden tests of write-homebrew-formula.mjs on fixed inputs. `node --test "tools/release/test/*.test.mjs"`.
// Regenerate the goldens after an intended change with UPDATE_GOLDEN=1.
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import { licenseOf, parseSums, readmeWithRow, writeTap, formula } from "../write-homebrew-formula.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const read = (...p) => readFileSync(join(here, ...p), "utf8");
const sumsText = read("fixtures", "SHA256SUMS");
const cargo = readFileSync(join(here, "..", "..", "..", "Cargo.toml"), "utf8");

function golden(name, actual) {
  const path = join(here, "golden", name);
  if (process.env.UPDATE_GOLDEN) writeFileSync(path, actual);
  assert.equal(actual, readFileSync(path, "utf8"), `${name} differs from its golden (UPDATE_GOLDEN=1 rewrites it)`);
}

test("the formula for fixed checksums is the golden", () => {
  const dir = mkdtempSync(join(tmpdir(), "tap-"));
  try {
    writeFileSync(join(dir, "README.md"), read("fixtures", "README.before.md"));
    writeTap({ tapDir: dir, version: "0.6.1", sumsText, cargoToml: cargo });
    golden("adam-agent.rb", readFileSync(join(dir, "Formula", "adam-agent.rb"), "utf8"));
    golden("README.after.md", readFileSync(join(dir, "README.md"), "utf8"));
    // Writing again changes nothing: the README row is an upsert.
    writeTap({ tapDir: dir, version: "0.6.1", sumsText, cargoToml: cargo });
    assert.equal(readFileSync(join(dir, "README.md"), "utf8"), read("golden", "README.after.md"));
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("the licence is the workspace's", () => {
  assert.equal(licenseOf(cargo), 'any_of: ["MIT", "Apache-2.0"]');
  assert.equal(licenseOf('[workspace.package]\nlicense = "MIT"\n'), '"MIT"');
  assert.throws(() => licenseOf('[workspace.package]\nlicense = "MIT WITH LLVM-exception"\n'));
  assert.throws(() => licenseOf("[package]\nname = 1\n"));
});

test("a missing archive, a bad version and a duplicate line are refused", () => {
  const sums = parseSums(sumsText);
  const license = licenseOf(cargo);
  assert.throws(() => formula({ version: "0.6.2", sums, license }), /no line for adam-agent_0.6.2_darwin_arm64/);
  assert.throws(() => formula({ version: "v0.6.1", sums, license }), /not a stable/);
  assert.throws(() => parseSums(`${sumsText}${sumsText.split("\n")[0]}\n`), /twice/);
  assert.throws(() => parseSums("nonsense\n"), /unreadable/);
});

test("the README row of another package survives and rows stay sorted", () => {
  const first = readmeWithRow(read("fixtures", "README.before.md"));
  const other = "| aapctl | `brew install vymalo/tap/aapctl` | a CLI |";
  const both = readmeWithRow(first, other);
  assert.match(both, /\| aapctl [^\n]*\n\| adam-agent /);
  assert.equal(readmeWithRow(both), both);
  assert.throws(() => readmeWithRow("<!-- cli-packages:end -->\n<!-- cli-packages:begin -->\n"));
});
