//! The document builder: what it lists and leaves out, its limits, its golden bytes and `ETag`, and that
//! the system's own reader (vendored in `tests/vendored`) reads what it builds, with nothing skipped.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod support;

use adam_operator_ports::DirectoryEntry;
use adam_operator_registry::document::{
    MAX_BODY_BYTES, MAX_ID_CHARS, MAX_ITEMS, MAX_TAG_CHARS, MAX_TAGS, MAX_TITLE_CHARS, PROFILE,
    is_valid_service_id,
};
use adam_operator_registry::{Overflow, build};
use proptest::prelude::*;
use serde_json::{Value, json};
use support::{consumer, entry, titled};

fn items_of(body: &[u8]) -> Vec<Value> {
    let doc: Value = serde_json::from_slice(body).unwrap();
    doc["linkset"][0]["item"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn the_fleet() -> Vec<DirectoryEntry> {
    let mut blocked = entry("another-agentic-system", "broken");
    blocked.blocked = true;
    let mut off = entry("another-agentic-system", "no-a2a");
    off.a2a_enabled = false;
    let mut no_card = entry("another-agentic-system", "new");
    no_card.agent_card = None;
    vec![
        titled(
            "another-agentic-system",
            "researcher",
            "Researcher",
            &["research"],
        ),
        titled(
            "another-agentic-system",
            "coder",
            "Coder",
            &["coding", "git"],
        ),
        entry("another-agentic-system", "chat"),
        blocked,
        off,
        no_card,
        // The same name in another namespace: the second is not listed.
        titled("zz-other", "chat", "Another chat", &[]),
    ]
}

#[test]
fn it_lists_what_the_directory_lists_in_scope_and_name_order() {
    let built = build(&the_fleet(), None).unwrap();
    assert_eq!(built.listed, ["chat", "coder", "researcher"]);
    let items = items_of(&built.body);
    assert_eq!(items.len(), 3);
    assert_eq!(
        items[1],
        json!({
            "href": "http://coder.another-agentic-system.svc:8080/.well-known/agent-card.json",
            "type": "application/json",
            "title": "Coder",
            "service": ["coder"],
            "tags": ["coding", "git"],
        })
    );
    // No title and no tags: the members are absent, and a reader defaults the title to the id.
    assert_eq!(
        items[0],
        json!({
            "href": "http://chat.another-agentic-system.svc:8080/.well-known/agent-card.json",
            "type": "application/json",
            "service": ["chat"],
        })
    );
}

#[test]
fn what_the_directory_does_not_list_is_not_an_omission_and_a_second_namespace_is() {
    let built = build(&the_fleet(), None).unwrap();
    assert_eq!(built.skipped.len(), 1, "{:?}", built.skipped);
    assert_eq!(built.skipped[0].entry, "zz-other/chat");
}

#[test]
fn the_document_is_the_same_for_the_same_entries_in_any_order() {
    let mut shuffled = the_fleet();
    shuffled.reverse();
    let (a, b) = (
        build(&the_fleet(), None).unwrap(),
        build(&shuffled, None).unwrap(),
    );
    assert_eq!(a.body, b.body);
    assert_eq!(a.etag, b.etag);
}

#[test]
fn the_etag_is_strong_and_follows_the_body() {
    let a = build(&the_fleet(), None).unwrap();
    assert!(a.etag.starts_with('"') && a.etag.ends_with('"') && !a.etag.starts_with("W/"));
    let mut changed = the_fleet();
    changed[1].title = Some("Coder 2".to_owned());
    let b = build(&changed, None).unwrap();
    assert_ne!(a.etag, b.etag);
    let with_anchor = build(
        &the_fleet(),
        Some("http://op.ns.svc:8080/registry/v1/agents"),
    )
    .unwrap();
    assert_ne!(
        a.etag, with_anchor.etag,
        "the anchor is part of the document"
    );
}

#[test]
fn the_golden_document_and_etag() {
    let built = build(
        &the_fleet(),
        Some("http://operator.another-agentic-system.svc:8080/registry/v1/agents"),
    )
    .unwrap();
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/golden-document.json");
    if matches!(
        std::env::var("ADAM_OPERATOR_UPDATE_GOLDENS").as_deref(),
        Ok("1")
    ) {
        std::fs::write(&path, &built.body).unwrap();
    }
    let golden = std::fs::read(&path).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&built.body),
        String::from_utf8_lossy(&golden),
        "regenerate with ADAM_OPERATOR_UPDATE_GOLDENS=1 and read the diff"
    );
    assert_eq!(built.etag, "\"ed6f1f870ea9083b4c60af055db8d1d4\"");
}

