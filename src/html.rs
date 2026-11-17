//! Finding `<link data-trug ...>` directives in an index.html and replacing them in place,
//! leaving every other byte of the document as it was.

use std::ops::Range;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Directive {
    /// The whole tag, `<` to `>`.
    pub range: Range<usize>,
    pub attrs: Vec<(String, String)>,
}

impl Directive {
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

pub fn directives(html: &str) -> Vec<Directive> {
    let bytes = html.as_bytes();
    let mut found = Vec::new();
    let mut i = 0;
    while let Some(offset) = html[i..].find('<') {
        let start = i + offset;
        let rest = &html[start..];
        if rest.starts_with("<!--") {
            i = rest.find("-->").map_or(html.len(), |end| start + end + 3);
            continue;
        }
        let name_len = rest[1..]
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric())
            .count();
        let name = rest[1..1 + name_len].to_ascii_lowercase();
        if name == "script" || name == "style" {
            let close = format!("</{name}");
            i = match find_ci(html, start, &close) {
                Some(at) => at + close.len(),
                None => html.len(),
            };
            continue;
        }
        let Some(end) = tag_end(bytes, start) else {
            break;
        };
        if name == "link" {
            let attrs = attributes(&html[start + 5..end]);
            if attrs.iter().any(|(key, _)| key == "data-trug") {
                found.push(Directive {
                    range: start..end + 1,
                    attrs,
                });
            }
        }
        i = end + 1;
    }
    found
}

/// The `<base data-trug-public-url>` tag, if the document has one.
pub fn public_url_base(html: &str) -> Option<Directive> {
    let bytes = html.as_bytes();
    let mut i = 0;
    while let Some(offset) = html[i..].find('<') {
        let start = i + offset;
        let rest = &html[start..];
        if rest.starts_with("<!--") {
            i = rest.find("-->").map_or(html.len(), |end| start + end + 3);
            continue;
        }
        let name_len = rest[1..]
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric())
            .count();
        let name = rest[1..1 + name_len].to_ascii_lowercase();
        if name == "script" || name == "style" {
            let close = format!("</{name}");
            i = find_ci(html, start, &close).map_or(html.len(), |at| at + close.len());
            continue;
        }
        let end = tag_end(bytes, start)?;
        if name == "base" {
            let attrs = attributes(&html[start + 5..end]);
            if attrs.iter().any(|(key, _)| key == "data-trug-public-url") {
                return Some(Directive {
                    range: start..end + 1,
                    attrs,
                });
            }
        }
        i = end + 1;
    }
    None
}

/// `<name attrs...>` with the named attributes dropped and `set` added or replacing its value.
pub fn tag(name: &str, attrs: &[(String, String)], drop: &[&str], set: (&str, &str)) -> String {
    let mut out = format!("<{name}");
    let mut done = false;
    for (key, value) in attrs {
        if drop.contains(&key.as_str()) {
            continue;
        }
        if key == set.0 {
            done = true;
            push_attr(&mut out, key, set.1);
        } else {
            push_attr(&mut out, key, value);
        }
    }
    if !done {
        push_attr(&mut out, set.0, set.1);
    }
    out.push('>');
    out
}

fn push_attr(out: &mut String, key: &str, value: &str) {
    out.push_str(&format!(" {key}=\"{}\"", value.replace('"', "&quot;")));
}

fn find_ci(html: &str, from: usize, needle: &str) -> Option<usize> {
    let hay = html.as_bytes();
    (from..hay.len().saturating_sub(needle.len() - 1))
        .find(|&at| hay[at..at + needle.len()].eq_ignore_ascii_case(needle.as_bytes()))
}

fn tag_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut quote = None;
    for (at, &b) in bytes.iter().enumerate().skip(start) {
        match (quote, b) {
            (None, b'"' | b'\'') => quote = Some(b),
            (Some(q), b) if b == q => quote = None,
            (None, b'>') => return Some(at),
            _ => {}
        }
    }
    None
}

fn attributes(text: &str) -> Vec<(String, String)> {
    let bytes = text.as_bytes();
    let mut attrs = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            i += 1;
        }
        let key_start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'='
            && bytes[i] != b'/'
        {
            i += 1;
        }
        if key_start == i {
            break;
        }
        let key = text[key_start..i].to_ascii_lowercase();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let q = bytes[i];
                let from = i + 1;
                i = from;
                while i < bytes.len() && bytes[i] != q {
                    i += 1;
                }
                value = text[from..i].to_owned();
                i += 1;
            } else {
                let from = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                value = text[from..i].to_owned();
            }
        }
        attrs.push((key, value));
    }
    attrs
}

/// Replaces each range with its text; ranges must not overlap.
pub fn replace(html: &str, mut edits: Vec<(Range<usize>, String)>) -> String {
    edits.sort_by_key(|(range, _)| range.start);
    let mut out = String::with_capacity(html.len());
    let mut at = 0;
    for (range, text) in edits {
        out.push_str(&html[at..range.start]);
        out.push_str(&text);
        at = range.end;
    }
    out.push_str(&html[at..]);
    out
}

/// Where to insert head content when there is no directive to take the place of: before `</head>`.
pub fn head_end(html: &str) -> usize {
    find_ci(html, 0, "</head").unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_only_data_trug_links() {
        let html = r#"<head><link rel="stylesheet" href="a.css"><link data-trug rel="rust" href="Cargo.toml"/>
<LINK rel='icon' HREF=x.png DATA-TRUG data-target-path="s/i"></head>"#;
        let found = directives(html);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].attr("rel"), Some("rust"));
        assert_eq!(found[0].attr("href"), Some("Cargo.toml"));
        assert_eq!(found[1].attr("rel"), Some("icon"));
        assert_eq!(found[1].attr("href"), Some("x.png"));
        assert_eq!(found[1].attr("data-target-path"), Some("s/i"));
        assert_eq!(
            &html[found[0].range.clone()],
            r#"<link data-trug rel="rust" href="Cargo.toml"/>"#
        );
    }

    #[test]
    fn finds_base_and_rebuilds_tags() {
        let html = r#"<!-- <base data-trug-public-url> --><base data-trug-public-url href="/x/" target=_top>"#;
        let base = public_url_base(html).unwrap();
        assert_eq!(base.attr("href"), Some("/x/"));
        assert_eq!(
            tag(
                "base",
                &base.attrs,
                &["data-trug-public-url"],
                ("href", "/y/")
            ),
            r#"<base href="/y/" target="_top">"#
        );
        assert!(public_url_base("<base href=/>").is_none());
    }

    #[test]
    fn skips_comments_and_scripts() {
        let html = r#"<!-- <link data-trug rel="rust"> --><script>var a = "<link data-trug>";</script><link data-trug rel="rust">"#;
        assert_eq!(directives(html).len(), 1);
    }

    #[test]
    fn replace_keeps_everything_else() {
        let html = "a\r\n <link data-trug rel=\"x\"> \tb";
        let found = directives(html);
        let out = replace(html, vec![(found[0].range.clone(), "Z".into())]);
        assert_eq!(out, "a\r\n Z \tb");
    }

    #[test]
    fn quoted_gt_does_not_end_a_tag() {
        let html = r#"<link data-trug rel="x" title="a>b">"#;
        assert_eq!(directives(html)[0].attr("title"), Some("a>b"));
    }
}
