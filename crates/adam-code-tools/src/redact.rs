//! Keeping the process's own secrets out of everything that leaves it.
//!
//! Text produced by things this process does not control ends up in places
//! clients can read: a command's output in an error, a
//! check's output in the findings of a failed run, a model provider's error
//! body (which may echo the key it rejected). A [`Redactor`] replaces the
//! *values* of the secrets the process holds with `[redacted: secret removed, do not reuse]`, and is applied
//! at the boundaries: tool results and errors, the checks'
//! progress lines, and the agent's final failure message.
//!
//! It is exact-value replacement, not a detector: a secret that was never
//! registered (or was transformed, say hashed or split across lines) is not
//! found. Alongside each value it also removes its standard Base64 form
//! (padded and unpadded), the way HTTP basic authorization would carry it.
//! Values shorter than [`MIN_SECRET_LEN`] are not registered: replacing every
//! `a` in the output would destroy it and protect nothing.

use std::borrow::Cow;
use std::fmt;
use std::sync::{Arc, RwLock};

use adam_llm_agent::StepIo;
use aho_corasick::{AhoCorasick, AhoCorasickBuilder, AhoCorasickKind, MatchKind};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use serde_json::Value;

/// What replaces a secret.
pub const REDACTED: &str = adam_llm_agent::REDACTED_MARKER;

/// The most a run's failure text (`RunView::error`, the A2A `Failed` status message) may carry, in
/// bytes. Upstream bodies and a command's output can be long, and the text is shown to clients.
pub const MAX_FAILURE_TEXT: usize = 2048;

/// Appended to a failure text that was cut.
const TRUNCATED: &str = " [truncated]";

/// Shortest value that is worth (and safe) replacing.
pub const MIN_SECRET_LEN: usize = 4;

/// How many of the secrets that were registered after startup ([`Redactor::add`]: the installation
/// tokens a GitHub App mints, one an hour for each installation) are remembered; the oldest is
/// forgotten first. A token that old has long expired, and the list is read at every scrub.
///
/// The coder's GitHub App keeps the tokens of up to 64 installations (`MAX_CACHED_INSTALLATIONS`
/// of `adam-workspace`'s feature `github`), and one more of each while a refresh overlaps the old
/// one: twice that many are remembered here, so no token the credentials can still hold, or hand
/// out, is forgotten (a test of the coder pins the two together) ([ADR 0017](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0017-a-github-app-works-on-every-account-it-is-installed-on.md),
/// D5). Sixteen, as before, was one live token with room to spare.
pub const MAX_ADDED: usize = 128;

/// What a [`Redactor`] knows: the secrets of startup, and the latest of those added since.
#[derive(Default)]
struct Registered {
    /// The secrets the process started with.
    fixed: Vec<String>,
    /// The secrets added later, oldest first (at most [`MAX_ADDED`]).
    added: std::collections::VecDeque<String>,
    /// Every needle of both, longest first, so that a secret that contains another is replaced
    /// whole.
    needles: Vec<String>,
    /// Every needle at once: [`scrub`](Redactor::scrub) scans a text with this, once, rather than
    /// once per needle. `None` when the build failed (more needles than the machine can index):
    /// `scrub` then replaces them one at a time.
    automaton: Option<AhoCorasick>,
    /// The length of the longest needle (its base64 and other derived forms included).
    longest: usize,
}

impl Registered {
    /// `needles` again, from the secrets.
    fn rebuild(&mut self) {
        let mut needles: Vec<String> = Vec::new();
        for secret in self.fixed.iter().chain(&self.added) {
            needles.push(secret.clone());
            needles.push(STANDARD.encode(secret));
            needles.push(STANDARD_NO_PAD.encode(secret));
            // Git's basic authorization for a token: base64("x-access-token:<token>").
            let basic = format!("x-access-token:{secret}");
            needles.push(STANDARD.encode(&basic));
            needles.push(STANDARD_NO_PAD.encode(&basic));
        }
        needles.sort_by_key(|n| std::cmp::Reverse(n.len()));
        needles.dedup();
        self.longest = needles.first().map_or(0, String::len);
        // Leftmost, then longest: where two values overlap, the one that starts first is replaced
        // and, of those that start together, the longest, so a value that contains another is
        // replaced whole. A contiguous NFA, never the DFA the builder would pick for a few
        // needles: one secret of 150 KB (its Base64 forms are 200 KB) makes a DFA table hundreds
        // of megabytes and minutes of build, while the NFA stays linear in the needles.
        self.automaton = AhoCorasickBuilder::new()
            .match_kind(MatchKind::LeftmostLongest)
            .kind(Some(AhoCorasickKind::ContiguousNFA))
            .build(needles.iter().map(String::as_bytes))
            .ok();
        self.needles = needles;
    }
}

