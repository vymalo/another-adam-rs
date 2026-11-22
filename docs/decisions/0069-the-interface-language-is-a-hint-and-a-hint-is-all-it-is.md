# 0069. The interface language is a hint, and a hint is all it is

Status: **Accepted** (2026-11-22). The owner decided on 2026-11-22, in the plan `2026-11-22-languages.md` of
`another-context` (decisions 14 to 17), that an agent answers in the interface language by default and in the person's
own language when they write in another, and that the languages a deployment offers are its configuration. This ADR
is the adam-rs side; the client half is the orchestration layer's.

## Context

The client knows the person's **interface** language; the agent does not. Today the agent answers in whatever
language the model picks from the conversation, which is right when a person writes in their own language and wrong
when the interface is in a language the person reads but does not write: a person on a French interface who types
`ok` or pastes a stack trace gets an answer in English, and a person on an English interface who writes a full
sentence in their own language gets it in English too.

Two facts about models decide the design:

- **A model writes a language it has barely seen badly.** A hint improves its own words; it does not make it an
  author of a language it has no material for.
- **A string a person must trust cannot be a model's words, in any language.** A consent, a payment, a
  confirmation of something irreversible: the text that asks a person to agree to something is reviewed, once per
  language, by a person — and no model output is.

adam-rs already carries per-message host conveniences as optional extensions the card lists and a request activates,
the message's metadata carrying the value (`access-tier/v1`, `steer/v1`, `mentions/v1`, `commands/v1`; ADR 0006).
A language belongs in exactly that shape.

## Decision

### 1. `locale/v1`

The URI is `https://agents.vymalo.com/a2a/extensions/locale/v1`. The card's entry is optional (`required: false`)
and its `params` are `{"locale": "<tag>"}`, one tag: **the agent's default language**, the one it answers in when a
message names none. A request that **activates** the extension may carry `metadata[<URI>] = {"locale": "<BCP 47 tag>"}`
on the message — **the client's hint**, one tag under the same key, the shape of `access-tier/v1`'s `{"tier": ..}`.
Nothing else: no array of languages, no second key, no per-language member.

- **Read only when activated.** A message whose request did not name the extension carries no hint, whatever its
  metadata says, exactly as a tier is not read from a message that did not activate `access-tier/v1`.
- **Validated and bounded.** The tag must be a well-formed BCP 47 tag (RFC 5646's `langtag` production) of at most
  64 bytes. A malformed one is refused, never passed on, and the message goes on **with no hint**: fail closed means
  no language rather than a guessed one.
- **Nothing when nothing is named.** A message that names no locale leaves no hint at all, never an empty string: an
  empty tag names no language, and a model told that the language is `""` would have to guess one.
- **The hint belongs to its message.** The client sends it with each message and the orchestration layer keeps it with
  the message, so a redelivery, a fork and a resumed run carry the same hint: it is this message's default, never a
  thread-wide setting.

### 2. The hint is a prompt instruction, not a translation layer

What the agent does with the hint is one rule in its instructions: answer in the interface language the message
names, and follow the person when they write in another language. Nothing is translated — not
a message, not a document, not a string — and there is no locale-aware formatting on the server, no template
catalogue and no language negotiation. Code, paths, command output and file names are never translated, in any
language.

The reason is that the hint is **one fact about the person, for the model's own words**, and everything else an
agent writes is either the person's (their words, their code, their paths) or the deployment's (its reviewed text).

### 3. The agent's own words never fill a string a person must trust

Consent, payments, confirmations of irreversible actions and trusted components are **reviewed static strings of
the deployment**, in the languages they were reviewed in. The hint never reaches them: the agent does not write,
translate, reword or fill one, in any language, whatever language the person writes in.

In the coder's prompt the rule is one line, *a consent, a payment, a confirmation is the product's reviewed text, in
its own language: never yours*, beside *answer in the language the interface names, and follow the person when they
write in another*. The two lines cost 284 characters of a prompt that must stay under 30,000 (ADR 0038's test, which
GitHub Copilot's agent-file limit sets): the next rule that goes in has to trim something first.

This is the dangerous rule of the feature, and it is a rule about **capability**, not about intent: a model that
writes a language barely must not be able to produce the text of a consent screen, because the failure is silent —
the text looks like a language and reads like a permission. A reviewed string is reviewed once per language by a
person; a model's translation of one is reviewed by nobody.

### 4. The language is the deployment's configuration, not code

Nothing in this repository names a language: the card's `locale` is read from the deployment (an environment
variable, a chart value, a folder), a tag it writes must be well formed or the deployment does not start (a bad
value is exit 78, as for every variable), and a deployment that offers no language lists no extension. Which
languages a deployment's own text exists in is its decision, and this repository's prose stays neutral about who they
are for.

### 5. The hint is a default, not a constraint

A message may name any well-formed tag, not only the card's default. It is still a hint, and the agent still follows
the person: refusing the message would break a turn for no safety gain, since a model that can write the language can
answer in it. The card's one tag says what the agent answers in **when nothing is named** — never a filter on the
person, and never a limit on the languages the deployment's **own reviewed text** exists in, which is exactly why the
hint never fills that text (decision 3).

## Consequences

- **Built in this slice:** `crates/adam-a2a` — the URI, the card entry (`ExtensionConfig::locale`), and the
  validated reading (`Locale`, `Locale::of`, `Locale::of_request`), with tests for a card that lists it and one that
  does not, a valid tag, a malformed one, a message that names none, and activation only on a card that offers it;
  the coder's prompt (`bin/adam-coder/agent/instructions.md`: the language rule and the high-risk rule);
  `docs/reference/a2a-server.md`; a parity row.
- **Not built here, and where:** the run's context key beside `access-tier/v1`'s (`crates/adam-a2a-runtime`), the
  instruction source that puts the hint in front of the model (`bin/adam-coder`, as `src/tier.rs` does for the
  tier), the cards of `adam-coder` and `adam-agent` and the deployment's list (`bin/adam-coder/src`, `bin/adam-agent`,
  `deploy/coder`), and the client half in `vymalo/another-agentic-system`. Until the source exists the prompt's
  first half has nothing to read: an agent then follows the person's language, which the same rule says, so nothing
  is wrong — only less is used.
- **No stored byte changes.** The hint is not written to a run's log by this crate; a card that lists the extension
  and a request that activates it are both optional, and an agent that never surfaces the hint behaves as before.
- **The contract page is this repository's until the orchestration layer publishes one.** The URI and the metadata
  shape copy `access-tier/v1` so that the client half has one shape to implement; `docs/api/locale-v1.md` of
  `vymalo/another-agentic-system` supersedes the description here when it exists, and a change to the shape is then
  that page's decision, not this ADR's.

## Slices

1. This ADR; `adam-a2a`'s `locale/v1` (URI, card entry, validated reading, tests); the coder's prompt rules; the docs
   and the parity row.
2. `adam-a2a-runtime`: the context key `vymalo.locale`, read from an activated message and cleared by a message that
   names none, beside `access-tier/v1`'s handling.
3. `adam-coder` and `adam-agent`: the card entry from the deployment's own tag, the instruction source that shows the
   model the hint, and that tag's variable in `deploy/coder`.
4. The client half: the orchestrator sends the person's interface locale with each message, activated per message
   (`vymalo/another-agentic-system`).
