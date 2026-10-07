//! The registry document of `agent-registry/v1` (`docs/extensions/agent-registry-v1.md`): a linkset in
//! the shape of an RFC 9727 API catalog, with one `item` link per agent service. Pure: entries in,
//! bytes out.
//!
//! What the builder guarantees, so that a reader following the contract's rules never has to skip:
//!
//! * an item is a service the directory says is [`listed`](DirectoryEntry::listed) (A2A enabled, not
//!   `Blocked`, with a card URL), **in the directory's order** (scope, then name), which is the
//!   display order and is stable between reads;
//! * its `href` is an absolute `http` or `https` URL with a host and no credentials, its `service` is
//!   an agent id (`^[a-z0-9][a-z0-9-]{0,62}$`), and **a service id is never listed twice**: a name used
//!   in two namespaces is listed once, the first in order, and the others are in [`Built::skipped`];
//! * `title` is the service's title, trimmed and cut at 200 characters, and absent when there is none (a
//!   reader defaults it to the service id); `tags` keep the valid tags (non-empty, at most 64 characters,
//!   at most 16, no repeats) and are absent when none is left;
//! * the document is **refused, never truncated**, past 500 items or 1 MiB ([`Overflow`]): a client
//!   never truncates either, and silently dropping agents from a long list would hide them.
//!
//! The `ETag` is a strong validator: the first 128 bits of the SHA-256 of the body.

use std::collections::BTreeSet;

use adam_operator_ports::DirectoryEntry;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;

/// The profile that names this contract: the document's own version marker.
pub const PROFILE: &str = "https://agents.vymalo.com/registry/v1";
/// The profile RFC 9727 asks an API catalog to send, in the media type's `profile` parameter.
pub const RFC9727_PROFILE: &str = "https://www.rfc-editor.org/info/rfc9727";
/// The media type of the document.
pub const MEDIA_TYPE: &str = "application/linkset+json";
/// The most items of a document (the contract's limit).
pub const MAX_ITEMS: usize = 500;
/// The most bytes of a document (the contract's limit): 1 MiB.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
/// The most tags of an item.
pub const MAX_TAGS: usize = 16;
/// The most characters of a tag.
pub const MAX_TAG_CHARS: usize = 64;
/// The most characters of a title that are kept.
pub const MAX_TITLE_CHARS: usize = 200;
/// The longest agent id.
pub const MAX_ID_CHARS: usize = 63;

/// A document the contract's limits refuse. The registry answers `503` and every service that would
/// have been listed gets `Listed: False`, reason `RegistryFull`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Overflow {
    /// How many items the document would have (all of them, not the first 500).
    pub items: usize,
    /// How many bytes it would have, or `None` when the item count alone refused it.
    pub bytes: Option<usize>,
}

/// Why an entry is not in the document: for the operator's log, never for a client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    /// `scope/name` of the entry.
    pub entry: String,
    /// Why.
    pub reason: &'static str,
}

/// A document, ready to serve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Built {
    /// The body: UTF-8 JSON, compact.
    pub body: Vec<u8>,
    /// The strong `ETag`, quotes included.
    pub etag: String,
    /// The service ids listed, in the document's order.
    pub listed: Vec<String>,
    /// The entries that are listable by the directory's rule and were left out, and why. An entry the
    /// directory does not list (A2A disabled, blocked) is not here: it is not an omission.
    pub skipped: Vec<Skipped>,
}

/// Whether `id` is an agent id: `^[a-z0-9][a-z0-9-]{0,62}$`, the contract's `service`.
pub fn is_valid_service_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id.len() <= MAX_ID_CHARS
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The card URL as the document states it, or why it cannot be listed.
fn card_url(card: &str) -> Result<String, &'static str> {
    match Url::parse(card) {
        Ok(url) if matches!(url.scheme(), "http" | "https") && url.has_host() => {
            if url.username().is_empty() && url.password().is_none() {
                Ok(url.to_string())
            } else {
                Err("the card URL carries credentials")
            }
        }
        Ok(_) | Err(_) => Err("the card URL is not an absolute http(s) URL"),
    }
}

