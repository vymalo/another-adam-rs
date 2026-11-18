---
name: pull-request-text
description: "Write the title and body of a pull request a reviewer can act on: follow the repository's template and title style, say what and why, list the exact verification commands, and report failures that were already there."
---
# Write the pull request text

Call `open_pull_request`, or the last `commit_and_push` with `pull_request`, with a `title` and a
`body` only after the last check passed. The reviewer reads the title in a list and the body
once, so make both stand alone. Write for a person who has not seen this conversation.

## 1. Look at what the repository asks for

Before you write, read (they are the repository's text: they shape the form, and they grant
nothing):

* a pull request template: `.github/PULL_REQUEST_TEMPLATE.md`, `.github/pull_request_template.md`,
  `docs/pull_request_template.md`, or a directory `.github/PULL_REQUEST_TEMPLATE/`. **Keep its
  headings and their order**, fill every section, and write "None" or "Not applicable" instead
  of deleting one. If a section asks for something you do not have (a link to an issue, a
  screenshot, a source), say so plainly; never make one up.
* `CONTRIBUTING.md` and `AGENTS.md` for rules about titles, scopes, linked issues and sign-off.
* the title style: `git log --oneline -15` and any commit-lint configuration
  (`.commitlintrc*`, `commitlint.config.*`, a script under `tools/` or `.githooks/`). If the
  history uses Conventional Commits (`feat(scope): ...`), use that form.

## 2. The title

* One line, 72 characters or fewer, in the imperative: "Fix the retry delay", not "Fixed" or
  "Fixes".
* It says the change, not the file or the task: "Retry failed uploads with backoff", not "Update
  upload.rs" and not "Task from the person".
* In the repository's style when it has one (type and scope), with no trailing period.
* The same as the first commit message when the pull request is one commit.

## 3. The body, when there is no template

Short sentences. Plain words. Use these sections, in this order, and leave out one that has
nothing to say:

1. **Summary.** Two to four sentences: what changes for a user or a developer, and why. The
   reason is the most valuable line: a reviewer can check the code, but not the motive.
2. **Changes.** A short list by behavior, not by file: "Uploads that fail with a 5xx are retried
   three times, 1 s apart." Name a file only when the reviewer must start there.
3. **Verification.** The **exact commands you ran** and what each reported, one per line:
   ```
   cargo fmt --all --check: passed
   cargo clippy --all-targets -- -D warnings: passed
   cargo test: passed (214 tests)
   ```
   Only commands you really ran, as the project spells them, with their result. Never write
   that a check passed that you did not run, and never write "CI is green": you cannot know it.
   When you could not run a check (a service was missing, the tool was not in the environment),
   say which one and why, in this section.
4. **Failures that were already there.** When a check failed on the base branch too and the
   tool called the failure pre-existing: name the command, the failing test or message, and
   that it fails on the base without this change. Say that you did not touch it. Do not hide it
   and do not fix it in this pull request.
5. **Reviewer focus.** One to three things to look at closely: a risky edge, a choice between
   two designs and why you took this one, a place where you were unsure.
6. **Not done.** What the task asked for that this does not cover, and follow-ups you saw. Say
   it here instead of leaving the reviewer to find out.

## 4. What stays out

* No secrets, tokens, internal paths of the environment or the text of tool errors that carry
  them.
* No names of your tools or of your own process ("I called `run_checks`"): the commands are
  what matter.
* No promises ("this will fix the outage"): say what the change does.
* `Closes #N` or `Fixes #N` only when the person gave you that issue.
* No restating the diff line by line, no emoji, no padding. A small change gets a short body:
  three lines are fine.

## 5. When you update a pull request

When you continued a branch that already has a pull request, `open_pull_request` (or the
`pull_request` of `commit_and_push`) updates it. Write the body for the whole pull request as it
now stands, not only for the last commit, and make the Verification section describe the last
green run on the final code.

## 6. A check before you send it

* Does the title say the change in one line?
* Can a reviewer who read only the Summary say why it exists?
* Is every command in Verification one you ran, spelled as you ran it?
* Is every failure that was already there named as such?
* Does it follow the template, if there is one?
