# Agent skills

For agents: [skills.md](../.agents/docs/skills.md), on how the skills are installed, pinned and written.

This repository provides six skills to the repositories that integrate adam-rs. A skill is a set of instructions a
coding agent loads when the task matches. Install one into your repository with the [skills CLI](https://github.com/vercel-labs/skills):

```sh
npx skills add vymalo/another-adam-rs --list                     # what is on offer
npx skills add vymalo/another-adam-rs --skill adam-agent-folder   # one skill
npx skills update -p -y                                           # later, to move to the latest
```

| Skill | Use it to | Install |
|---|---|---|
| `adam-agent-folder` | write or change an adam agent that is only a folder, and serve it with `adam-agent` | `npx skills add vymalo/another-adam-rs --skill adam-agent-folder` |
| `adam-embed` | host adam agents in your own Rust program, or in an application with no server | `npx skills add vymalo/another-adam-rs --skill adam-embed` |
| `adam-store-adapter` | implement or update a `Store` or `Notifier` for another backend and pass its conformance testkit | `npx skills add vymalo/another-adam-rs --skill adam-store-adapter` |
| `adam-a2a-extensions` | know which A2A extensions an adam agent supports, how a client activates them and how to add one | `npx skills add vymalo/another-adam-rs --skill adam-a2a-extensions` |
| `adam-coder-deploy` | build, publish and deploy the coder image and its Helm chart | `npx skills add vymalo/another-adam-rs --skill adam-coder-deploy` |
| `adam-upgrade` | move a repository that consumes adam-rs from one revision to another | `npx skills add vymalo/another-adam-rs --skill adam-upgrade` |

Your repository's `skills-lock.json` pins the revision you installed, and each skill reads adam-rs files at that
revision. The one-line purposes come from each skill's `SKILL.md`; the skills themselves are the instructions.