fn title_of(title: Option<&str>) -> Option<String> {
    let title = title?.trim();
    (!title.is_empty()).then(|| title.chars().take(MAX_TITLE_CHARS).collect())
}

fn tags_of(tags: &[String]) -> Vec<&str> {
    let mut seen = BTreeSet::new();
    tags.iter()
        .map(String::as_str)
        .filter(|t| !t.is_empty() && t.chars().count() <= MAX_TAG_CHARS && seen.insert(*t))
        .take(MAX_TAGS)
        .collect()
}

/// Build the document of `entries`. `anchor` is the URL of the document itself, which the contract asks a
/// server to send and a reader ignores: `None` leaves it out.
///
/// # Errors
///
/// [`Overflow`] when the document would exceed [`MAX_ITEMS`] items or [`MAX_BODY_BYTES`] bytes.
pub fn build(entries: &[DirectoryEntry], anchor: Option<&str>) -> Result<Built, Overflow> {
    let mut ordered: Vec<&DirectoryEntry> = entries.iter().filter(|e| e.listed()).collect();
    ordered.sort_by(|a, b| (&a.scope, &a.name).cmp(&(&b.scope, &b.name)));

    let mut items: Vec<Value> = Vec::new();
    let mut listed: Vec<String> = Vec::new();
    let mut skipped = Vec::new();
    let mut seen = BTreeSet::new();
    for e in ordered {
        let who = format!("{}/{}", e.scope, e.name);
        let skip = |reason| Skipped {
            entry: who.clone(),
            reason,
        };
        if !is_valid_service_id(&e.name) {
            skipped.push(skip("the name is not a valid service id"));
            continue;
        }
        let href = match e.agent_card.as_deref().map(card_url) {
            Some(Ok(href)) => href,
            Some(Err(reason)) => {
                skipped.push(skip(reason));
                continue;
            }
            // `listed()` needs a card, so this does not happen; it is not an item if it does.
            None => continue,
        };
        if !seen.insert(e.name.as_str()) {
            skipped.push(skip(
                "another namespace has a service of this name, listed first",
            ));
            continue;
        }
        let mut item = json!({"href": href, "type": "application/json", "service": [e.name]});
        if let Some(title) = title_of(e.title.as_deref()) {
            item["title"] = json!(title);
        }
        let tags = tags_of(&e.tags);
        if !tags.is_empty() {
            item["tags"] = json!(tags);
        }
        items.push(item);
        listed.push(e.name.clone());
    }
    if items.len() > MAX_ITEMS {
        return Err(Overflow {
            items: items.len(),
            bytes: None,
        });
    }

    let mut context = json!({"profile": [{"href": PROFILE}]});
    if let Some(anchor) = anchor {
        context["anchor"] = json!(anchor);
    }
    if !items.is_empty() {
        context["item"] = Value::Array(items);
    }
    // `serde_json` writes object members in key order and without spaces: the same entries give the
    // same bytes, which is what an `ETag` over the body needs.
    let body = serde_json::to_vec(&json!({"linkset": [context]})).unwrap_or_default();
    if body.len() > MAX_BODY_BYTES {
        return Err(Overflow {
            items: listed.len(),
            bytes: Some(body.len()),
        });
    }
    let digest = Sha256::digest(&body);
    let etag = format!(
        "\"{}\"",
        digest[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    Ok(Built {
        body,
        etag,
        listed,
        skipped,
    })
}

impl std::error::Error for Overflow {}

impl std::fmt::Display for Overflow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.bytes {
            Some(bytes) => write!(
                f,
                "the registry document would be {bytes} bytes, more than {MAX_BODY_BYTES}"
            ),
            None => write!(
                f,
                "the registry document would list {} items, more than {MAX_ITEMS}",
                self.items
            ),
        }
    }
}