#[test]
fn the_context_object_has_the_version_marker_and_the_anchor_only_when_given() {
    let doc: Value = serde_json::from_slice(&build(&[], None).unwrap().body).unwrap();
    assert_eq!(doc, json!({"linkset": [{"profile": [{"href": PROFILE}]}]}));
    let doc: Value = serde_json::from_slice(
        &build(&[], Some("http://x/registry/v1/agents"))
            .unwrap()
            .body,
    )
    .unwrap();
    assert_eq!(doc["linkset"][0]["anchor"], "http://x/registry/v1/agents");
}

#[test]
fn a_card_that_a_reader_would_skip_is_not_listed() {
    let bad = [
        ("relative", "/relative/card.json"),
        ("ftp", "ftp://x.example.com/card"),
        ("creds", "https://user:pw@x.example.com/card"),
        ("nonsense", "not a url"),
        ("empty", ""),
    ];
    let mut entries = vec![entry("ns", "good")];
    for (name, card) in bad {
        let mut e = entry("ns", name);
        e.agent_card = Some(card.to_owned());
        entries.push(e);
    }
    // And a name that is no service id.
    entries.push(entry("ns", "Not_Valid"));
    entries.push(entry("ns", &"a".repeat(MAX_ID_CHARS + 1)));
    let built = build(&entries, None).unwrap();
    assert_eq!(built.listed, ["good"]);
    assert_eq!(built.skipped.len(), 7);
    assert!(
        built
            .skipped
            .iter()
            .all(|s| !s.reason.is_empty() && s.entry.starts_with("ns/"))
    );
}

#[test]
fn titles_and_tags_are_made_to_fit_the_contract() {
    let long_title = "é".repeat(MAX_TITLE_CHARS + 50);
    let many: Vec<String> = (0..MAX_TAGS + 4).map(|i| format!("t{i}")).collect();
    let mut tags: Vec<String> = vec![
        String::new(),
        "x".repeat(MAX_TAG_CHARS + 1),
        "dup".to_owned(),
        "dup".to_owned(),
        "x".repeat(MAX_TAG_CHARS),
    ];
    tags.extend(many);
    let mut e = entry("ns", "tagged");
    e.title = Some(long_title);
    e.tags = tags;
    let mut blank = entry("ns", "blank");
    blank.title = Some("   ".to_owned());
    blank.tags = vec![String::new()];
    let built = build(&[e, blank], None).unwrap();
    let items = items_of(&built.body);
    let blank_item = &items[0];
    assert!(blank_item.get("title").is_none() && blank_item.get("tags").is_none());
    let tagged = &items[1];
    assert_eq!(
        tagged["title"].as_str().unwrap().chars().count(),
        MAX_TITLE_CHARS
    );
    let kept = tagged["tags"].as_array().unwrap();
    assert_eq!(kept.len(), MAX_TAGS);
    assert_eq!(kept[0], "dup");
    assert_eq!(kept[1].as_str().unwrap().len(), MAX_TAG_CHARS);
    assert_eq!(kept.iter().filter(|t| *t == "dup").count(), 1);
}

#[test]
fn the_limits_are_refusals_and_not_truncations() {
    let fits: Vec<DirectoryEntry> = (0..MAX_ITEMS)
        .map(|i| entry("ns", &format!("a{i}")))
        .collect();
    let built = build(&fits, None).unwrap();
    assert_eq!(built.listed.len(), MAX_ITEMS);
    assert!(built.body.len() <= MAX_BODY_BYTES);

    let too_many: Vec<DirectoryEntry> = (0..=MAX_ITEMS)
        .map(|i| entry("ns", &format!("a{i}")))
        .collect();
    assert_eq!(
        build(&too_many, None).unwrap_err(),
        Overflow {
            items: MAX_ITEMS + 1,
            bytes: None
        }
    );

    // 500 items whose titles make the body pass 1 MiB.
    let heavy: Vec<DirectoryEntry> = (0..MAX_ITEMS)
        .map(|i| {
            let mut e = entry("ns", &format!("a{i}"));
            e.title = Some("é".repeat(MAX_TITLE_CHARS));
            e.agent_card = Some(format!("http://a{i}.ns.svc:8080/{}", "p/".repeat(300)));
            e.tags = (0..MAX_TAGS)
                .map(|t| format!("{t:0>width$}", width = MAX_TAG_CHARS))
                .collect();
            e
        })
        .collect();
    let err = build(&heavy, None).unwrap_err();
    assert!(err.bytes.is_some_and(|b| b > MAX_BODY_BYTES), "{err}");
    assert_eq!(err.items, MAX_ITEMS);
}

#[test]
fn an_overflow_says_what_it_is() {
    let text = Overflow {
        items: 501,
        bytes: None,
    }
    .to_string();
    assert!(text.contains("501") && text.contains("500"), "{text}");
    let text = Overflow {
        items: 3,
        bytes: Some(2_000_000),
    }
    .to_string();
    assert!(text.contains("2000000"), "{text}");
}

