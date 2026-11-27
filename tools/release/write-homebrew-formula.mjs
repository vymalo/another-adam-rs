// Writes Formula/adam-agent.rb into a checkout of the Homebrew tap, and the row of the tap's README that lists it.
// Node standard library only. The formula is a function of three things: the release version, the SHA-256 of
// each archive (read from the release's SHA256SUMS) and the licence in the workspace Cargo.toml.
//
//   node write-homebrew-formula.mjs [--no-readme] <tap-dir> <version> <SHA256SUMS> <Cargo.toml>
//
// release.yml's `adam-agent-tap` job and tools/release/homebrew-smoke.sh call it; the golden test is
// tools/release/test/homebrew-formula.test.mjs (ADR 0084, amended 2026-11-27).
import { mkdirSync, readFileSync, writeFileSync, existsSync } from "node:fs";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const REPO = "vymalo/another-adam-rs";
export const DESC = "Serve an agent folder over A2A: one binary, SQLite on a laptop";

// The four archives of a release, named adam-agent_<version>_<os>_<arch>.tar.gz.
export const TARGETS = [
  { os: "darwin", arch: "arm64", block: ["on_macos", "on_arm"] },
  { os: "darwin", arch: "amd64", block: ["on_macos", "on_intel"] },
  { os: "linux", arch: "arm64", block: ["on_linux", "on_arm"] },
  { os: "linux", arch: "amd64", block: ["on_linux", "on_intel"] },
];

export const archiveName = (version, { os, arch }) => `adam-agent_${version}_${os}_${arch}.tar.gz`;

export function assertVersion(version) {
  if (!/^\d+\.\d+\.\d+$/.test(version)) throw new Error(`not a stable X.Y.Z version: ${version}`);
}

// `sha256sum` and `shasum -a 256` both write "<hex>  <name>" (a `*` before the name for binary mode).
export function parseSums(text) {
  const sums = new Map();
  for (const line of text.split("\n")) {
    if (!line.trim()) continue;
    const m = line.match(/^([a-f0-9]{64}) [ *](\S+)$/);
    if (!m) throw new Error(`unreadable SHA256SUMS line: ${line}`);
    if (sums.has(m[2])) throw new Error(`SHA256SUMS names ${m[2]} twice`);
    sums.set(m[2], m[1]);
  }
  return sums;
}

// `license` of [workspace.package]: "MIT OR Apache-2.0" is any_of, "AND" is all_of, one id is a string.
export function licenseOf(cargoToml) {
  const section = cargoToml.split(/^\[/m).find((s) => s.startsWith("workspace.package]"));
  const id = section?.match(/^license\s*=\s*"([^"]+)"/m)?.[1];
  if (!id) throw new Error("no license in [workspace.package] of Cargo.toml");
  const list = (ids) => `[${ids.map((i) => `"${i}"`).join(", ")}]`;
  if (/ WITH /.test(id) || /[()]/.test(id)) throw new Error(`license expression not supported: ${id}`);
  if (id.includes(" OR ") && !id.includes(" AND ")) return `any_of: ${list(id.split(" OR "))}`;
  if (id.includes(" AND ") && !id.includes(" OR ")) return `all_of: ${list(id.split(" AND "))}`;
  if (/ (OR|AND) /.test(id)) throw new Error(`license expression not supported: ${id}`);
  return `"${id}"`;
}

export function formula({ version, sums, license }) {
  assertVersion(version);
  const base = `https://github.com/${REPO}/releases/download/v${version}`;
  const stanza = (t, indent) => {
    const name = archiveName(version, t);
    const sha = sums.get(name);
    if (!sha) throw new Error(`SHA256SUMS has no line for ${name}`);
    const pad = " ".repeat(indent);
    return `${pad}url "${base}/${name}"\n${pad}sha256 "${sha}"\n`;
  };
  let out = `class AdamAgent < Formula
  desc "${DESC}"
  homepage "https://github.com/${REPO}"
  version "${version}"
  license ${license}

`;
  for (const outer of ["on_macos", "on_linux"]) {
    out += `  ${outer} do\n`;
    const inner = TARGETS.filter((t) => t.block[0] === outer);
    inner.forEach((t, i) => {
      out += `    ${t.block[1]} do\n${stanza(t, 6)}    end\n${i < inner.length - 1 ? "\n" : ""}`;
    });
    out += "  end\n\n";
  }
  out += `  def install
    bin.install "adam-agent"
  end

  test do
    system bin/"adam-agent", "--version"
  end
end
`;
  return out;
}

