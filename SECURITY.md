# Security policy

## Report a vulnerability privately

Use GitHub's private vulnerability reporting: open the repository's **Security** tab and choose
**Report a vulnerability**, or go to
<https://github.com/vymalo/another-adam-rs/security/advisories/new>.

**Do not open a public issue, a pull request or a discussion for a vulnerability.** A public report gives everyone the
details before there is a fix.

## What to include

- What is affected: the crate, binary, chart or image, and the commit or image tag.
- What an attacker can do, and what they need first (a bearer token, a task, access to a run's commands).
- Steps to reproduce, or a proof of concept. The easiest is the local stack, which runs against mocks with dummy
  credentials: [Run it locally](docs/guides/run-locally.md).
- Anything you already know about a fix.

Please reproduce against your own machine or cluster. Do not test against a deployment you do not own, and do not read
or change other people's data.

## Scope

In scope: the code in this repository, the coder image (it also carries `adam-agent`), the operator image, and the Helm
charts in `deploy/`. Of particular interest: a run reading or changing another run's files, a command reaching the
coder's secrets, a way round the access tier or the checks a tool enforces, and a secret in a log or a file. A weakness
in a third-party component we use is best reported to that project; tell us too if our configuration makes it worse.

## Known limitations need no report

Some limits are already written down. Reporting them privately adds nothing; a fix or a better idea is welcome as an
ordinary issue or pull request.

- The limits of run-pod isolation, including local mode and Podman, which this repository documents as not protected:
  [ADR 0041, "Known limits, not built"](docs/decisions/0041-a-run-pod-sees-only-its-own-run.md#known-limits-not-built).
- The hardening work on the [product roadmap](https://github.com/vymalo/another-agentic-system/blob/main/ROADMAP.md):
  the tickets labelled `security`.
- Known gaps recorded in [`docs/roadmap.md`](docs/roadmap.md).

## Supported versions

Fixes land on `main`, and the coder image is rebuilt from it. No older branch is maintained.
