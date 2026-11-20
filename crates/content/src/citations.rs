//! Shared display/portable projection of Responses URL annotations. Raw text
//! and annotations are never modified in native replay.

use rig_core::message::{AssistantContent, Text};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

pub(crate) const MAX_SOURCES: usize = 64;
pub(crate) const MAX_SOURCE_URL_BYTES: usize = 8 * 1024;
pub(crate) const MAX_SOURCE_TITLE_CHARS: usize = 512;

/// A display-only citation, independent of its position in the answer text.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CitedSource {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlDisplay {
    pub host: String,
    pub rest: String,
}

/// Readable parts of a page address. Opening it still requires `safe_http_url`.
pub fn url_display(raw: &str) -> Option<UrlDisplay> {
    let parsed = url::Url::parse(raw).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let host = parsed.host_str()?;
    let host = host.strip_prefix("www.").unwrap_or(host).to_owned();
    let mut rest = match parsed.path() {
        "/" => String::new(),
        path => path.to_owned(),
    };
    if let Some(query) = parsed.query() {
        rest.push('?');
        rest.push_str(query);
    }
    if let Some(fragment) = parsed.fragment() {
        rest.push('#');
        rest.push_str(fragment);
    }
    Some(UrlDisplay { host, rest })
}

/// Collect usable cited pages in annotation order, even when their offsets
/// cannot yet be applied to the live answer. The first citation owns its title.
pub fn sources(annotations: Option<&Value>) -> Vec<CitedSource> {
    let mut sources = Vec::new();
    let mut seen = HashSet::new();
    for annotation in annotations.and_then(Value::as_array).into_iter().flatten() {
        if annotation.get("type").and_then(Value::as_str) != Some("url_citation") {
            continue;
        }
        let Some(url) = annotation
            .get("url")
            .and_then(Value::as_str)
            .filter(|url| url.len() <= MAX_SOURCE_URL_BYTES)
            .and_then(safe_http_url)
            .filter(|url| url.len() <= MAX_SOURCE_URL_BYTES)
        else {
            continue;
        };
        if !seen.insert(url.clone()) {
            continue;
        }
        let title = annotation
            .get("title")
            .and_then(Value::as_str)
            .map(sanitize_display)
            .map(|title| {
                title
                    .trim()
                    .chars()
                    .take(MAX_SOURCE_TITLE_CHARS)
                    .collect::<String>()
            })
            .filter(|title| !title.is_empty());
        sources.push(CitedSource { url, title });
        if sources.len() == MAX_SOURCES {
            break;
        }
    }
    sources
}

pub fn text_sources(text: &Text) -> Vec<CitedSource> {
    sources(
        text.native
            .as_ref()
            .and_then(|native| native.item.get("annotations")),
    )
}

/// Strip terminal control and Unicode directional formatting characters from
/// untrusted display metadata. Newlines in titles must not create new blocks.
pub fn sanitize_display(value: &str) -> String {
    value.chars().filter(|c| !c.is_control() && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}')).collect()
}

fn escape_title(title: &str) -> String {
    let mut escaped = String::new();
    for c in sanitize_display(title).chars() {
        // Markdown decodes HTML entities in labels too; a provider's literal
        // &Tab; or numeric entity must not turn back into terminal controls.
        if c == '&' {
            escaped.push_str("&amp;");
            continue;
        }
        if matches!(
            c,
            '\\' | '[' | ']' | '(' | ')' | '*' | '_' | '`' | '<' | '>' | '#' | '!' | '|'
        ) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// Only usable HTTP(S) destinations become links. Encode Markdown delimiters
/// after URL parsing, without shortening the full copyable destination.
pub fn safe_http_url(raw: &str) -> Option<String> {
    if raw.chars().any(|c| c.is_control()) || sanitize_display(raw) != raw {
        return None;
    }
    let parsed = url::Url::parse(raw).ok()?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return None;
    }
    Some(
        parsed
            .as_str()
            .replace('(', "%28")
            .replace(')', "%29")
            .replace('<', "%3C")
            .replace('>', "%3E")
            .replace('\\', "%5C")
            .replace('`', "%60"),
    )
}

/// An API offset addresses characters, not Rust bytes. `char_indices` converts
/// scalar-character offsets without slicing through UTF-8. Unknown or invalid
/// ranges use appended links; ordinary narrative is never replaced.
fn range(text: &str, annotation: &Value) -> Option<(usize, usize)> {
    let start = usize::try_from(annotation.get("start_index")?.as_u64()?).ok()?;
    let end = usize::try_from(annotation.get("end_index")?.as_u64()?).ok()?;
    if start > end {
        return None;
    }
    let boundary = |index| {
        text.char_indices()
            .map(|(byte, _)| byte)
            .chain(std::iter::once(text.len()))
            .nth(index)
    };
    Some((boundary(start)?, boundary(end)?))
}

fn is_citation_marker(text: &str) -> bool {
    // Recognize only the provider's citation token, not prose or arbitrary
    // Markdown links which may happen to fall within a citation's span.
    text.starts_with('\u{e200}')
        && text.ends_with('\u{e201}')
        && text
            .strip_prefix("\u{e200}cite\u{e202}")
            .is_some_and(|tail| {
                let body = tail.trim_end_matches('\u{e201}');
                !body.is_empty()
                    && body
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '\u{e202}' | '_' | '-'))
            })
}

