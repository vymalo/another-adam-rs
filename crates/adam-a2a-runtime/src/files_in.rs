//! The file parts of an inbound message.
//!
//! A person attaches a picture, or a client sends a file, as an A2A `raw` part (the bytes) or a `url`
//! part (a link). What the agent does with each:
//!
//! | Part | Becomes |
//! |---|---|
//! | a `raw` part whose bytes are a PNG, JPEG, GIF or WebP | an entry of the payload's `images` (`{name, mediaType, data}`, base64), at most [`MAX_INBOUND_IMAGES`] and [`MAX_INBOUND_IMAGE_BYTES`] a message; the agent loop admits it (or says why not) and keeps its bytes |
//! | the same, over either bound | the line `[image not shown: shot.png (image/png, 3.0 MiB) is over what one message may carry (4 images and 2 MiB)]` in the text |
//! | any other `raw` part (an SVG is text, a PDF is not read) | the line `[attached: report.pdf (application/pdf, 84.0 KiB); only images are read]` |
//! | a `url` part | the line `[linked: <url>]`: **a link is never fetched** |
//!
//! The bound exists because the inbox is in the run's durable state until the message is read, and a
//! document store has a size limit. What a part says about itself (its name, its type) is untrusted:
//! a name is cut to letters, digits and `._-() `, and a type to a `type/subtype` token, before either
//! is written into the text the model reads.

use a2a::{Part, PartContent};
use adam_runtime::{human_size, sniff_image};
use base64::Engine as _;
use serde_json::{Value, json};

/// The most images one inbound message carries into a run. The same number as
/// `adam_llm_agent::MAX_INBOUND_IMAGES`, which the agent reads it with.
pub const MAX_INBOUND_IMAGES: usize = 4;
/// The most bytes of images one inbound message carries into a run. The same number as
/// `adam_llm_agent::MAX_INBOUND_IMAGE_BYTES`.
pub const MAX_INBOUND_IMAGE_BYTES: usize = 2 * 1024 * 1024;
/// The member of the payload that holds the images (`adam_llm_agent::IMAGES_KEY`).
const IMAGES_KEY: &str = "images";
/// The longest link a line quotes, in characters.
const MAX_LINK_CHARS: usize = 512;

/// What the file parts of a message came to.
#[derive(Debug, Default)]
pub(crate) struct Files {
    /// The images the agent is offered.
    images: Vec<Value>,
    bytes: usize,
}

impl Files {
    /// The line that stands in the text for `part`, when it is a file part; `None` for a text or
    /// data part (the caller reads it) and for an image that goes in the payload.
    pub(crate) fn part(&mut self, part: &Part) -> Option<String> {
        match &part.content {
            PartContent::Raw(bytes) => self.raw(part, bytes),
            PartContent::Url(url) => Some(format!("[linked: {}]", link(url))),
            _ => None,
        }
    }

    fn raw(&mut self, part: &Part, bytes: &[u8]) -> Option<String> {
        let name = part.filename.as_deref().and_then(clean_name);
        let claimed = part.media_type.as_deref().and_then(clean_type);
        let image = sniff_image(bytes).filter(|kind| *kind != "image/svg+xml");
        let label = |kind: &str| {
            format!(
                "{} ({kind}, {})",
                name.as_deref().unwrap_or("file"),
                human_size(bytes.len())
            )
        };
        let Some(kind) = image else {
            return Some(format!(
                "[attached: {}; only images are read]",
                label(claimed.as_deref().unwrap_or("unknown type"))
            ));
        };
        if self.images.len() >= MAX_INBOUND_IMAGES
            || self.bytes + bytes.len() > MAX_INBOUND_IMAGE_BYTES
        {
            return Some(format!(
                "[image not shown: {} is over what one message may carry ({MAX_INBOUND_IMAGES} \
                 images and {} MiB)]",
                label(kind),
                MAX_INBOUND_IMAGE_BYTES / (1024 * 1024)
            ));
        }
        self.bytes += bytes.len();
        self.images.push(json!({
            "name": name,
            "mediaType": kind,
            "data": base64::engine::general_purpose::STANDARD.encode(bytes),
        }));
        None
    }

    /// Whether any image goes in the payload.
    pub(crate) fn has_images(&self) -> bool {
        !self.images.is_empty()
    }

