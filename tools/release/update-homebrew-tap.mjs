// Opens or updates the one pull request that moves vymalo/homebrew-tap to a release of adam-agent.
// Node standard library, `git` and `gh`. It runs in release.yml's `adam-agent-tap` job with the release App's
// token (GH_TOKEN) scoped to this repository and the tap.
//
//   node update-homebrew-tap.mjs <tap-checkout> <base-branch> <version> <SHA256SUMS> <Cargo.toml>
//
// Monotonic: the bot branch is rebuilt from the tap's current default branch, never merged into, and a version
// older than the one the tap or the open branch already has is not written ("stale"), so a re-run of an old
// release cannot move the tap backwards. Only this script writes `bot/adam-agent`.
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { REPO, writeTap } from "./write-homebrew-formula.mjs";

const BRANCH = "bot/adam-agent";
const TAP = "vymalo/homebrew-tap";
const FORMULA = "Formula/adam-agent.rb";
const FILES = [FORMULA, "README.md"];

// Git smart HTTP wants Basic authentication, unlike GitHub's REST API. The token goes through the environment,
// never through an argument that a log could print.
export function gitAuthEnvironment(token) {
  if (!token) return {};
  return {
    GIT_CONFIG_COUNT: "1",
    GIT_CONFIG_KEY_0: "http.https://github.com/.extraheader",
    GIT_CONFIG_VALUE_0: `AUTHORIZATION: basic ${Buffer.from(`x-access-token:${token}`).toString("base64")}`,
  };
}

export function compareVersions(left, right) {
  const a = left.split(".").map(BigInt);
  const b = right.split(".").map(BigInt);
  for (let i = 0; i < 3; i++) if (a[i] !== b[i]) return a[i] > b[i] ? 1 : -1;
  return 0;
}

export function updateTap({ cwd, base, version, sumsText, cargoToml, run }) {
  if (!/^\d+\.\d+\.\d+$/.test(version)) throw new Error("expected a stable X.Y.Z version");
  const execute = run ?? ((command, args) => execFileSync(command, args, {
    cwd,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, ...gitAuthEnvironment(process.env.GH_TOKEN) },
  }).trim());
  const git = (...args) => execute("git", args);
  git("check-ref-format", `refs/heads/${base}`);
  if (git("status", "--porcelain")) throw new Error("the tap checkout must be clean before updating");
  git("fetch", "origin", `+refs/heads/${base}:refs/remotes/origin/${base}`);
  const remoteHead = git("ls-remote", "--heads", "origin", `refs/heads/${BRANCH}`).split(/\s/)[0];
  if (remoteHead) git("fetch", "origin", `+refs/heads/${BRANCH}:refs/remotes/origin/${BRANCH}`);
  for (const ref of [`origin/${base}`, ...(remoteHead ? [`origin/${BRANCH}`] : [])]) {
    if (!git("ls-tree", "--name-only", ref, "--", FORMULA)) continue;
    const existing = git("show", `${ref}:${FORMULA}`).match(/^\s*version "(\d+\.\d+\.\d+)"/m)?.[1];
    if (!existing) throw new Error(`cannot read the current version in ${ref}:${FORMULA}`);
    if (compareVersions(version, existing) < 0) return "stale";
  }
  // Rebuild the branch from the fresh default branch, keeping unrelated tap changes, and lease the push
  // against the remote commit observed above.
  git("checkout", "-B", BRANCH, `origin/${base}`);
  writeTap({ tapDir: cwd, version, sumsText, cargoToml });
  git("add", "--", ...FILES);
  if (!git("diff", "--cached", "--name-only", "--", ...FILES)) return "unchanged";
  const desiredTree = git("write-tree");
  const pendingTree = remoteHead ? git("rev-parse", `${remoteHead}^{tree}`) : "";
  if (desiredTree !== pendingTree) {
    git("-c", "user.name=release-please[bot]", "-c", "user.email=release-please[bot]@users.noreply.github.com",
      "commit", "-m", `chore: update adam-agent to ${version}`);
    git("push", `--force-with-lease=refs/heads/${BRANCH}:${remoteHead}`, "origin", `HEAD:refs/heads/${BRANCH}`);
  } else {
    git("reset", "--soft", remoteHead);
  }
  const open = JSON.parse(execute("gh", ["pr", "list", "--repo", TAP, "--state", "open", "--head", BRANCH, "--base", base, "--json", "url"]));
  if (open.length > 1) throw new Error("several open tap pull requests for adam-agent need a person to reconcile them");
  const dir = mkdtempSync(join(tmpdir(), "tap-pr-body-"));
  try {
    const bodyFile = join(dir, "body.md");
    const release = `https://github.com/${REPO}/releases/tag/v${version}`;
    const run_ = process.env.GITHUB_RUN_ID
      ? `https://github.com/${REPO}/actions/runs/${process.env.GITHUB_RUN_ID}`
      : `https://github.com/${REPO}/actions/workflows/release.yml`;
    writeFileSync(bodyFile, `## Summary\n\nUpdate the \`adam-agent\` formula to ${version}, from [the release](${release}).\n\n## Verification\n\nThe [release workflow](${run_}) built the four archives, signed their SHA256SUMS and installed the formula with Homebrew on Linux and macOS (arm64 and x86_64), running \`adam-agent --version\`, before it opened this pull request. The checksums are those of the release's own SHA256SUMS.\n\n## Risk\n\nThe macOS binaries are not notarized. Review the version and the four checksums.\n\nMechanically generated; no person has approved it.\n`);
    const options = ["--title", `chore: update adam-agent to ${version}`, "--body-file", bodyFile];
    if (open.length) {
      execute("gh", ["pr", "edit", open[0].url, ...options]);
      return "updated";
    }
    execute("gh", ["pr", "create", "--repo", TAP, "--base", base, "--head", BRANCH, ...options]);
    return "created";
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  if (process.env.GITHUB_ACTIONS === "true" && process.env.GH_TOKEN) {
    // The App action masks the token, not the encoded git credential derived from it.
    console.log(`::add-mask::${gitAuthEnvironment(process.env.GH_TOKEN).GIT_CONFIG_VALUE_0.slice("AUTHORIZATION: basic ".length)}`);
  }
  const [cwd, base, version, sums, cargo] = process.argv.slice(2);
  if (![cwd, base, version, sums, cargo].every(Boolean)) {
    throw new Error("usage: update-homebrew-tap.mjs <tap-checkout> <base-branch> <version> <SHA256SUMS> <Cargo.toml>");
  }
  console.log(updateTap({ cwd: resolve(cwd), base, version, sumsText: readFileSync(sums, "utf8"), cargoToml: readFileSync(cargo, "utf8") }));
}