#[test]
fn service_ids_are_the_contracts() {
    for good in ["a", "0", "coder", "coder-2", "a-", &"a".repeat(63)] {
        assert!(is_valid_service_id(good), "{good}");
    }
    for bad in ["", "Coder", "-x", "a_b", "a.b", &"a".repeat(64)] {
        assert!(!is_valid_service_id(bad), "{bad}");
    }
}

// ---------------------------------------------------------------- the consumer

#[test]
fn the_consumer_reads_the_fleet_in_full_with_nothing_skipped() {
    let built = build(
        &the_fleet(),
        Some("http://op.ns.svc:8080/registry/v1/agents"),
    )
    .unwrap();
    let read = consumer::parse(&built.body).expect("the consumer's reader accepts it");
    assert!(read.skipped.is_empty(), "{:?}", read.skipped);
    let ids: Vec<&str> = read.items.iter().map(|i| i.service.as_str()).collect();
    assert_eq!(ids, built.listed);
    let coder = read.items.iter().find(|i| i.service == "coder").unwrap();
    assert_eq!(coder.title, "Coder");
    assert_eq!(coder.tags, ["coding", "git"]);
    assert_eq!(
        coder.href,
        "http://coder.another-agentic-system.svc:8080/.well-known/agent-card.json"
    );
    let chat = read.items.iter().find(|i| i.service == "chat").unwrap();
    assert_eq!(
        chat.title, "chat",
        "no title: the reader defaults it to the id"
    );
}

#[test]
fn the_consumer_reads_an_empty_registry_as_an_empty_list_and_not_an_unreadable_one() {
    let built = build(&[], None).unwrap();
    let read = consumer::parse(&built.body).unwrap();
    assert!(read.items.is_empty() && read.skipped.is_empty());
}

#[test]
fn the_vendored_documents_read_as_the_consumer_reads_them() {
    // The fixtures are what the consumer's own tests and compose stack use: the reader here is the
    // consumer's, so a drift of the vendored copy shows as a change of what these documents say.
    for (name, expect) in [
        ("contract-example.json", vec!["coder", "researcher"]),
        ("consumer-mock-registry-agents.json", vec!["platform-coder"]),
    ] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        let read = consumer::parse(&std::fs::read(path).unwrap()).unwrap();
        let ids: Vec<&str> = read.items.iter().map(|i| i.service.as_str()).collect();
        assert_eq!(ids, expect, "{name}");
        assert!(read.skipped.is_empty(), "{name}");
    }
}

#[test]
fn a_document_at_the_limits_is_still_readable_by_the_consumer() {
    let fits: Vec<DirectoryEntry> = (0..MAX_ITEMS)
        .map(|i| entry("ns", &format!("a{i}")))
        .collect();
    let built = build(&fits, None).unwrap();
    let read = consumer::parse(&built.body).unwrap();
    assert_eq!(read.items.len(), MAX_ITEMS);
}

fn arbitrary_entry() -> impl Strategy<Value = DirectoryEntry> {
    let name = prop_oneof![
        8 => "[a-z0-9][a-z0-9-]{0,62}",
        1 => "[A-Za-z0-9_.-]{0,70}",
    ];
    let scope = "[a-z][a-z0-9-]{0,10}";
    let title = proptest::option::of(prop_oneof!["[ -~]{0,20}", "\\PC{0,300}"]);
    let tags = proptest::collection::vec(prop_oneof!["[a-z-]{0,10}", "\\PC{0,80}"], 0..24);
    let card = proptest::option::weighted(
        0.9,
        prop_oneof![
            4 => "https?://[a-z0-9.-]{1,20}(:[0-9]{2,4})?/[a-z./-]{0,30}",
            1 => "\\PC{0,40}",
        ],
    );
    (name, scope, title, tags, card, any::<bool>(), any::<bool>()).prop_map(
        |(name, scope, title, tags, agent_card, a2a_enabled, blocked)| DirectoryEntry {
            scope,
            name,
            title,
            description: None,
            tags,
            agent_card,
            a2a_enabled,
            blocked,
        },
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Whatever the directory holds, the builder either refuses the document or writes one the
    /// consumer's reader takes without skipping an item, and lists exactly the ids it says it lists.
    #[test]
    fn whatever_the_directory_holds_the_consumer_skips_nothing(
        entries in proptest::collection::vec(arbitrary_entry(), 0..40)
    ) {
        let Ok(built) = build(&entries, None) else { return Ok(()); };
        let read = consumer::parse(&built.body).expect("readable");
        prop_assert!(read.skipped.is_empty(), "{:?}", read.skipped);
        let ids: Vec<&str> = read.items.iter().map(|i| i.service.as_str()).collect();
        prop_assert_eq!(ids, built.listed.iter().map(String::as_str).collect::<Vec<_>>());
        // Every listed id is a listed entry of the directory.
        for id in &built.listed {
            prop_assert!(entries.iter().any(|e| &e.name == id && e.listed()));
        }
    }
}
