//! `locale/v1`: the interface language a message names, a BCP 47 tag.
//!
//! A client that knows the person's interface language names it **for each message**, in the
//! message's metadata under [`LOCALE_EXTENSION`], as `{"locale": "<tag>"}`; the card's entry says
//! the agent's **default** language, the one it answers in when a message names none. The hint is
//! an instruction for the agent's **own words**, never a translation layer: nothing here
//! translates a message, a document or a string (adam ADR 0069).
//!
//! Two rules keep a hint harmless:
//!
//! * **Only an activated extension is read.** A message whose request did not name the extension
//!   carries no hint, whatever its metadata says ([`Locale::of_request`]). The hint belongs to its
//!   message: it is a default, never a thread-wide setting.
//! * **A malformed tag is refused, and the message goes on without a hint.** The tag is a
//!   well-formed BCP 47 tag (RFC 5646's `langtag` production) of at most [`MAX_LOCALE_BYTES`]
//!   bytes; anything else is [`NotALocale`], and a message that names no locale carries no hint at
//!   all, never an empty string.
//!
//! The high-risk rule lives where the words are written: the agent's own words never fill a string
//! a person must trust (a consent, a payment, a confirmation, a trusted component), because those
//! are the deployment's reviewed text, in the languages it was reviewed in (ADR 0069).

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use a2a::Message;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::Caller;
use crate::extensions::LOCALE_EXTENSION;

/// The longest tag kept, in bytes. A well-formed tag of an interface language is far shorter
/// (`fr-CM`, `zh-Hant-TW`, `de-CH-1901`), and the bound is what keeps a message from carrying a
/// paragraph where a language belongs. A longer tag is refused, never cut.
pub const MAX_LOCALE_BYTES: usize = 64;

/// The member of the entry under [`LOCALE_EXTENSION`] that carries the tag: `{"locale": "fr-CM"}`.
/// It is part of the wire: clients and agents read it by name.
pub const LOCALE_MEMBER: &str = "locale";

/// A well-formed BCP 47 language tag: a language subtag, then at most a script, a region, variants,
/// extensions and private use (RFC 5646's `langtag` production), at most [`MAX_LOCALE_BYTES`] bytes.
///
/// The tag is kept as it came: language tags are case insensitive, and rewriting one would be a
/// choice this crate has no business making. It is a language tag and nothing more: a string of
/// this type is safe to print and to put in a prompt, because every subtag is ASCII alphanumeric
/// and the whole is bounded.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Locale(String);

impl Locale {
    /// `tag`, when it is a well-formed BCP 47 tag of at most [`MAX_LOCALE_BYTES`] bytes.
    ///
    /// # Errors
    ///
    /// [`NotALocale`] for an empty tag, a tag longer than the bound, or a string that is not the
    /// `langtag` production: a language subtag of 2 to 8 ASCII letters, then an optional script
    /// (4 letters), an optional region (2 letters or 3 digits), variants, extensions (a singleton
    /// and at least one subtag) and private use (`x` and at least one subtag), each subtag of 1 to
    /// 8 ASCII alphanumerics. The grandfathered and irregular tags (`i-klingon`, `en-GB-oed`) and a
    /// tag of private use alone (`x-private`) are not accepted: an interface language is an ordinary
    /// tag that names a language.
    pub fn parse(tag: &str) -> Result<Self, NotALocale> {
        if well_formed(tag) {
            Ok(Self(tag.to_owned()))
        } else {
            Err(NotALocale(tag.to_owned()))
        }
    }

    /// The tag, as it came.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The tag a message's `metadata` names under [`LOCALE_EXTENSION`], when it is well formed.
    ///
    /// `None` for no entry under the URI, an entry that is not an object, a [`LOCALE_MEMBER`] that
    /// is missing, not a string, empty or not a well-formed tag: a malformed tag is refused, never
    /// passed on, and a message that names none leaves no hint at all (never an empty string).
    ///
    /// Activation is the caller's to check, as for every extension: see [`Self::of_request`].
    pub fn of(metadata: &HashMap<String, Value>) -> Option<Self> {
        let tag = metadata
            .get(LOCALE_EXTENSION)?
            .get(LOCALE_MEMBER)?
            .as_str()?;
        Self::parse(tag).ok()
    }

