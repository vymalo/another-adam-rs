//! What a tool needs to share a file as a [file artifact](Artifact::file): the media type its bytes
//! support, the usual extension of a media type, and the line the model reads instead of the bytes.
//!
//! One rule for every source of a file: the coder's `share_file` (a media type claimed by the file's
//! extension), the files of an MCP server's result (`adam-mcp`) and those of a remote agent's task
//! (`adam-assembly`), both claimed by their sender.

use crate::events::Artifact;

/// What a file is when nothing better can be said of it: bytes to download, never a picture.
const OCTETS: &str = "application/octet-stream";

/// How much of a file is looked at for `<svg`.
const SVG_PROBE_BYTES: usize = 4096;

/// The image types whose bytes [`sniff_image`] recognises. A file claimed to be one of them must
/// be that image by its bytes.
const CHECKED_IMAGES: [&str; 5] = [
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/svg+xml",
];

/// The image `bytes` are, by their first bytes: PNG, JPEG, GIF, WebP, or SVG (text with no NUL that
/// holds an `<svg` element in its first 4 KiB, after any BOM, XML declaration, doctype or comment).
/// `None` for anything else.
pub fn sniff_image(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if looks_like_svg(bytes) {
        Some("image/svg+xml")
    } else {
        None
    }
}

fn looks_like_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(SVG_PROBE_BYTES)];
    if head.contains(&0) {
        return false;
    }
    let head = head.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(head);
    let text = String::from_utf8_lossy(head).to_ascii_lowercase();
    let mut rest = text.trim_start();
    // What may come before the root element: `<?xml ...?>`, `<!DOCTYPE ...>`, `<!-- ... -->`.
    loop {
        let skipped = if let Some(after) = rest.strip_prefix("<?") {
            after.split_once("?>").map(|(_, tail)| tail)
        } else if let Some(after) = rest.strip_prefix("<!--") {
            after.split_once("-->").map(|(_, tail)| tail)
        } else if let Some(after) = rest.strip_prefix("<!") {
            after.split_once('>').map(|(_, tail)| tail)
        } else {
            None
        };
        match skipped {
            Some(tail) => rest = tail.trim_start(),
            None => break,
        }
    }
    rest.starts_with("<svg")
        && rest[4..]
            .chars()
            .next()
            .is_some_and(|c| c.is_whitespace() || c == '>' || c == '/')
}

/// The media type to share `bytes` as, given what was `claimed` for them (by their sender, or by a
/// file's extension), checked against the bytes:
///
/// * an image [`sniff_image`] knows must be that image by its bytes (a "PNG" that is an HTML page is
///   not one);
/// * bytes that are an image, claimed as something else, disagree;
/// * with no usable claim, an image is the image its bytes say.
///
/// Everything that disagrees, and anything that cannot be told, is `application/octet-stream`. A
/// claim is lowercased, loses its parameters (`; charset=utf-8`) and must be `type/subtype`;
/// `image/jpg` reads as `image/jpeg`. A claim this rule cannot check (`application/pdf`,
/// `image/avif`) stands.
pub fn checked_media_type(claimed: Option<&str>, bytes: &[u8]) -> String {
    let sniffed = sniff_image(bytes);
    let claimed = claimed.and_then(normalised);
    match (claimed, sniffed) {
        (Some(claim), Some(image)) if claim == image => claim,
        (Some(_), Some(_)) => OCTETS.to_owned(),
        (Some(claim), None) if CHECKED_IMAGES.contains(&claim.as_str()) => OCTETS.to_owned(),
        (Some(claim), None) => claim,
        (None, Some(image)) => image.to_owned(),
        (None, None) => OCTETS.to_owned(),
    }
}

/// `claimed` as a bare, lowercase `type/subtype`, or `None` when it is not one.
fn normalised(claimed: &str) -> Option<String> {
    let bare = claimed
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let (kind, sub) = bare.split_once('/')?;
    let token = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$&-^_.+".contains(&b))
    };
    if !token(kind) || !token(sub) {
        return None;
    }
    Some(if bare == "image/jpg" {
        "image/jpeg".to_owned()
    } else {
        bare
    })
}

/// The usual extension of `media_type`, without the dot (`png` for `image/png`), and `bin` for a type
/// this table does not know.
pub fn extension_of(media_type: &str) -> &'static str {
    match media_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "image/avif" => "avif",
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        "audio/mpeg" => "mp3",
        "audio/ogg" => "ogg",
        "video/mp4" => "mp4",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "text/markdown" => "md",
        "text/csv" => "csv",
        "text/tab-separated-values" => "tsv",
        "text/html" => "html",
        "text/css" => "css",
        "application/json" => "json",
        "application/xml" | "text/xml" => "xml",
        "application/yaml" => "yaml",
        "application/zip" => "zip",
        "application/gzip" => "gz",
        "application/x-tar" => "tar",
        _ => "bin",
    }
}

impl Artifact {
    /// What a tool tells the model it shared, in place of the bytes, in one line: `Shared report.pdf
    /// (84.0 KiB, application/pdf).` for a file artifact; for an image, also how to show it inline,
    /// by the file's name (what the person's screen resolves an image's source against), never by a
    /// path: `Shared chart.png (1.5 KiB, image/png). To show it in your answer, write
    /// ![description](chart.png).` A JSON artifact is `Shared <name>.`
    pub fn shared_line(&self) -> String {
        let Some(file) = &self.file else {
            return format!("Shared {}.", self.name);
        };
        let media_type = self.mime_type.as_deref().unwrap_or(OCTETS);
        let mut line = format!(
            "Shared {} ({}, {media_type}).",
            file.filename,
            human_size(file.bytes.len())
        );
        if media_type.starts_with("image/") {
            line.push_str(&format!(
                " To show it in your answer, write ![description]({}).",
                link_destination(&file.filename)
            ));
        }
        line
    }
}