/// Replaces registered secret values in text.
///
/// Clones share what they know: a secret added through one ([`add`](Self::add)) is scrubbed by all of
/// them, which is how a token minted while the process runs reaches the tools' copies.
#[derive(Clone, Default)]
pub struct Redactor {
    registered: Arc<RwLock<Registered>>,
}

impl fmt::Debug for Redactor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Redactor({} values)", self.read().needles.len())
    }
}

impl Redactor {
    /// Redact each of `secrets` (and its Base64 forms).
    pub fn new<S: AsRef<str>>(secrets: impl IntoIterator<Item = S>) -> Self {
        let mut registered = Registered {
            fixed: secrets
                .into_iter()
                .map(|s| s.as_ref().to_owned())
                .filter(|s| s.len() >= MIN_SECRET_LEN)
                .collect(),
            ..Registered::default()
        };
        registered.rebuild();
        Self {
            registered: Arc::new(RwLock::new(registered)),
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Registered> {
        self.registered
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Also redact `secret` (and its Base64 forms) from now on, in this redactor and every clone of
    /// it: a value the process only learned while it ran, such as an installation token minted
    /// for a GitHub App. Adding a known value changes nothing; only the latest
    /// `2 * MAX_CACHED_INSTALLATIONS` (128) added values are kept (those of startup are never
    /// dropped).
    pub fn add(&self, secret: &str) {
        if secret.len() < MIN_SECRET_LEN {
            return;
        }
        {
            let known = self.read();
            if known.fixed.iter().chain(&known.added).any(|s| s == secret) {
                return;
            }
        }
        let mut registered = self
            .registered
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if registered
            .fixed
            .iter()
            .chain(&registered.added)
            .any(|s| s == secret)
        {
            return;
        }
        registered.added.push_back(secret.to_owned());
        while registered.added.len() > MAX_ADDED {
            registered.added.pop_front();
        }
        registered.rebuild();
    }

    /// Redact `secret` (and its Base64 forms) from now on, **for good**: unlike [`add`](Self::add) it
    /// is never forgotten, however many installation tokens are added after it. For a value the
    /// process learns once, after it started, and holds as long as it runs (a key an MCP server's
    /// header reads from the environment). Adding a known value changes nothing; a value too
    /// short to be safe to replace is ignored.
    pub fn add_fixed(&self, secret: &str) {
        if secret.len() < MIN_SECRET_LEN || self.read().fixed.iter().any(|s| s == secret) {
            return;
        }
        let mut registered = self
            .registered
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if registered.fixed.iter().any(|s| s == secret) {
            return;
        }
        // It may have been added as a rotating value first: it is fixed now.
        registered.added.retain(|s| s != secret);
        registered.fixed.push(secret.to_owned());
        registered.rebuild();
    }

    /// Register the values of the environment variables `names` (looked up with `lookup`): the
    /// variables a `mcp.json` refers to as `${VAR}`, which hold credentials of this process. A
    /// name with no value, or a value too short to be a secret, adds nothing. The values are
    /// permanent ([`add_fixed`](Self::add_fixed)). Returns how many values were registered. Names only are ever logged, never values.
    pub fn add_env_values<'a>(
        &self,
        names: impl IntoIterator<Item = &'a str>,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> usize {
        let mut registered = 0;
        for name in names {
            if let Some(value) = lookup(name).filter(|v| v.len() >= MIN_SECRET_LEN) {
                self.add_fixed(&value);
                registered += 1;
            }
        }
        registered
    }

    /// How a tool call's step reports its input and output ([`StepIo`]), with this redactor
    /// over every string of both: the arguments the model gave and what the tool answered are sent to
    /// whoever reads the steps, so the process's secrets are scrubbed first. The redactor shared
    /// with this one, so a token registered later (an installation token the GitHub App minted) is
    /// scrubbed from the steps too.
    pub fn step_io(&self) -> StepIo {
        let redactor = self.clone();
        StepIo::default().redact(move |text| redactor.scrub_values(text.to_owned()))
    }

    /// The length in bytes of the longest value this redactor replaces, derived forms included.
    pub fn longest(&self) -> usize {
        self.read().longest
    }

    /// How far from the edge of a cut text a secret may still be only half there: the longest value
    /// it holds, and at least [`SCRUB_OVERLAP`](adam_llm_agent::SCRUB_OVERLAP). What a keeper cuts
    /// back at the open edge of a text, and what a capture holds beyond what it keeps.
    pub fn margin(&self) -> usize {
        self.longest().max(adam_llm_agent::SCRUB_OVERLAP)
    }

    /// How many bytes at the end of `text` are the first part of a value this redactor replaces (0
    /// when none): what an output that is written as it arrives holds back until the next read
    /// shows whether the value goes on ([`adam_llm_agent::StreamScrubber::release`]). A value that
    /// ends `text` whole is held too when a longer one starts with it, so that the longer one is
    /// replaced whole.
    pub fn partial_secret_len(&self, text: &str) -> usize {
        let bytes = text.as_bytes();
        let known = self.read();
        let mut best = 0;
        for needle in &known.needles {
            let needle = needle.as_bytes();
            let longest = needle.len().saturating_sub(1).min(bytes.len());
            for len in (best + 1..=longest).rev() {
                if bytes.ends_with(&needle[..len]) {
                    best = len;
                    break;
                }
            }
        }
        best
    }

    /// Whether nothing is registered.
    pub fn is_empty(&self) -> bool {
        self.read().needles.is_empty()
    }

    /// `text` with every registered value replaced by [`REDACTED`].
    ///
    /// One scan, leftmost and then longest: where values overlap, the one that starts first is
    /// replaced, and of those that start together the longest, so a value that contains another is
    /// replaced whole. The mark is not scanned again: a value that happens to read like a part of
    /// `[redacted: secret removed, do not reuse]` is not rewritten inside it.
    pub fn scrub<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let known = self.read();
        let Some(automaton) = &known.automaton else {
            // The automaton could not be built: redact one needle at a time, as before.
            return sequential_scrub(&known.needles, text);
        };
        let mut out: Option<String> = None;
        let mut copied = 0;
        for found in automaton.find_iter(text.as_bytes()) {
            let clean = out.get_or_insert_with(|| String::with_capacity(text.len()));
            clean.push_str(&text[copied..found.start()]);
            clean.push_str(REDACTED);
            copied = found.end();
        }
        match out {
            Some(mut clean) => {
                clean.push_str(&text[copied..]);
                Cow::Owned(clean)
            }
            None => Cow::Borrowed(text),
        }
    }

    /// [`scrub`](Self::scrub) for an owned string: every occurrence, for a message. A failure text,
    /// a tool error and a question are free text that leaves the process, never code a model reads
    /// back, so a secret inside a longer token is replaced too.
    pub fn scrub_string(&self, text: String) -> String {
        self.scrub_once(&text, false).unwrap_or(text)
    }

    /// [`scrub`](Self::scrub) for an owned string, at the **whole-value** sites: an ordinary word is
    /// replaced only where it is a value ([`adam_llm_agent::is_value_position`]), so a held secret
    /// that is also an ordinary word is kept where it is an identifier (`fn postgres_handler`,
    /// `my_postgres_pool`). For the text a model reads back as code: a tool's result, a kept output,
    /// a file an artifact carries, a step's input and output, a JSON string value.
    ///
    /// Only an ordinary word takes the rule: a value with a digit, a capital, `_`, `-` or a Base64
    /// form is never an identifier in code, and is replaced wherever it stands — the bench's
    /// `cG9zdGdyZXM=` glued to other text included.
    pub fn scrub_values(&self, text: String) -> String {
        self.scrub_once(&text, true).unwrap_or(text)
    }

    /// `text` scrubbed once, leftmost and then longest, with `values_only` choosing the whole-value
    /// rule for ordinary words; `None` when nothing was replaced. A mark already in the text is not
    /// scanned again: a scrub that runs twice (the coder settles an output, then the redacting
    /// layer scrubs the result) must not rewrite the mark it wrote, and a registered value that is
    /// a word of the mark (`secret`) would otherwise nest inside it.
    fn scrub_once(&self, text: &str, values_only: bool) -> Option<String> {
        let known = self.read();
        let Some(automaton) = &known.automaton else {
            return match sequential_scrub_values(&known.needles, text, values_only) {
                Cow::Borrowed(_) => None,
                Cow::Owned(clean) => Some(clean),
            };
        };
        let mut out: Option<String> = None;
        let mut copied = 0;
        for found in automaton.find_iter(text.as_bytes()) {
            let len = found.end() - found.start();
            let word = known
                .needles
                .get(found.pattern().as_usize())
                .is_some_and(|needle| adam_llm_agent::is_ordinary_word(needle));
            if inside_the_mark(text, found.start())
                || (values_only
                    && word
                    && !adam_llm_agent::is_value_position(text, found.start(), len))
            {
                continue;
            }
            let clean = out.get_or_insert_with(|| String::with_capacity(text.len()));
            clean.push_str(&text[copied..found.start()]);
            clean.push_str(REDACTED);
            copied = found.end();
        }
        out.map(|mut clean| {
            clean.push_str(&text[copied..]);
            clean
        })
    }

    /// `text` scrubbed, then cut to [`MAX_FAILURE_TEXT`] bytes (at a character boundary, with a
    /// marker): what a run's failure text may carry out of the process. It is in this order on
    /// purpose: cutting first could leave the front half of a secret that scrubbing would then no
    /// longer recognise. The scrub is [`scrub_string`](Self::scrub_string), the message one: a
    /// failure text goes into state and logs, never back to a model as code.
    pub fn failure_text(&self, text: String) -> String {
        let mut clean = self.scrub_string(text);
        if clean.len() > MAX_FAILURE_TEXT {
            let mut end = MAX_FAILURE_TEXT;
            while !clean.is_char_boundary(end) {
                end -= 1;
            }
            clean.truncate(end);
            clean.push_str(TRUNCATED);
        }
        clean
    }

    /// Scrub every string inside `value`, at the whole-value sites
    /// ([`scrub_values`](Self::scrub_values)); an object key is an identifier position and stays.
    pub fn scrub_value(&self, value: &mut Value) {
        match value {
            Value::String(s) => *s = self.scrub_values(std::mem::take(s)),
            Value::Array(items) => items.iter_mut().for_each(|v| self.scrub_value(v)),
            Value::Object(map) => {
                let entries = std::mem::take(map);
                for (key, mut v) in entries {
                    self.scrub_value(&mut v);
                    // A key is not scrubbed: it names a field (S-4G7Y's ruling). A held value that
                    // is also a word (`postgres`) would otherwise rename the field.
                    map.insert(key, v);
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }
}

/// The sequential replacement [`Redactor::scrub_once`] makes when the automaton could not be
/// built: the same two rules, one needle at a time, so a build that fails cannot change the
/// semantics.
fn sequential_scrub_values<'a>(
    needles: &[String],
    text: &'a str,
    values_only: bool,
) -> Cow<'a, str> {
    let mut out = Cow::Borrowed(text);
    for needle in needles {
        if !out.contains(needle.as_str()) {
            continue;
        }
        let rule = values_only && adam_llm_agent::is_ordinary_word(needle);
        let mut next = String::with_capacity(out.len());
        let mut at = 0;
        while let Some(found) = out[at..].find(needle.as_str()) {
            let start = at + found;
            let end = start + needle.len();
            next.push_str(&out[at..start]);
            if rule && !adam_llm_agent::is_value_position(&out, start, needle.len())
                || inside_the_mark(&out, start)
            {
                next.push_str(&out[start..end]);
            } else {
                next.push_str(REDACTED);
            }
            at = end;
        }
        next.push_str(&out[at..]);
        out = Cow::Owned(next);
    }
    out
}

/// Whether `at` falls inside a [`REDACTED`] mark already in `text`: a mark that spans `at` starts at
/// one of the `REDACTED.len() - 1` bytes before it, and the whole of it must be there — a text that
/// merely ends like the mark's front (`x[redact`) is not one.
fn inside_the_mark(text: &str, at: usize) -> bool {
    let head = &text[..at];
    (1..REDACTED.len()).any(|k| {
        head.len() >= k && head.ends_with(&REDACTED[..k]) && text[at - k..].starts_with(REDACTED)
    })
}

/// The sequential replacement [`Redactor::scrub`] made before it had an automaton: kept for a
/// build that fails, and the oracle of the test that compares the two.
fn sequential_scrub<'a>(needles: &[String], text: &'a str) -> Cow<'a, str> {
    let mut out = Cow::Borrowed(text);
    for needle in needles {
        if out.contains(needle.as_str()) {
            out = Cow::Owned(out.replace(needle.as_str(), REDACTED));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// A secret split at every byte position between two reads: what could still be its start is
    /// held back, whole, until the next read shows whether it goes on, and what is written is the
    /// scrub of the whole.
    #[test]
    fn a_secret_split_at_every_position_is_never_written_in_part() {
        let secret = "Zq9#Lm2$Xv7!Pw4@Hn8%";
        let longer = format!("{secret}-and-more");
        let redactor = Redactor::new([secret, longer.as_str()]);
        let scrub = |text: &str| redactor.scrub(text).into_owned();
        let partial = |text: &str| redactor.partial_secret_len(text);
        for k in 1..secret.len() {
            assert_eq!(
                partial(&format!("before {}", &secret[..k])),
                k,
                "split at {k}"
            );
        }
        // The shorter secret, whole, is the start of the longer one: it is held too.
        assert_eq!(partial(&format!("before {secret}")), secret.len());
        for text in [secret.to_owned(), longer.clone()] {
            for k in 1..text.len() {
                let (first, rest) = text.split_at(k);
                let mut scrubber = adam_llm_agent::StreamScrubber::new(redactor.margin());
                let mut written = scrubber.push(format!("a {first}").as_bytes(), &scrub);
                written.push_str(&scrubber.release(&scrub, &partial));
                assert!(
                    !written.contains(&first[..first.len().min(3)]),
                    "split at {k}: {written:?} holds the start of the secret"
                );
                written.push_str(&scrubber.push(format!("{rest} b").as_bytes(), &scrub));
                written.push_str(&scrubber.release(&scrub, &partial));
                written.push_str(&scrubber.finish(&scrub));
                assert_eq!(
                    written, "a [redacted: secret removed, do not reuse] b",
                    "split at {k} of {text}"
                );
            }
        }
    }

    #[test]
    fn replaces_every_occurrence_and_the_base64_forms() {
        let r = Redactor::new(["sk-secret-value"]);
        assert_eq!(
            r.scrub("key sk-secret-value and again sk-secret-value."),
            "key [redacted: secret removed, do not reuse] and again [redacted: secret removed, do not reuse]."
        );
        let b64 = STANDARD.encode("sk-secret-value");
        let bare = STANDARD_NO_PAD.encode("sk-secret-value");
        assert_eq!(
            r.scrub(&format!("Basic {b64}")),
            "Basic [redacted: secret removed, do not reuse]"
        );
        assert_eq!(
            r.scrub(&format!("Basic {bare}")),
            "Basic [redacted: secret removed, do not reuse]"
        );
        let basic = STANDARD.encode("x-access-token:sk-secret-value");
        assert_eq!(
            r.scrub(&format!("Authorization: Basic {basic}")),
            "Authorization: Basic [redacted: secret removed, do not reuse]"
        );
        assert!(matches!(r.scrub("nothing here"), Cow::Borrowed(_)));
    }

    #[test]
    fn the_values_of_named_variables_are_registered_when_there_are_any() {
        let r = Redactor::new(Vec::<String>::new());
        let vars = |name: &str| match name {
            "SEARCH_MCP_TOKEN" => Some("search-tok-9f3a1c".to_owned()),
            "CONTEXT7_API_KEY" => Some("ctx7sk-0123456789".to_owned()),
            "SHORT" => Some("abc".to_owned()),
            "EMPTY" => Some(String::new()),
            _ => None,
        };
        let registered = r.add_env_values(
            [
                "SEARCH_MCP_TOKEN",
                "CONTEXT7_API_KEY",
                "SHORT",
                "EMPTY",
                "UNSET",
            ],
            vars,
        );
        assert_eq!(registered, 2, "unset, empty and too short add nothing");
        assert_eq!(
            r.scrub("Bearer search-tok-9f3a1c / ctx7sk-0123456789 / abc"),
            "Bearer [redacted: secret removed, do not reuse] / [redacted: secret removed, do not reuse] / abc"
        );
    }

    #[test]
    fn registered_environment_values_survive_any_number_of_added_tokens() {
        let r = Redactor::new(["startup-secret-value"]);
        let registered = r.add_env_values(["MCP_KEY"], |_| Some("mcp-key-0123456789".to_owned()));
        assert_eq!(registered, 1);
        // The installation tokens a GitHub App mints are added one an hour for each installation:
        // far more than the bounded list keeps.
        for n in 0..(MAX_ADDED + 1) {
            r.add(&format!("ghs_installation_token_{n:04}"));
        }
        assert_eq!(
            r.scrub("key mcp-key-0123456789 / ghs_installation_token_0000"),
            "key [redacted: secret removed, do not reuse] / ghs_installation_token_0000",
            "the oldest token is forgotten, the key never is"
        );
        assert_eq!(
            r.scrub("startup-secret-value"),
            "[redacted: secret removed, do not reuse]"
        );
        assert_eq!(
            r.scrub(&format!("ghs_installation_token_{:04}", MAX_ADDED)),
            "[redacted: secret removed, do not reuse]"
        );
        // A value first learned as a rotating one becomes permanent, and a repeat adds nothing.
        r.add("late-rotating-value");
        r.add_fixed("late-rotating-value");
        r.add_fixed("late-rotating-value");
        for n in 0..(MAX_ADDED + 1) {
            r.add(&format!("ghs_other_token_{n:04}"));
        }
        assert_eq!(
            r.scrub("late-rotating-value"),
            "[redacted: secret removed, do not reuse]"
        );
    }

    #[test]
    fn short_and_empty_values_are_ignored_and_longer_ones_win() {
        let r = Redactor::new(["", "a", "abc", "abcd-longer", "abcd"]);
        assert_eq!(r.scrub("abc a"), "abc a", "too short to be safe to replace");
        assert_eq!(
            r.scrub("x abcd-longer y"),
            "x [redacted: secret removed, do not reuse] y"
        );
        assert!(Redactor::new(Vec::<String>::new()).is_empty());
    }

    /// The bench's case: the database password `postgres` is a held secret **and** an ordinary
    /// word. As a value it goes; as an identifier it stays.
    #[test]
    fn a_dictionary_word_is_redacted_as_a_value_and_kept_as_an_identifier() {
        let r = Redactor::new(["postgres"]);
        let marker = REDACTED;
        assert_eq!(
            r.scrub_values("postgres://postgres:postgres@db:5432/app".into()),
            format!("postgres://{marker}:{marker}@db:5432/app")
        );
        assert_eq!(
            r.scrub_values("password=postgres".into()),
            format!("password={marker}")
        );
        assert_eq!(
            r.scrub_values("fn postgres_handler() {}".into()),
            "fn postgres_handler() {}"
        );
        assert_eq!(
            r.scrub_values("my_postgres_pool".into()),
            "my_postgres_pool"
        );
        // A message takes the conservative scrub: a secret inside a longer token goes too.
        assert_eq!(
            r.scrub_string("fn postgres_handler() {}".into()),
            format!("fn {marker}_handler() {{}}")
        );
        // The streaming closure stays conservative: it replaces inside an identifier too.
        assert_eq!(
            r.scrub("fn postgres_handler() {}"),
            format!("fn {marker}_handler() {{}}")
        );
        // A short random secret in a value is still hidden.
        let short = Redactor::new(["x7k2"]);
        assert_eq!(
            short.scrub_values("token=x7k2".into()),
            format!("token={marker}")
        );
        // The fallback a failed automaton build takes keeps the rule.
        assert_eq!(
            sequential_scrub_values(&["postgres".into()], "fn postgres_handler() {}", true),
            "fn postgres_handler() {}"
        );
        assert_eq!(
            sequential_scrub_values(
                &["postgres".into()],
                "postgres://postgres:postgres@db:5432/app",
                true
            ),
            format!("postgres://{marker}:{marker}@db:5432/app")
        );
        // The message rule takes the identifier too, and a mark already there is not rewritten.
        assert_eq!(
            sequential_scrub_values(&["postgres".into()], "fn postgres_handler() {}", false),
            format!("fn {marker}_handler() {{}}")
        );
        assert_eq!(
            sequential_scrub_values(&["secret".into()], &format!("a {marker} b"), true),
            format!("a {marker} b")
        );
    }

    /// A value that is not an ordinary word is never an identifier, so it is replaced wherever it
    /// stands: a Base64 form glued to other text (`x cG9zdGdyZXM=abc`) is not kept because the byte
    /// after it is a letter.
    #[test]
    fn a_base64_form_glued_to_text_is_still_replaced() {
        let r = Redactor::new(["postgres"]);
        let b64 = STANDARD.encode("postgres");
        assert_eq!(
            r.scrub_values(format!("x {b64}abc")),
            format!("x {REDACTED}abc")
        );
        // The bare word is an ordinary one: as an identifier it stays, as a value it goes.
        assert_eq!(
            r.scrub_values("fn postgres_handler() {}".into()),
            "fn postgres_handler() {}"
        );
    }

    /// A text that merely ends like the mark's front is not a mark: a value there is still a value.
    #[test]
    fn a_prefix_of_the_mark_is_not_a_mark() {
        let r = Redactor::new(["secret"]);
        assert_eq!(
            r.scrub_string("x[redactsecret".into()),
            "x[redact[redacted: secret removed, do not reuse]"
        );
        // A value is replaced, and the mark itself is not rewritten.
        assert_eq!(
            r.scrub_string("password=secret".into()),
            format!("password={REDACTED}")
        );
        let marked = "a [redacted: secret removed, do not reuse] b".to_owned();
        assert_eq!(r.scrub_string(marked.clone()), marked);
    }

    /// A scrub that runs twice (the coder settles an output, then the redacting layer scrubs the
    /// result) does not rewrite the mark it wrote, even when a registered value is a word of it.
    #[test]
    fn the_mark_is_idempotent() {
        let r = Redactor::new(["secret"]);
        for once in [
            r.scrub_values("password=secret".into()),
            r.scrub_string("password=secret".into()),
        ] {
            assert_eq!(once, format!("password={REDACTED}"));
            assert_eq!(r.scrub_values(once.clone()), once);
            assert_eq!(r.scrub_string(once.clone()), once);
        }
    }

    /// The two whole-value sites besides a plain text result: a JSON string value and step IO. The
    /// bench's DSN keeps its scheme and loses its user and password; the identifier stays.
    #[test]
    fn a_json_value_and_a_step_are_whole_values() {
        let r = Redactor::new(["postgres"]);
        let mut v = json!({
            "conn": "postgres://postgres:postgres@db:5432/app",
            "cmd": "fn postgres_handler() {}",
        });
        r.scrub_value(&mut v);
        assert_eq!(
            v["conn"],
            json!(format!("postgres://{REDACTED}:{REDACTED}@db:5432/app"))
        );
        assert_eq!(v["cmd"], json!("fn postgres_handler() {}"));
    }

    /// The mark is a sentence, so a held value that is one of its words (`secret`, `removed`) is
    /// still *inside* the mark. What the output says is the same whatever the value was, so nothing
    /// about the secret is told: this is not a leak, and the mark says what happened.
    #[test]
    fn a_value_that_reads_like_a_word_of_the_mark_is_not_told_apart() {
        let a = Redactor::new(["secret"]).scrub_values("password=secret".into());
        let b = Redactor::new(["hunter2-hunter2"]).scrub_values("password=hunter2-hunter2".into());
        assert_eq!(a, b);
        assert_eq!(a, format!("password={REDACTED}"));
        // A value that is not a word of the mark is gone from the text.
        let c = Redactor::new(["secret"]).scrub_values("secret=1 and password=secret".into());
        assert_eq!(c, format!("{REDACTED}=1 and password={REDACTED}"));
    }

    #[test]
    fn json_values_are_scrubbed_and_keys_are_identifiers() {
        let r = Redactor::new(["hunter2-hunter2"]);
        let mut v = json!({"a": ["x hunter2-hunter2", 1, null], "hunter2-hunter2": {"b": "ok"}});
        r.scrub_value(&mut v);
        // A key is an identifier position, like `fn hunter2_hunter2`: it stays. A string value is a
        // value position: it goes.
        assert_eq!(
            v,
            json!({"a": ["x [redacted: secret removed, do not reuse]", 1, null], "hunter2-hunter2": {"b": "ok"}})
        );
    }

    #[test]
    fn debug_shows_no_secret() {
        let r = Redactor::new(["hunter2-hunter2"]);
        assert!(!format!("{r:?}").contains("hunter2"));
    }

    #[test]
    fn a_secret_added_later_is_scrubbed_by_every_clone_and_only_the_latest_are_kept() {
        let r = Redactor::new(["startup-secret"]);
        let clone = r.clone();
        assert_eq!(
            clone.scrub("ghs_minted_1 startup-secret"),
            "ghs_minted_1 [redacted: secret removed, do not reuse]"
        );
        r.add("ghs_minted_1");
        assert_eq!(
            clone.scrub("token ghs_minted_1 and startup-secret"),
            "token [redacted: secret removed, do not reuse] and [redacted: secret removed, do not reuse]",
            "a clone sees what was added to another"
        );
        // Its Base64 forms too, and adding it again changes nothing.
        let basic = STANDARD.encode("x-access-token:ghs_minted_1");
        assert_eq!(
            clone.scrub(&format!("Basic {basic}")),
            "Basic [redacted: secret removed, do not reuse]"
        );
        let before = format!("{r:?}");
        r.add("ghs_minted_1");
        r.add("startup-secret");
        r.add("abc");
        assert_eq!(format!("{r:?}"), before);
        // The latest are kept; the oldest is forgotten, the startup secret never is.
        for n in 2..=MAX_ADDED + 1 {
            r.add(&format!("ghs_minted_{n:03}"));
        }
        assert_eq!(r.scrub("ghs_minted_1"), "ghs_minted_1", "forgotten");
        assert_eq!(
            r.scrub(&format!("ghs_minted_002 ghs_minted_{:03}", MAX_ADDED + 1)),
            "[redacted: secret removed, do not reuse] [redacted: secret removed, do not reuse]"
        );
        assert_eq!(
            r.scrub("startup-secret"),
            "[redacted: secret removed, do not reuse]"
        );
        // An empty redactor learns too.
        let empty = Redactor::default();
        assert!(empty.is_empty());
        empty.add("ghs_first_token");
        assert!(!empty.is_empty());
    }

    /// The one-pass scan replaces what the old per-needle loop replaced, on the values whose order
    /// the loop made no difference for: a prefix, a Base64 pair (the padded form contains the bare
    /// one), and repeats. `sequential_scrub` is the old loop, kept as the oracle.
    #[test]
    fn the_one_pass_scrub_agrees_with_the_sequential_one_where_the_order_is_the_position() {
        let r = Redactor::new(["abcd", "abcd-longer", "sk-secret-value"]);
        let padded = STANDARD.encode("sk-secret-value");
        let bare = STANDARD_NO_PAD.encode("sk-secret-value");
        let basic = STANDARD.encode("x-access-token:sk-secret-value");
        for text in [
            "abcd and abcd-longer".to_owned(),
            "abcd-longer, then abcd".to_owned(),
            "sk-secret-value / sk-secret-value".to_owned(),
            format!("Basic {padded} and {bare}"),
            format!("Authorization: Basic {basic}"),
            "nothing to replace".to_owned(),
        ] {
            assert_eq!(
                r.scrub(&text),
                sequential_scrub(&r.read().needles, &text),
                "{text}"
            );
        }
    }

    /// Where two values overlap at different starts, the scan takes the leftmost and then the
    /// longest; the old loop took whichever needle came first in its longest-first list. Both
    /// leave no value whole. The mark is never scanned again, so a value that reads like a part of
    /// it is not rewritten inside it (the old loop rewrote `[redacted: secret removed, do not reuse]` itself).
    #[test]
    fn overlapping_values_are_replaced_leftmost_then_longest_and_the_mark_is_not_rescanned() {
        let r = Redactor::new(["bcde", "abcd"]);
        assert_eq!(
            r.scrub("abcde"),
            "[redacted: secret removed, do not reuse]e"
        );
        let r = Redactor::new(["abcd", "abcdefgh"]);
        assert_eq!(
            r.scrub("x abcdefgh y"),
            "x [redacted: secret removed, do not reuse] y"
        );
        let r = Redactor::new(["reda", "secret-value"]);
        assert_eq!(r.scrub("secret-value"), REDACTED);
        assert!(!r.scrub("secret-value").contains("secret-value"));
    }

    /// The 650 needles an App deployment can hold (130 installation tokens, five forms each) over
    /// a megabyte: one scan finds them all, and a text with none is returned without a copy.
    #[test]
    fn six_hundred_and_fifty_needles_scan_a_megabyte_once() {
        let secrets: Vec<String> = (0..130)
            .map(|n| format!("ghs_installation_token_{n:04}_abcdefghijklmnop"))
            .collect();
        let r = Redactor::new(&secrets);
        assert_eq!(r.read().needles.len(), 5 * secrets.len());
        let mut text = "x".repeat(1024 * 1024);
        for (n, secret) in secrets.iter().step_by(13).enumerate() {
            let at = 10_000 + n * 100_000;
            text.replace_range(at..at + secret.len(), secret);
        }
        let clean = r.scrub(&text);
        for secret in &secrets {
            assert!(!clean.contains(secret), "a secret is left: {secret}");
        }
        assert_eq!(
            clean.matches(REDACTED).count(),
            secrets.iter().step_by(13).count()
        );
        assert!(matches!(r.scrub("nothing here"), Cow::Borrowed(_)));
    }

    /// A secret of 150 KB (its Base64 forms are 200 KB) builds the automaton the linear way: the
    /// builder would pick a DFA for a few needles, and that table is hundreds of megabytes and
    /// minutes of build. It is still replaced.
    #[test]
    fn a_secret_of_150_kib_builds_a_linear_automaton() {
        let secret = "S".repeat(150_000);
        let r = Redactor::new([secret.as_str()]);
        assert_eq!(
            r.read().automaton.as_ref().unwrap().kind(),
            AhoCorasickKind::ContiguousNFA
        );
        assert_eq!(
            r.scrub(&format!("a{secret}b")),
            "a[redacted: secret removed, do not reuse]b"
        );
    }
}