    /// The hint a request carries: the tag of `message`, when the request **activated**
    /// [`LOCALE_EXTENSION`] (the card lists it and the request named it, [`Caller::has_extension`])
    /// and the entry is well formed.
    ///
    /// `None` is the whole hint of a message that names no locale: nothing is added to the run, so
    /// an agent answers in the person's language, as it always did.
    pub fn of_request(caller: &Caller, message: &Message) -> Option<Self> {
        if !caller.has_extension(LOCALE_EXTENSION) {
            return None;
        }
        Self::of(message.metadata.as_ref()?)
    }
}

impl fmt::Display for Locale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Locale {
    type Err = NotALocale;

    fn from_str(tag: &str) -> Result<Self, NotALocale> {
        Self::parse(tag)
    }
}

impl Serialize for Locale {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Locale {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let tag = String::deserialize(deserializer)?;
        Self::parse(&tag).map_err(serde::de::Error::custom)
    }
}

/// A string that is not a well-formed language tag.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "{0:?} is not a language tag: it is a BCP 47 tag (a language, then at most a script, a region, \
     variants, extensions and private use) of at most {MAX_LOCALE_BYTES} bytes"
)]
pub struct NotALocale(pub String);

/// Whether `tag` is a well-formed `langtag` of RFC 5646, at most [`MAX_LOCALE_BYTES`] bytes.
///
/// *verified* 2026-11-22 against RFC 5646 §2.1, Figure 1
/// (<https://www.rfc-editor.org/rfc/rfc5646.txt>), whose ABNF this follows:
/// `language ["-" script] ["-" region] *("-" variant) *("-" extension) ["-" privateuse]`.
fn well_formed(tag: &str) -> bool {
    if tag.len() > MAX_LOCALE_BYTES {
        return false;
    }
    let parts: Vec<&str> = tag.split('-').collect();
    // Every subtag is 1 to 8 ASCII alphanumerics: no empty one, no space, no other character.
    if parts.iter().any(|part| {
        part.is_empty() || part.len() > 8 || !part.bytes().all(|b| b.is_ascii_alphanumeric())
    }) {
        return false;
    }

    // language = 2*3ALPHA ["-" extlang] / 4ALPHA / 5*8ALPHA
    let language = parts[0];
    let mut i = match language.len() {
        2..=3 if is_alpha(language) => {
            // Up to three extension languages (`zh-cmn-Hans-CN`); a 2-letter or 3-digit subtag
            // that follows is a region, and a 4-letter one a script, never an extlang.
            let mut i = 1;
            while i < parts.len() && i <= 3 && is_alpha(parts[i]) && parts[i].len() == 3 {
                i += 1;
            }
            i
        }
        4..=8 if is_alpha(language) => 1,
        _ => return false,
    };

    // script = 4ALPHA
    if i < parts.len() && parts[i].len() == 4 && is_alpha(parts[i]) {
        i += 1;
    }
    // region = 2ALPHA / 3DIGIT
    if i < parts.len()
        && ((parts[i].len() == 2 && is_alpha(parts[i]))
            || (parts[i].len() == 3 && parts[i].bytes().all(|b| b.is_ascii_digit())))
    {
        i += 1;
    }
    // variant = 5*8alphanum / (DIGIT 3alphanum)
    while i < parts.len() && is_variant(parts[i]) {
        i += 1;
    }
    // extension = singleton 1*("-" (2*8alphanum)); singleton is any alphanumeric but `x`
    while i < parts.len()
        && parts[i].len() == 1
        && parts[i].bytes().all(|b| b.is_ascii_alphanumeric())
        && !parts[i].eq_ignore_ascii_case("x")
    {
        i += 1;
        let first = i;
        while i < parts.len() && (2..=8).contains(&parts[i].len()) {
            i += 1;
        }
        if i == first {
            return false;
        }
    }
    // privateuse = "x" 1*("-" (1*8alphanum))
    if i < parts.len() && parts[i].eq_ignore_ascii_case("x") {
        let first = i + 1;
        i = parts.len();
        if i == first {
            return false;
        }
    }
    i == parts.len()
}

fn is_alpha(part: &str) -> bool {
    part.bytes().all(|b| b.is_ascii_alphabetic())
}

