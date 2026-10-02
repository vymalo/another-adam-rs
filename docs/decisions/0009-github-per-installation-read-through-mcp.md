# 0009. GitHub per installation: an App or a token, and GitHub read through MCP

Status: **Accepted** (2026-10-01). The defaults are the ones the slice 7 plan proposed (its D7.1,
D7.6, D7.7 and D7.8); the owner may revisit them. This record covers how the coder authenticates to
GitHub, which `adam-workspace` and the coder build, and the two decisions around it that later changes
of the slice build: the coder's own tools stay in process, and GitHub is *read* through the official
GitHub MCP server. The status notes at the end say which are built.

## Context

The coder has one static `GITHUB_TOKEN` (`bin/adam-coder/src/config.rs`), scoped to
`ALLOWED_REPO_HOSTS` by `ScopedToken` (`crates/adam-workspace/src/credentials.rs`). *Verified
2026-10-01 at adam `2e259a3`: read both.* A personal access token belongs to a person, lives until
someone revokes it and reaches everything that person can. A deployment for a team wants an identity
of its own, scoped to the repositories it was installed on, whose credentials expire by themselves: a
**GitHub App installation**.

The `GitCredentials` port already asks for a token *per repository* ("static in the MVP; a broker
later"), and `git` and the REST client already take a token per call. So the seam exists; what is
missing is a source of tokens that is not a constant.

*Verified 2026-10-01 against docs.github.com* ("Generating a JSON Web Token (JWT) for a GitHub App",
the REST reference of `POST /app/installations/{installation_id}/access_tokens`, "Authenticating as
a GitHub App installation"):

* The App signs a JWT with `RS256`. Its `iat` is best set 60 seconds in the past against clock
  drift, its `exp` is at most 10 minutes ahead, and its `iss` is the App's client ID or its
  application ID.
* The JWT, as a bearer, is traded at `POST /app/installations/{id}/access_tokens` for
  `201 {"token", "expires_at", ...}`. A bad JWT, a forbidden or an unknown installation is `401`,
  `403` or `404`.
* An installation access token expires after one hour. It is the password of `x-access-token` for
  git over HTTPS and a bearer for the REST API.
* GitHub began a staged rollout of a longer, stateless token format on 2026-04-27, so nothing may
  depend on a token's length or characters.

The official GitHub MCP server (`github/github-mcp-server`). The slice 7 plan read its README on
2026-10-01; the change that ships it checked the rest against the **source at the tag `v1.12.2`**
(commit `85598ba`) **and the binary of its image**, run here. *Verified 2026-10-01* (source:
`cmd/github-mcp-server/main.go`, `internal/ghmcp/`, `internal/githubapp/`, `pkg/http/transport/`; the
image `ghcr.io/github/github-mcp-server:v1.12.2`, index digest
`sha256:508a0857ec762b1ab1cece29193345b501fab1dd9d1228a7b617062954cecac6`, anonymous pull, built
2026-09-16; `v1.13.0` was published the same day and was **not** checked):

* **The image.** The binary is `/server/github-mcp-server`, a static Go binary (`CGO_ENABLED=0`) in a
  distroless image whose entrypoint is that binary and whose default command is `stdio`; the index has
  `linux/amd64` and `linux/arm64`. `docker/coder/Dockerfile` copies the binary out and pins the image by
  tag and digest.
* **The command line.** `github-mcp-server stdio` with `--read-only` and `--toolsets` (a comma list; the
  names `context`, `repos`, `issues` and `pull_requests` exist) and the global `--gh-host`. Every flag has
  an environment variable (prefix `GITHUB_`, dashes as underscores), so `GITHUB_HOST` is `--gh-host`.
* **The credentials.** `GITHUB_PERSONAL_ACCESS_TOKEN`; or a GitHub App: `GITHUB_APP_ID`,
  `GITHUB_APP_INSTALLATION_ID` and `GITHUB_APP_PRIVATE_KEY_PATH` or `GITHUB_APP_PRIVATE_KEY`. It mints
  the installation token itself, signs the JWT with `iss` as a **string** (`"12345"`; the coder's own
  JWT has a number when the ID is one, and GitHub takes either), refreshes it before it expires, and
  calls the REST API with it. A token **and** an App are an error ("mutually exclusive"); an App without
  its key is an error that names `GITHUB_APP_PRIVATE_KEY_PATH`.
* **An empty variable is an unset one.** Every test is on the value (`token == ""`, `appID != ""`), and
  the server was run both ways with the five variables the coder's `mcp.json` passes: a token and the
  three App variables empty (calls GitHub with the token), and an **empty** token with an App (traded a
  JWT at the installation's token endpoint, and called GitHub with the token it got:
  `bin/adam-coder/tests/binary.rs`, `the_embedded_agent_connects_the_real_github_mcp_server`). An empty
  `GITHUB_HOST` is github.com. So `${VAR:-}` in `mcp.json` is enough and adam-mcp needs no fallback.
* **`tools/list` needs no token.** The server starts and lists its tools without calling GitHub, with no
  credential at all: with `--read-only --toolsets context,repos,issues,pull_requests` it lists 25 tools,
  and the twelve of the coder's allow-list are among them under exactly the names of the plan. A
  *call* with no credential starts the server's OAuth device login (a tool result that says "Visit
  https://github.com/login/device and enter the code ..."), so the image smoke test lists tools only.
  A **classic** token makes the server ask GitHub for the token's scopes once at startup (`HEAD`,
  `X-OAuth-Scopes`) and hide the tools whose scopes it lacks; the twelve survive any scope set tried (none,
  `public_repo`, `repo`, `read:org`; the two hidden are the organisation's teams).
* **`--read-only`** removes every write tool: without it the list holds `create_branch`,
  `create_or_update_file`, `add_issue_comment` and the rest, with it none.

## Decision

1. **The coder is authenticated one of two ways, exactly one.** A **token** (`GITHUB_TOKEN`, as
   before) or an **App installation** (`GITHUB_APP_ID`, `GITHUB_APP_INSTALLATION_ID` and one of
   `GITHUB_APP_PRIVATE_KEY_PATH` or `GITHUB_APP_PRIVATE_KEY`). The names are the ones
   github-mcp-server reads, so one set of variables serves both. Both set, a partial App set, or
   neither is a configuration error (exit 78) that lists every problem and never a value.
2. **The key is parsed at startup.** A file that cannot be read, a PEM with no private key, a key that
   is not RSA of 2048 to 8192 bits, or an encrypted one stops the process, so a deployment learns of
   it when it rolls out and not at the first push. PKCS#1 (what GitHub lets the owner download) and
   PKCS#8 are accepted. A key in a variable accepts `\n` escapes, for the places that cannot hold a
   multi-line value. A rotated key needs a restart.
3. **`adam_workspace::GitHubApp` mints the tokens, behind the existing port.** It implements
   `GitCredentials`: `token_for` returns the cached installation token while more than five minutes
   of it are left, and otherwise mints one, under a lock so that however many callers wait there is
   one request. The JWT is `{"alg":"RS256","typ":"JWT"}` with `iat` now minus 60 s, `exp` now plus
   540 s and `iss` the App (a number when the ID is one, a string for a client ID), signed with
   `aws-lc-rs` and `RSA_PKCS1_SHA256`. The request carries `Accept: application/vnd.github+json` and
   `X-GitHub-Api-Version: 2022-11-28`. The token endpoint is `{GITHUB_API_URL}/app/installations/{id}/access_tokens`,
   so GitHub Enterprise Server and a mock need no other setting. A failed mint is not cached.
4. **The host is checked before anything is minted.** The coder wraps the App in `HostScoped`, which
   applies `ScopedToken`'s rule to any credentials: a repository on a host that is not in
   `ALLOWED_REPO_HOSTS`, and every local remote, is refused before a JWT is signed. Whoever chooses a
   repository URL cannot make the coder sign anything or send an installation token anywhere.
5. **Mint failures are typed, and the run is told what to check.** `401`, `403` and `404` are
   `Auth`, and name the variables; a run that ends at one fails naming `GITHUB_APP_ID`,
   `GITHUB_APP_INSTALLATION_ID` and the key (the coder's completion policy, as for a bad token). `429`
   and a `403` with no rate limit left are `RateLimited`, with the `Retry-After` GitHub gave.
   `5xx`, a transport failure and an answer that cannot be read are `Transient`. Neither the JWT nor a
   token is in a message, a `Debug` output or a log line.
6. **A minted token is a secret from the moment it exists.** The coder's `Redactor` is shared (its
   values live behind one lock, so clones see what is added), and the credentials the coder gives
   the workspace register every token they hand out. The key's PEM and its base64 body are
   registered at startup. At most 16 minted tokens are remembered, oldest forgotten first, which is
   more than the tokens one process can have in flight (one lives for an hour, one is minted five
   minutes before the last runs out). The key is hidden from OpenCode and from the project's commands
   (`GITHUB_APP_PRIVATE_KEY`, as `GITHUB_TOKEN` is); a key *file* is only a path.
7. **The trusted toolset stays in adam-coder, in process (D7.1).** What must hold, the sandbox, the
   named-repository rule, consent, checks bound to the pushed commit, publishing and the pull request
   behind the gate, and the creation of a repository, is the coder's own `#[tool]`s. A process
   boundary would not make them more trusted, and it would move the completion policy and the run
   notes out of the agent, make adam-mcp pass the host's grants in `params._meta` and artifacts and
   progress through, and make MCP calls retry-safe. Serving the toolset over MCP later is possible
   (an rmcp server, the grants in `params._meta`) and is not decided here (the plan's OD1).
8. **GitHub is read through the official server, read-only (D7.8).** The coder's `mcp.json` names
   the official binary over stdio, started with `--read-only` and a toolset allow-list, and a `tools`
   allow-list of read tools, so the model can read repositories, issues, pull requests and files and
   cannot write: **writes stay the coder's own** (decision 7), because they are what the gate guards.
   Production runs the binary inside the coder image; development and the e2e point the coder at a
   mock over http. In App mode the server gets the same `GITHUB_APP_*` variables, the key as a *file*
   (the key in a variable is not passed to a child, and a server that has an App and no key does not
   start: the coder then stops at startup, exit 69; the chart mounts the key as a file). The coder's
   deployment sets `MCP_ALLOW_STDIO=true`, which its policy needs to start a local process (the image
   does not: see the status note of 2026-10-01, coder-only). The
   allow-list is checked at startup (a name the server does not list stops the coder), and the image
   build runs the same check over stdio with no credential.
9. **Creating a repository is off unless the deployment says who it may be created for (D7.7).**
   `CREATE_REPO_OWNERS`, empty means off; the repository is private and empty; the person is asked
   every time, by a question the coder's tool writes; the credential is the same installation (an
   App needs the `Administration` permission for an organisation, and creates in organisations
   only) or token (`repo` scope).

```mermaid
sequenceDiagram
  participant T as a tool (push, pull request)
  participant H as HostScoped
  participant A as GitHubApp
  participant G as GitHub API
  participant X as git or REST
  T->>H: token_for(repository)
  H->>H: the host is in ALLOWED_REPO_HOSTS? (else refused, nothing signed)
  H->>A: token_for(repository)
  alt a cached token has more than 5 minutes left
    A-->>H: the cached token
  else none, or about to expire (one caller at a time, the others wait)
    A->>A: sign a JWT (iat now-60s, exp now+540s, iss the App)
    A->>G: POST /app/installations/{id}/access_tokens, Bearer JWT
    G-->>A: 201 {token, expires_at}
    A->>A: keep it, and the redactor learns it
    A-->>H: the new token
  end
  H-->>T: the token
  T->>X: x-access-token:<token> for git, Bearer <token> for REST
```

```mermaid
stateDiagram-v2
  [*] --> Empty: the process starts, the key is parsed
  Empty --> Fresh: mint (201)
  Fresh --> Expiring: 5 minutes or less left
  Expiring --> Fresh: mint (201)
  Empty --> Empty: mint failed (Auth, RateLimited or Transient), nothing cached
  Expiring --> Expiring: mint failed, the next call tries again
  Fresh --> Fresh: token_for returns the cached token
```

## Consequences

* **A deployment chooses per installation.** The same image runs with a token or an App. The chart
  (`github.auth: token|app`) and the compose override (`dev/compose.github-app.yaml`) do both.
* **The App's key is a long-lived secret in the pod**, a mounted file or a variable, and every pod
  that runs workers has it. A control plane has none of it. The key is never in the chart, in a log
  or in a tool result (the redactor), and a rotated key needs a restart.
* **An installation token is valid for an hour, and a push can be made with it for as long.** The
  coder replaces it five minutes early, so a step that began just before the margin has the rest of
  the margin. A token that leaks is good for what is left of its hour; the redactor is the defence
  inside the process.
* **One more hop at the start of every hour**, and a dependency on GitHub's token endpoint, where a
  token had none. A failed mint is a typed error for the step: `Transient` and `RateLimited` are the
  retryable classes of the host's error model, `Auth` is not.
* **`aws-lc-rs` and `rustls-pki-types` are direct dependencies of `adam-workspace`'s `github`
  feature.** Both were already in the tree under `rustls`; the features chosen add no new package.
* **The installation token's reach is the App's.** A repository the App is not installed on fails with
  `Auth` at `git` or at the REST call, not at the mint, and the run's error names the variables.
* *Unverified:* GitHub Enterprise Server's token endpoint and its rate-limit headers (the code reads
  `GITHUB_API_URL` and the standard headers), and a live App against github.com. What is tested is a
  mock: `cargo test -p adam-workspace --test github_app` checks the JWT's signature, `iss` and
  lifetime and the cycle (refresh margin, one request for 16 callers, the typed errors) with a key
  made for the run, and the compose e2e checks that the mock saw a JWT-shaped bearer and that every
  call to the repositories' API carried the token it gave (WireMock does not check the signature).

## Alternatives considered

* **Let github-mcp-server hold the App and give the coder a token from it.** The server's App mode is
  `stdio`-only and exposes no token; the coder needs tokens for `git` and for its own REST calls
  anyway. Rejected.
* **A token broker outside the process.** A sidecar or a service that returns tokens. More moving
  parts for a one-endpoint exchange. Not needed now, and not excluded: `GitCredentials` is the port,
  and a broker is another implementation of it.
* **A JWT crate and a GitHub client crate (`jsonwebtoken`, `octocrab`).** More dependencies for 60
  lines, and the second is a host SDK in all but name. Rejected for `aws-lc-rs`, which `rustls`
  already brings, and `reqwest`, which the REST client already uses.
* **Mint on every call.** Simple, and at most a request per push, but a run pushes, comments and
  polls several times a minute against a rate limit that counts per installation. Rejected for the
  cache.
* **Hold the old token on a failed refresh.** The cached token is good for five more minutes at the
  refresh margin. Rejected for now: the failure is rare and a step that fails with `Transient` is
  retried, which is simpler than a second state. It can come later without a change of interface.
* **A separate MCP-server process for the coder's own tools (OD1).** Decision 7. Left to the owner
  with the cost of about three more changes and a replay risk.

## Status notes

*2026-10-01: decisions 1 to 6 are built, in `adam-workspace` (`GitHubApp`, `AppKey`, `HostScoped`) and
in the coder (`GitHubAuth`, the shared `Redactor`, `RedactingCredentials`; `bin/adam-coder/README.md`,
"GitHub credentials"), with the chart (`github.auth`) and the compose override. Decision 7 is the state
of the code.*

*2026-10-01 (slice 7, A6): decision 8 is built. `bin/adam-coder/agent/mcp.json` names `github-mcp-server`
(stdio, `--read-only`, four toolsets, twelve tools); the coder image carries v1.12.2 by tag and digest,
(the coder's deployment sets `MCP_ALLOW_STDIO=true`, see the next note) and tests the tool list at build and in the container smoke test; the dev stack
and the e2e read GitHub through `mock-github-mcp` over http (`dev/coder-agent/mcp.json`); a test runs the
coder's binary against the real server in both modes (`ADAM_TEST_GITHUB_MCP_SERVER`; CI takes the binary
out of the pinned image). The facts of the context are verified against that tag (see above); what is not:
a live GitHub (the server was only run against a mock of its REST API), GitHub Enterprise Server, and
v1.13.0. Because the shipped `mcp.json` names a local process, **a coder on the embedded files needs
`MCP_ALLOW_STDIO=true` and the binary on `PATH`**, or it stops at startup (exit 78, or 69 when the binary
is missing); the image has the binary (and, at first, the variable: see the next note).*

*2026-10-01 (slice 7, A8): decision 9 is built (`bin/adam-coder/README.md`, "A repository of its own, on
request"). `create_repository { owner, name, private?, description? }` is off unless `CREATE_REPO_OWNERS` (the
chart's `github.createRepoOwners`) lists the owner; it asks the person with a question it writes (the repository, its
visibility, the description quoted) before **every** creation, records the yes for exactly `owner/name` and the
visibility, and creates the repository **empty** (`auto_init: false`) when the model calls it again; a yes to a
private repository does not cover a public one. The credentials are asked for the new repository's address, so the
allowed-hosts check applies, and the `clone_url` the host answers with must pass the workspace's policy. It creates
with `POST /orgs/{owner}/repos` for an organisation and `POST /user/repos` for the person the credentials are
(`CodeHost::owner_kind` and `CodeHost::authenticated_login`, which is `None` for an installation token), and refuses
any other owner, a user included when the credentials are an installation, before asking the person. Checked
against docs.github.com on 2026-10-01 (read through a summarising fetch, not the raw pages): both endpoints answer
`201`, `403` or `422` (a name that exists is `422`); an OAuth or classic token needs `public_repo` or `repo` for a
public repository and `repo` for a private one; and the page "Permissions required for GitHub Apps" lists
`POST /orgs/{org}/repos` and `POST /user/repos` under the Administration repository permission (write) for user and
installation tokens. **Unverified:** that an installation token can really create in an organisation (the code
assumes the Administration permission is what is missing when GitHub says `403`, and says so to the model), that
`GET /user` answers an installation token with a `403` (the code reads a `403` that is not a rate limit as "no
login"; a live App was not tried), and that `POST /user/repos` is unusable for an installation (the code never
sends it: an installation has no user to create for).*

*2026-10-01 (review of slice 7, A8): `create_repository` is safe to repeat. It writes an intent
(`owner/name`, visibility; `RunNotes::creating`) into the run's notes before it asks the host. If the process
dies between the host's answer and the note of it, the replay meets "already exists" **with** that intent in the
notes: it looks the repository up (`CodeHost::find_repository`, `GET /repos/{owner}/{name}`; a host that cannot
say answers `None`), applies the same address policy as to a new repository, grants it and records it, instead of
refusing it as a name the run did not create. A name that exists without an intent is still left alone, and the
intent is removed when the creation is recorded or the host definitely refused it (it stays after a failure that
may have happened after the host made the repository, a timeout). The consent rule is the one of ADR 0008: only
an explicit yes or no is recorded, so a "wait" no longer ends the question for the task.*

*2026-10-01: the owner chose coder-only for local-process MCP servers. The coder image no longer sets `MCP_ALLOW_STDIO=true`
(decision 8, as built in A6, had it in the image's `ENV`). The image carries two binaries, `adam-coder` and `adam-agent`
(ADR 0005), and an image-wide variable made every agent run from it allow a folder's `command` servers; only the coder, whose
shipped `mcp.json` starts the pinned `github-mcp-server`, needs that. The coder's own deployment sets it instead: the chart on
the roles that run workers (`MCP_ALLOW_STDIO: "true"` in the StatefulSet, none in the control plane's Deployment, which connects
no MCP server; its render check and golden follow), `compose.yaml` on the `coder` service, and the container smoke test, which
also checks that the image itself sets nothing. An `adam-agent` run from the image refuses local-process servers (the default,
`false`) unless its own deployment opts in. Nothing else in decision 8 changes.*