/// `name` as the destination of a Markdown link: as it is, or between `<` and `>` (with `<`, `>`
/// and `\` escaped) when it holds a space, a parenthesis or one of those, which a bare destination
/// cannot (CommonMark 0.31, "Links").
fn link_destination(name: &str) -> String {
    if !name.contains([' ', '(', ')', '<', '>', '\\']) {
        return name.to_owned();
    }
    let mut out = String::with_capacity(name.len() + 2);
    out.push('<');
    for c in name.chars() {
        if matches!(c, '<' | '>' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('>');
    out
}

/// `1234` bytes as `1.2 KiB`.
pub(crate) fn human_size(len: usize) -> String {
    #[allow(clippy::cast_precision_loss)] // a file of at most a few MiB
    let len_f = len as f64;
    if len < 1024 {
        format!("{len} bytes")
    } else if len < 1024 * 1024 {
        format!("{:.1} KiB", len_f / 1024.0)
    } else {
        format!("{:.1} MiB", len_f / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
    const SVG: &[u8] = b"<svg xmlns='http://www.w3.org/2000/svg' width='4' height='4'/>";

    #[test]
    fn images_are_known_by_their_first_bytes() {
        assert_eq!(sniff_image(PNG), Some("image/png"));
        assert_eq!(sniff_image(b"\xFF\xD8\xFF\xE0 jfif"), Some("image/jpeg"));
        assert_eq!(sniff_image(b"GIF89a...."), Some("image/gif"));
        assert_eq!(sniff_image(b"RIFF\x10\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff_image(SVG), Some("image/svg+xml"));
        let declared = b"\xEF\xBB\xBF<?xml version=\"1.0\"?>\n<!DOCTYPE svg PUBLIC \"x\" \"y\">\n<!-- made by a script -->\n<SVG viewBox='0 0 1 1'></SVG>";
        assert_eq!(sniff_image(declared), Some("image/svg+xml"));
        for not in [
            b"%PDF-1.7".as_slice(),
            b"<svgx>not an svg element",
            b"<svg\0>",
            b"just text",
            b"",
        ] {
            assert_eq!(sniff_image(not), None, "{not:?}");
        }
    }

    #[test]
    fn a_claim_stands_unless_the_bytes_disagree() {
        for (claimed, bytes, expected) in [
            (Some("image/png"), PNG, "image/png"),
            (Some("IMAGE/PNG"), PNG, "image/png"),
            (
                Some("image/jpg"),
                b"\xFF\xD8\xFF\xDB".as_slice(),
                "image/jpeg",
            ),
            (Some("image/svg+xml"), SVG, "image/svg+xml"),
            (Some("application/pdf"), b"%PDF-1.7", "application/pdf"),
            (
                Some("text/plain; charset=utf-8"),
                b"hello".as_slice(),
                "text/plain",
            ),
            // A type this rule cannot check stands.
            (Some("image/avif"), b"....ftypavif", "image/avif"),
            // A claimed image that its bytes are not.
            (Some("image/png"), b"<html>not a png</html>", OCTETS),
            (Some("image/png"), SVG, OCTETS),
            (Some("image/svg+xml"), b"just some text", OCTETS),
            // An image claimed as something else.
            (Some("application/pdf"), PNG, OCTETS),
            (Some("text/plain"), SVG, OCTETS),
            // No usable claim: the bytes say.
            (None, PNG, "image/png"),
            (Some("png"), PNG, "image/png"),
            (Some("image/ png"), SVG, "image/svg+xml"),
            (None, b"\0\x01\x02", OCTETS),
            (Some(""), b"text", OCTETS),
        ] {
            assert_eq!(
                checked_media_type(claimed, bytes),
                expected,
                "{claimed:?} {bytes:?}"
            );
        }
    }

    #[test]
    fn every_extension_of_the_table_names_a_type_that_has_one() {
        assert_eq!(extension_of("image/png"), "png");
        assert_eq!(extension_of("application/pdf"), "pdf");
        assert_eq!(extension_of("image/jpeg"), "jpg");
        assert_eq!(extension_of("application/x-unknown"), "bin");
        assert_eq!(extension_of(OCTETS), "bin");
    }

    #[test]
    fn the_shared_line_says_name_size_and_type_never_the_bytes() {
        let file = Artifact::file("chart", "image/png", "chart.png", vec![7; 1536]).unwrap();
        assert_eq!(
            file.shared_line(),
            "Shared chart.png (1.5 KiB, image/png). To show it in your answer, write \
             ![description](chart.png)."
        );
        // A name a bare link cannot hold is put between angle brackets.
        let spaced = Artifact::file("c", "image/svg+xml", "my chart (v2).svg", vec![1]).unwrap();
        assert!(
            spaced
                .shared_line()
                .ends_with("write ![description](<my chart (v2).svg>)."),
            "{}",
            spaced.shared_line()
        );
        assert_eq!(link_destination("a<b>\\c d"), "<a\\<b\\>\\\\c d>");
        // Only an image is shown inline; any other file is offered as it is.
        let pdf = Artifact::file("r", "application/pdf", "report.pdf", vec![1; 10]).unwrap();
        assert_eq!(
            pdf.shared_line(),
            "Shared report.pdf (10 bytes, application/pdf)."
        );
        let json = Artifact::new("checks", None, serde_json::json!({"passed": true}));
        assert_eq!(json.shared_line(), "Shared checks.");
    }

    #[test]
    fn sizes_are_said_in_plain_units() {
        assert_eq!(human_size(0), "0 bytes");
        assert_eq!(human_size(1023), "1023 bytes");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(4 * 1024 * 1024), "4.0 MiB");
    }
}