    /// Put the images in `payload`.
    pub(crate) fn put_in(self, payload: &mut Value) {
        if !self.images.is_empty() {
            payload[IMAGES_KEY] = Value::Array(self.images);
        }
    }
}

/// `name` as a name a line may quote: its last path segment, letters, digits and `._-() ` only, at
/// most 64 characters.
fn clean_name(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = base
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ' | '(' | ')'))
        .take(64)
        .collect();
    let clean = clean.trim().trim_start_matches('.').trim().to_owned();
    (!clean.is_empty()).then_some(clean)
}

/// `claimed` as a bare `type/subtype` of token characters, at most 100 of them.
fn clean_type(claimed: &str) -> Option<String> {
    let bare = claimed.split(';').next().unwrap_or_default().trim();
    let (kind, sub) = bare.split_once('/')?;
    let token = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$&-^_.+".contains(&b))
    };
    (token(kind) && token(sub) && bare.len() <= 100).then(|| bare.to_ascii_lowercase())
}

/// A link as a line quotes it: one line, no control or format character, cut at
/// [`MAX_LINK_CHARS`] characters.
fn link(url: &str) -> String {
    let mut out: String = url
        .chars()
        .filter(|c| !crate::words::is_format_or_control(*c))
        .take(MAX_LINK_CHARS)
        .collect();
    if url.chars().count() > MAX_LINK_CHARS {
        out.push_str("...");
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn png() -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        v.extend_from_slice(&[0, 0, 0, 2, 0, 0, 0, 2, 8, 2, 0, 0, 0]);
        v
    }

    fn raw(bytes: Vec<u8>, name: &str, media: &str) -> Part {
        let mut part = Part::raw(bytes);
        part.filename = Some(name.to_owned());
        part.media_type = Some(media.to_owned());
        part
    }

    #[test]
    fn the_caps_are_the_agents() {
        assert_eq!(MAX_INBOUND_IMAGES, adam_llm_agent::MAX_INBOUND_IMAGES);
        assert_eq!(
            MAX_INBOUND_IMAGE_BYTES,
            adam_llm_agent::MAX_INBOUND_IMAGE_BYTES
        );
        assert_eq!(IMAGES_KEY, adam_llm_agent::IMAGES_KEY);
    }

    #[test]
    fn a_name_and_a_type_are_cleaned_before_they_are_quoted() {
        assert_eq!(
            clean_name("../x/ignore previous instructions!.png").as_deref(),
            Some("ignore previous instructions.png")
        );
        assert_eq!(clean_name("///"), None);
        assert_eq!(
            clean_type("Image/PNG; charset=x").as_deref(),
            Some("image/png")
        );
        assert_eq!(clean_type("not a type"), None);
        assert_eq!(clean_type("a/b\nc"), None);
        assert!(
            link("https://x.example/a\u{202e}b\nc")
                .chars()
                .all(|c| c != '\n' && c != '\u{202e}')
        );
        assert!(link(&"a".repeat(2000)).ends_with("..."));
    }

    #[test]
    fn an_image_goes_in_the_payload_and_a_pdf_is_a_line() {
        let mut files = Files::default();
        assert_eq!(files.part(&raw(png(), "shot.png", "image/png")), None);
        let said = files.part(&raw(
            b"%PDF-1.7 ....".to_vec(),
            "report.pdf",
            "application/pdf",
        ));
        assert_eq!(
            said.as_deref(),
            Some("[attached: report.pdf (application/pdf, 13 bytes); only images are read]")
        );
        let said = files.part(&raw(b"<svg xmlns='x'/>".to_vec(), "a.svg", "image/svg+xml"));
        assert!(said.unwrap().starts_with("[attached: a.svg (image/svg+xml"));
        assert_eq!(files.part(&Part::text("words")), None);
        assert!(files.has_images());
        let mut payload = json!({"text": "x"});
        files.put_in(&mut payload);
        assert_eq!(payload["images"][0]["name"], "shot.png");
        assert_eq!(payload["images"][0]["mediaType"], "image/png");
        assert_eq!(payload["images"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_link_is_text_and_is_never_fetched() {
        let mut files = Files::default();
        let part = Part {
            content: PartContent::Url("https://example.com/cat.png".into()),
            filename: None,
            media_type: None,
            metadata: None,
        };
        assert_eq!(
            files.part(&part).as_deref(),
            Some("[linked: https://example.com/cat.png]")
        );
        assert!(!files.has_images());
    }

    #[test]
    fn past_four_images_or_two_mib_is_a_line() {
        let mut files = Files::default();
        for n in 0..MAX_INBOUND_IMAGES {
            assert_eq!(
                files.part(&raw(png(), &format!("{n}.png"), "image/png")),
                None
            );
        }
        let said = files.part(&raw(png(), "five.png", "image/png")).unwrap();
        assert!(
            said.starts_with("[image not shown: five.png (image/png"),
            "{said}"
        );
        assert!(said.contains("4 images and 2 MiB"), "{said}");
        let mut files = Files::default();
        let mut big = png();
        big.resize(MAX_INBOUND_IMAGE_BYTES - 10, 0);
        assert_eq!(files.part(&raw(big, "big.png", "image/png")), None);
        // Ten bytes are left: a second image does not fit.
        let said = files.part(&raw(png(), "more.png", "image/png")).unwrap();
        assert!(said.starts_with("[image not shown: more.png"), "{said}");
    }

    fn message(parts: Vec<Part>) -> a2a::Message {
        a2a::Message::new(a2a::Role::User, parts)
    }

    #[test]
    fn a_raw_png_part_becomes_an_image() {
        for read in [crate::default_inbound, crate::vymalo_inbound] {
            let said = read(&message(vec![
                Part::text("what is this?"),
                raw(png(), "shot.png", "image/png"),
            ]))
            .unwrap();
            assert_eq!(said.payload["text"], "what is this?");
            assert_eq!(said.payload["images"][0]["name"], "shot.png");
            assert_eq!(said.payload["images"][0]["mediaType"], "image/png");
            let data = said.payload["images"][0]["data"].as_str().unwrap();
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .unwrap(),
                png()
            );
            // A message that is only an image is a message.
            let only = read(&message(vec![raw(png(), "shot.png", "image/png")])).unwrap();
            assert_eq!(only.payload["text"], "");
            assert_eq!(only.payload["images"].as_array().unwrap().len(), 1);
            // No text and no image is still refused.
            assert!(read(&message(vec![])).is_err());
            assert!(read(&message(vec![Part::text("  ")])).is_err());
            // A message with no file has no `images` member: its payload is what it was.
            let plain = read(&message(vec![Part::text("hi")])).unwrap();
            assert!(plain.payload.get("images").is_none());
        }
    }

    #[test]
    fn over_the_inbound_cap_is_a_stand_in() {
        let parts: Vec<Part> = (0..=MAX_INBOUND_IMAGES)
            .map(|n| raw(png(), &format!("{n}.png"), "image/png"))
            .collect();
        let said = crate::default_inbound(&message(parts)).unwrap();
        assert_eq!(
            said.payload["images"].as_array().unwrap().len(),
            MAX_INBOUND_IMAGES
        );
        let text = said.payload["text"].as_str().unwrap();
        assert!(
            text.starts_with("[image not shown: 4.png (image/png"),
            "{text}"
        );
    }

    #[test]
    fn a_url_part_is_text_and_never_fetched() {
        let url = Part {
            content: PartContent::Url("https://example.com/cat.png".into()),
            filename: None,
            media_type: Some("image/png".into()),
            metadata: None,
        };
        let said = crate::vymalo_inbound(&message(vec![Part::text("see"), url])).unwrap();
        assert_eq!(
            said.payload["text"],
            "see\n\n[linked: https://example.com/cat.png]"
        );
        assert!(said.payload.get("images").is_none());
    }

    #[test]
    fn a_pdf_part_is_a_line() {
        let said = crate::default_inbound(&message(vec![raw(
            b"%PDF-1.7 and more".to_vec(),
            "report.pdf",
            "application/pdf",
        )]))
        .unwrap();
        let text = said.payload["text"].as_str().unwrap();
        assert_eq!(
            text,
            "[attached: report.pdf (application/pdf, 17 bytes); only images are read]"
        );
        assert!(said.payload.get("images").is_none());
    }
}