fn is_variant(part: &str) -> bool {
    let bytes = part.as_bytes();
    ((5..=8).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_alphanumeric()))
        || (part.len() == 4
            && bytes[0].is_ascii_digit()
            && bytes[1..].iter().all(u8::is_ascii_alphanumeric))
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::{Part, Role};
    use serde_json::json;

    fn metadata(entry: Value) -> HashMap<String, Value> {
        HashMap::from([(LOCALE_EXTENSION.to_owned(), entry)])
    }

    fn saying(entry: Option<Value>) -> Message {
        let mut message = Message::new(Role::User, vec![Part::text("bonjour")]);
        message.metadata = entry.map(metadata);
        message
    }

    fn activated() -> Caller {
        Caller::new("token-0").with_extensions([LOCALE_EXTENSION])
    }

    #[test]
    fn the_languages_of_a_deployment_are_well_formed_tags() {
        // The shapes a deployment writes: a language and a region, a script, variants, private use.
        for tag in [
            "en-CM",
            "fr-CM",
            "wes-CM",
            "ybb-CM",
            "en",
            "de-CH-1901",
            "zh-Hant-TW",
            "zh-cmn-Hans-CN",
            "sl-rozaj-biske-1994",
            "es-419",
            "en-US-x-twain",
            "en-a-bbb",
            "FR-cm",
        ] {
            let locale = Locale::parse(tag).unwrap_or_else(|e| panic!("{tag}: {e}"));
            assert_eq!(locale.as_str(), tag, "kept as it came");
            assert_eq!(locale.to_string(), tag);
            assert_eq!(tag.parse::<Locale>(), Ok(locale.clone()));
            assert_eq!(serde_json::to_value(&locale).unwrap(), json!(tag));
            assert_eq!(
                serde_json::from_value::<Locale>(json!(tag)).unwrap(),
                locale,
                "{tag}"
            );
        }
    }

    #[test]
    fn anything_else_is_refused_and_never_passed_on() {
        for bad in [
            "",
            " ",
            " fr-CM",
            "fr-CM ",
            "fr_CM",
            "fr CM",
            "-fr",
            "fr-",
            "fr--CM",
            "fr-CM\n",
            "123",
            "f",
            "i-klingon",
            "en-GB-oed",
            "en-123456789",
            "en-US-abcdefghi",
            "x",
            "en-x",
            "en-a",
            "fr-CM;q=1",
            "fr-CM,en",
            "fr-\u{00e9}",
        ] {
            assert_eq!(
                Locale::parse(bad),
                Err(NotALocale(bad.to_owned())),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_tag_is_bounded_and_a_longer_one_is_refused_not_cut() {
        // A long but well-formed tag: a language of 8 letters, private use, six 8-letter subtags.
        let long = format!("abcdefgh-x-{}", ["aaaaaaaa"; 6].join("-"));
        assert_eq!(long.len(), MAX_LOCALE_BYTES);
        assert!(Locale::parse(&long).is_ok(), "{long}");
        // Past the bound: refused, never cut.
        let longer = format!("{long}-a");
        assert_eq!(
            Locale::parse(&longer),
            Err(NotALocale(longer.clone())),
            "{longer}"
        );
        assert!(Locale::parse(&"a".repeat(MAX_LOCALE_BYTES + 1)).is_err());
    }

    #[test]
    fn the_entry_names_the_tag_and_nothing_else_does() {
        assert_eq!(
            Locale::of(&metadata(json!({"locale": "fr-CM"}))),
            Some(Locale::parse("fr-CM").unwrap())
        );
        // Unknown members of the entry are ignored.
        assert_eq!(
            Locale::of(&metadata(
                json!({"locale": "ybb-CM", "why": "the person's"})
            )),
            Some(Locale::parse("ybb-CM").unwrap())
        );
        // Never an empty string, never a malformed tag, never another shape.
        for entry in [
            json!({"locale": ""}),
            json!({"locale": "  "}),
            json!({"locale": "fr-CM "}),
            json!({"locale": 3}),
            json!({"locale": null}),
            json!({}),
            json!({"tag": "fr-CM"}),
            json!("fr-CM"),
            json!(["fr-CM"]),
            json!(null),
        ] {
            assert_eq!(Locale::of(&metadata(entry.clone())), None, "{entry}");
        }
        assert_eq!(Locale::of(&HashMap::new()), None, "no entry at all");
    }

    #[test]
    fn only_a_request_that_activated_the_extension_reads_a_tag() {
        let named = saying(Some(json!({"locale": "wes-CM"})));
        assert_eq!(
            Locale::of_request(&activated(), &named).map(|l| l.as_str().to_owned()),
            Some("wes-CM".to_owned())
        );
        // Not activated: the entry is not read, whatever it says.
        assert_eq!(Locale::of_request(&Caller::new("token-0"), &named), None);
        // Activated, but the message names no locale: no hint at all.
        assert_eq!(Locale::of_request(&activated(), &saying(None)), None);
        assert_eq!(
            Locale::of_request(&activated(), &saying(Some(json!({"locale": ""})))),
            None
        );
    }
}