/// Preserve Responses text metadata in Rig's native attachment rather than
/// flattening annotations into the canonical text or the request cache prefix.
pub fn text_with_openai_extras(text: impl Into<String>, extras: Value) -> Text {
    match AssistantContent::Text(Text::new(text)).with_native(extras) {
        AssistantContent::Text(text) => text,
        _ => unreachable!("attaching native data preserves the block kind"),
    }
}

pub fn render_text(text: &Text) -> String {
    render(
        &text.text,
        text.native
            .as_ref()
            .and_then(|native| native.item.get("annotations")),
    )
}

pub fn render(text: &str, annotations: Option<&Value>) -> String {
    render_inner(text, annotations, false, false)
}

/// Display-only projection of a provisional native text part. Offsets still refer
/// to the original text, before sanitizing or inserting any Markdown links.
/// A range ahead of received text may become usable on the next delta; it is
/// not an invalid-offset fallback until the text is complete.
pub fn render_preview(text: &str, annotations: Option<&Value>, text_complete: bool) -> String {
    render_inner(text, annotations, !text_complete, true)
}

fn incomplete_marker(text: &str) -> Option<usize> {
    let start = text.rfind('\u{e200}')?;
    let tail = &text[start..];
    let prefix = "\u{e200}cite\u{e202}";
    (prefix.starts_with(tail)
        || tail.strip_prefix(prefix).is_some_and(|body| {
            body.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '\u{e202}' | '_' | '-'))
        }))
    .then_some(start)
}

fn preview_markers(text: &str) -> Vec<(usize, usize)> {
    let mut markers = Vec::new();
    for (start, _) in text.match_indices('\u{e200}') {
        if let Some(end) = text[start..].find('\u{e201}') {
            let end = start + end + '\u{e201}'.len_utf8();
            if is_citation_marker(&text[start..end]) {
                markers.push((start, end));
            }
        }
    }
    if let Some(start) = incomplete_marker(text) {
        markers.push((start, text.len()));
    }
    markers
}

