---
description: Review the open pull request of a repository you name, and say what to change.
argument-hint: "<repository> [pull request number]"
---
Review an open pull request. Do not change anything and do not push.

What to review: $ARGUMENTS

If that names no repository, ask which one before you start. Read the pull request, its diff and
its checks. Report what is wrong or risky first, by file and line, then what is good, and end
with one line: approve, or request changes.
