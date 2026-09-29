---
name: release-notes
description: Drafts release notes from merged pull requests. Use when the user asks for a changelog or release notes.
license: Apache-2.0
compatibility: Needs the GitHub CLI.
metadata:
  version: "1.0"
allowed-tools: Bash(git log:*) Read
---
1. List the merged pull requests since the last tag.
2. Group them by label.

See [the style guide](references/style.md).