fn render_inner(
    text: &str,
    annotations: Option<&Value>,
    growing: bool,
    provisional: bool,
) -> String {
    let annotations = annotations
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    // Opaque citation tokens are not readable answer text while metadata is
    // still arriving. Remove only recognized tokens, keeping original offsets
    // for any links; completed/canonical rendering retains its existing rules.
    let mut removals = if provisional {
        preview_markers(text)
    } else {
        Vec::new()
    };
    if annotations.is_empty() {
        if removals.is_empty() {
            return text.into();
        }
        let mut result = String::with_capacity(text.len());
        let mut cursor = 0;
        for (start, end) in removals {
            result.push_str(&text[cursor..start]);
            cursor = end;
        }
        result.push_str(&text[cursor..]);
        return result;
    }
    let received = growing.then(|| text.chars().count() as u64);
    let mut at: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    let mut fallback = Vec::new();
    let mut seen = HashSet::new();
    for annotation in annotations {
        if annotation.get("type").and_then(Value::as_str) != Some("url_citation") {
            continue;
        }
        let Some(url) = annotation
            .get("url")
            .and_then(Value::as_str)
            .and_then(safe_http_url)
        else {
            continue;
        };
        let title = annotation
            .get("title")
            .and_then(Value::as_str)
            .filter(|s| !sanitize_display(s).trim().is_empty())
            .unwrap_or(&url);
        // Entity-escape rather than percent-encode query separators, keeping
        // the actual HTTP URL unchanged after Markdown parsing.
        let destination = url.replace('&', "&amp;");
        let link = format!("[{}]({destination})", escape_title(title));
        let location = range(text, annotation);
        if location.is_none()
            && let (Some(received), Some(start), Some(end)) = (
                received,
                annotation.get("start_index").and_then(Value::as_u64),
                annotation.get("end_index").and_then(Value::as_u64),
            )
            && start <= end
            && end > received
        {
            continue;
        }
        if !seen.insert((location, url)) {
            continue;
        }
        match location {
            Some((start, end)) => {
                if is_citation_marker(&text[start..end]) && !removals.contains(&(start, end)) {
                    removals.push((start, end));
                }
                at.entry(end).or_default().push(link);
            }
            None => fallback.push(link),
        }
    }
    removals.sort_unstable();
    // Overlapping removal spans are not trusted. Still attach all links, but
    // retain the original prose/token rather than risk deleting narrative.
    if removals.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        removals.clear();
    }
    let mut output = String::new();
    for (byte, character) in text
        .char_indices()
        .chain(std::iter::once((text.len(), '\0')))
    {
        if let Some(links) = at.get(&byte) {
            if !output.is_empty() && !output.ends_with(char::is_whitespace) {
                output.push(' ');
            }
            output.push_str(&links.join(" "));
        }
        if byte != text.len()
            && !removals
                .iter()
                .any(|(start, end)| byte >= *start && byte < *end)
        {
            output.push(character);
        }
    }
    if !fallback.is_empty() {
        output.push_str("\n\n");
        output.push_str(&fallback.join(" "));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn citation(start: Value, end: Value) -> Value {
        json!({"type":"url_citation","title":"Source","url":"https://example.com/a","start_index":start,"end_index":end})
    }
    #[test]
    fn unicode_prose_markers_and_repeated_claims() {
        let a = citation(json!(0), json!(2));
        let b = citation(json!(2), json!(4));
        assert_eq!(
            render("🦀é你好", Some(&json!([a, a, b]))),
            "🦀é [Source](https://example.com/a)你好 [Source](https://example.com/a)"
        );
        let text = "Fact. \u{e200}cite\u{e202}turn0search0\u{e201}";
        let a = citation(json!(6), json!(text.chars().count()));
        assert_eq!(
            render(text, Some(&json!([a]))),
            "Fact. [Source](https://example.com/a)"
        );
    }
    #[test]
    fn invalid_offsets_append_without_deleting() {
        for (start, end) in [
            (json!(-1), json!(2)),
            (json!(2), json!(1)),
            (json!(0), json!(u64::MAX)),
            (Value::Null, json!(1)),
        ] {
            assert_eq!(
                render("Text", Some(&json!([citation(start, end)]))),
                "Text\n\n[Source](https://example.com/a)"
            );
        }
    }
    #[test]
    fn previews_defer_future_ranges_and_hide_only_unfinished_markers() {
        let full = "🦀 Claim. \u{e200}cite\u{e202}turn0search0\u{e201}";
        let annotations = json!([citation(json!(9), json!(full.chars().count()))]);
        for tail in ["\u{e200}", "\u{e200}ci", "\u{e200}cite\u{e202}turn0"] {
            let partial = format!("🦀 Claim. {tail}");
            assert_eq!(
                render_preview(&partial, Some(&annotations), false),
                "🦀 Claim. "
            );
            assert_eq!(render_preview(&partial, None, false), "🦀 Claim. ");
        }
        assert_eq!(
            render_preview(full, Some(&annotations), false),
            "🦀 Claim. [Source](https://example.com/a)"
        );
        assert_eq!(
            render_preview("🦀", Some(&annotations), true),
            "🦀\n\n[Source](https://example.com/a)"
        );
        assert_eq!(
            render_preview("ordinary \u{e200} prose", None, false),
            "ordinary \u{e200} prose"
        );
        // Canonical projection is unchanged, including malformed raw tokens.
        assert_eq!(render("Claim. \u{e200}ci", None), "Claim. \u{e200}ci");
    }

    #[test]
    fn provisional_complete_markers_wait_for_metadata_without_hiding_prose() {
        let token = "\u{e200}cite\u{e202}turn0search0\u{e201}";
        let text = format!("Claim {token} and more prose {token}.");
        for complete in [false, true] {
            assert_eq!(
                render_preview(&text, None, complete),
                "Claim  and more prose ."
            );
            let annotations = json!([citation(json!(6), json!(6 + token.chars().count()))]);
            assert_eq!(
                render_preview(&text, Some(&annotations), complete),
                "Claim [Source](https://example.com/a) and more prose ."
            );
            assert_eq!(render_preview("Claim \u{e200}ci", None, complete), "Claim ");
        }
        assert_eq!(
            render(&text, None),
            text,
            "canonical raw projection is unchanged"
        );
    }

    #[test]
    fn cited_sources_are_safe_ordered_and_bounded_independently_of_offsets() {
        let annotations = json!([
            {"type":"url_citation", "url":"https://EXAMPLE.org/doc", "title":"  \u{202e}First\n title\u{1b}  ", "end_index":999},
            {"type":"url_citation", "url":"https://example.org/doc", "title":"Later title"},
            {"type":"url_citation", "url":"http://example.org/blank", "title":" \t\u{200e} "},
            {"type":"url_citation", "url":"https://rust-lang.org/", "title":"🦀".repeat(513)},
            {"type":"file_citation", "url":"https://example.org/file"},
            {"type":"url_citation", "url":"javascript:alert(1)"},
            {"type":"url_citation", "url":"https://user:pass@example.org/"},
            {"type":"url_citation", "url":"https://example.org/\u{1b}no"},
            {"type":"url_citation", "url":format!("https://example.org/{}", "x".repeat(8192))},
            {"type":"url_citation", "url":format!("https://example.org/{}", "é".repeat(1500))}
        ]);
        let pages = sources(Some(&annotations));
        assert_eq!(pages.len(), 3);
        assert_eq!(pages[0].url, "https://example.org/doc");
        assert_eq!(pages[0].title.as_deref(), Some("First title"));
        assert_eq!(pages[1].url, "http://example.org/blank");
        assert_eq!(pages[1].title, None);
        assert_eq!(pages[2].title.as_ref().unwrap().chars().count(), 512);
        assert!(sources(None).is_empty());
        assert!(sources(Some(&json!({}))).is_empty());
        let native = text_with_openai_extras("answer", json!({"annotations": annotations}));
        assert_eq!(text_sources(&native), pages);

        let many: Vec<_> = (0..70)
            .map(|i| json!({"type":"url_citation", "url":format!("https://example.org/{i}")}))
            .collect();
        let pages = sources(Some(&json!(many)));
        assert_eq!(pages.len(), 64);
        assert_eq!(pages[63].url, "https://example.org/63");
    }

    #[test]
    fn display_address_uses_the_ascii_host_and_preserves_the_path_query_and_fragment() {
        assert_eq!(
            url_display("HTTPS://WWW.Bücher.DE:8080/a%20b?x=1&y=2#answer"),
            Some(UrlDisplay {
                host: "xn--bcher-kva.de".into(),
                rest: "/a%20b?x=1&y=2#answer".into(),
            })
        );
        assert_eq!(url_display("https://example.org/").unwrap().rest, "");
        assert_eq!(url_display("http://example.org/?#").unwrap().rest, "?#");
        assert!(url_display("file:///tmp/x").is_none());
        assert!(url_display("not a URL").is_none());
    }

    #[test]
    fn metadata_is_not_executable_markdown_or_terminal_control() {
        let a = json!({"type":"url_citation","title":"[bad]*\u{001b}\n", "url":"https://example.com/a(b)","start_index":0,"end_index":4});
        assert_eq!(
            render("Text", Some(&json!([a]))),
            "Text [\\[bad\\]\\*](https://example.com/a%28b%29)"
        );
        for url in [
            "javascript:alert(1)",
            "file:///tmp/x",
            "https://x.test/\u{001b}",
            "https://user:pass@x.test",
        ] {
            assert!(safe_http_url(url).is_none());
        }
        assert_eq!(
            render(
                "Text",
                Some(&json!([{"type":"future","url":"https://x.test"}]))
            ),
            "Text"
        );
    }
}