// The tap README lists the command-line packages in one table between two markers, one row each, so a second
// formula (aapctl) adds its row and never touches this one. A README with no table gets the section before
// "## Upgrade" (or at the end).
const BEGIN = "<!-- cli-packages:begin -->";
const END = "<!-- cli-packages:end -->";
export const ROW = `| adam-agent | \`brew install vymalo/tap/adam-agent\` | ${DESC}; [another-adam-rs](https://github.com/${REPO}) |`;

// The prose of the tap README that says "the app" learns that the tap has command-line tools too. Each sentence is
// replaced only where the old one is still there, so a second run, or a README already edited, changes nothing.
const PROSE = [
  [
    "The tap provides a macOS Cask and a Linux AppImage Formula, each for arm64 and x86_64. Windows is not distributed.",
    "The tap provides a macOS Cask and a Linux AppImage Formula, each for arm64 and x86_64, and [command-line tools](#command-line-tools). Windows is not distributed.",
  ],
  [
    "Then upgrade only this app:",
    "Then upgrade one package. A command-line tool: `brew upgrade vymalo/tap/adam-agent`. The desktop app:",
  ],
  [
    "Run the command for your platform:",
    "A command-line tool: `brew uninstall vymalo/tap/adam-agent`. The desktop app, for your platform:",
  ],
];

export function readmeWithRow(readme, row = ROW) {
  for (const [from, to] of PROSE) readme = readme.replace(from, to);
  const name = row.split("|")[1].trim();
  const key = `| ${name} |`;
  const start = readme.indexOf(BEGIN);
  const stop = readme.indexOf(END);
  if (start === -1 && stop === -1) {
    const section = `## Command-line tools

Command-line programs for Linux and macOS, arm64 and x86_64, from [GitHub Releases](https://github.com/${REPO}/releases). \`brew install\` names the formula; the SHA-256 of each download is in the formula.

${BEGIN}
| Formula | Install | What it is |
|---|---|---|
${row}
${END}

`;
    const at = readme.indexOf("\n## Upgrade");
    if (at === -1) return `${readme.replace(/\n*$/, "\n")}\n${section.replace(/\n+$/, "\n")}`;
    return `${readme.slice(0, at + 1)}${section}${readme.slice(at + 1)}`;
  }
  if (start === -1 || stop < start) throw new Error("the cli-packages markers of the tap README are not a pair");
  const head = readme.slice(0, stop);
  const rows = head.slice(head.indexOf("\n", start) + 1).split("\n").filter(Boolean);
  const kept = rows.filter((r) => !r.startsWith(key));
  const i = kept.findIndex((r) => r.startsWith("|---"));
  const body = [...kept.slice(0, i + 1), ...[...kept.slice(i + 1), row].sort(), ""];
  return `${readme.slice(0, head.indexOf("\n", start) + 1)}${body.join("\n")}${readme.slice(stop)}`;
}

export function writeTap({ tapDir, version, sumsText, cargoToml, readme = true }) {
  const text = formula({ version, sums: parseSums(sumsText), license: licenseOf(cargoToml) });
  mkdirSync(join(tapDir, "Formula"), { recursive: true });
  writeFileSync(join(tapDir, "Formula", "adam-agent.rb"), text);
  const files = ["Formula/adam-agent.rb"];
  const path = join(tapDir, "README.md");
  if (readme && existsSync(path)) {
    writeFileSync(path, readmeWithRow(readFileSync(path, "utf8")));
    files.push("README.md");
  }
  return files;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const args = process.argv.slice(2);
  const readme = !args.includes("--no-readme");
  const [tapDir, version, sums, cargo] = args.filter((a) => a !== "--no-readme");
  if (![tapDir, version, sums, cargo].every(Boolean)) {
    throw new Error("usage: write-homebrew-formula.mjs [--no-readme] <tap-dir> <version> <SHA256SUMS> <Cargo.toml>");
  }
  for (const f of writeTap({ tapDir, version, sumsText: readFileSync(sums, "utf8"), cargoToml: readFileSync(cargo, "utf8"), readme })) {
    console.log(`wrote ${f}`);
  }
}
