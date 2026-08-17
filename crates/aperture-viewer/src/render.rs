//! HTML, written by hand and self-contained.
//!
//! No template engine and no assets: the pages are four shapes, the CSS is thirty
//! lines, and a viewer that needed a build step to demonstrate a database would be
//! demonstrating the build step. Glean's own hyperlink demo is ~550 lines of Haskell
//! for the same reason.

use std::fmt::Write as _;

/// Escape text for HTML **element content**.
///
/// Every string that reaches a page goes through this: a path, an identifier, a line
/// of somebody's source. Source in particular is the case that matters — it is full
/// of `<`, `>` and `&`, and a viewer that rendered it raw would be a viewer that
/// executed it.
#[must_use]
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Escape text for a **URL path segment**.
///
/// A separate function from [`escape`], because the two contexts have different
/// dangerous characters and one function for both is how an escaping bug is written.
#[must_use]
pub fn url(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char);
            }
            other => {
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}

const STYLE: &str = "\
:root { color-scheme: light dark; --rule: #d8dcda; --muted: #5a6470; --link: #2b4162; }
@media (prefers-color-scheme: dark) {
  :root { --rule: #262d36; --muted: #9ba5af; --link: #93b4da; }
}
* { box-sizing: border-box; }
body { margin: 0; font: 14px/1.5 system-ui, sans-serif; }
header { display: flex; gap: 16px; align-items: baseline; padding: 10px 16px;
         border-bottom: 1px solid var(--rule); flex-wrap: wrap; }
header a { font-weight: 600; text-decoration: none; color: inherit; }
header form { margin-left: auto; display: flex; gap: 6px; }
input[type=text] { font: inherit; padding: 4px 8px; border: 1px solid var(--rule);
                   border-radius: 3px; background: transparent; color: inherit; min-width: 22ch; }
button { font: inherit; padding: 4px 10px; border: 1px solid var(--rule);
         border-radius: 3px; background: transparent; color: inherit; cursor: pointer; }
main { padding: 12px 16px 60px; }
h1 { font-size: 15px; font-weight: 600; margin: 0 0 12px; font-family: ui-monospace, monospace; }
a { color: var(--link); }
.muted { color: var(--muted); }
ul.list { list-style: none; padding: 0; margin: 0; font-family: ui-monospace, monospace; }
ul.list li { padding: 2px 0; }
table.rows { border-collapse: collapse; font-family: ui-monospace, monospace; font-size: 13px; }
table.rows td { padding: 2px 14px 2px 0; vertical-align: top; }
table.rows td.n { text-align: right; color: var(--muted); }
pre.src { font: 12.5px/1.45 ui-monospace, monospace; margin: 0; overflow-x: auto; }
pre.src code { display: block; }
pre.src .ln { display: inline-block; width: 6ch; text-align: right; padding-right: 2ch;
              color: var(--muted); user-select: none; }
pre.src a { text-decoration: none; border-bottom: 1px dotted currentColor; }
pre.src code:target { background: rgba(128, 160, 255, .18); }
.kind { color: var(--muted); font-size: 12px; }
.stat { color: var(--muted); font-size: 12px; margin-bottom: 10px; }
";

/// A whole page: the chrome, the search box, and `body` inside it.
#[must_use]
pub fn page(title: &str, term: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{title}</title><style>{STYLE}</style></head><body>\
         <header><a href=\"/\">aperture</a>\
         <span class=\"muted\">code index</span>\
         <form action=\"/search\" method=\"get\">\
         <input type=\"text\" name=\"q\" value=\"{term}\" placeholder=\"symbol\" autofocus>\
         <button type=\"submit\">search</button></form></header>\
         <main>{body}</main></body></html>",
        title = escape(title),
        term = escape(term),
    )
}

/// A file's source, with a link spliced over every cross-reference.
///
/// `lines` is `(number, text)` in order; `links` is `(line, col, length, href, title)`
/// with **1-based** columns counted in characters, which is what the indexers emit.
///
/// Links on one line are applied left to right and non-overlapping: a second link
/// starting inside the first is dropped rather than nested, since a nested `<a>` is
/// not something a browser will render as two links anyway.
#[must_use]
pub fn source(lines: &[(i64, String)], links: &[(i64, i64, i64, String, String)]) -> String {
    let mut out = String::with_capacity(lines.iter().map(|(_, t)| t.len() + 64).sum());
    out.push_str("<pre class=\"src\">");

    let mut at = 0usize;

    for (number, text) in lines {
        // The links are sorted by (line, col) because `src.FileXRef` is keyed that
        // way, so this walks them in step with the lines rather than searching.
        let start = at;
        while at < links.len() && links[at].0 == *number {
            at += 1;
        }
        let on_this_line = &links[start..at];

        let _ = write!(
            out,
            "<code id=\"L{number}\"><span class=\"ln\">{number}</span>"
        );

        let chars: Vec<char> = text.chars().collect();
        let mut cursor = 0usize;

        for (_, col, length, href, title) in on_this_line {
            // 1-based, and a zero length means the indexer could not measure the
            // extent — a reference wrapping a line, say. Both are skipped rather
            // than guessed at.
            let from = (*col - 1).max(0) as usize;
            let to = from.saturating_add((*length).max(0) as usize);

            if *length <= 0 || from < cursor || to > chars.len() {
                continue;
            }

            out.push_str(&escape(&chars[cursor..from].iter().collect::<String>()));
            let _ = write!(
                out,
                "<a href=\"{}\" title=\"{}\">{}</a>",
                escape(href),
                escape(title),
                escape(&chars[from..to].iter().collect::<String>())
            );
            cursor = to;
        }

        out.push_str(&escape(&chars[cursor..].iter().collect::<String>()));
        out.push_str("</code>");
    }

    out.push_str("</pre>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_is_escaped_before_it_is_rendered() {
        let lines = vec![(1, "a < b && c".to_owned())];
        let html = source(&lines, &[]);

        assert!(html.contains("a &lt; b &amp;&amp; c"), "{html}");
        assert!(!html.contains("a < b"), "{html}");
    }

    /// **A link covers the identifier and nothing else**, and the text either side
    /// of it survives.
    #[test]
    fn a_link_covers_exactly_its_span() {
        let lines = vec![(1, "foo(bar)".to_owned())];
        // `bar` starts at 1-based column 5 and is three characters long.
        let links = vec![(1, 5, 3, "/file/x#L2".to_owned(), "bar".to_owned())];

        let html = source(&lines, &links);

        assert!(
            html.contains(r#"foo(<a href="/file/x#L2" title="bar">bar</a>)"#),
            "{html}"
        );
    }

    /// A link whose span runs past the line is dropped rather than panicking.
    ///
    /// The indexer measures a span against the source it parsed, and a viewer reads
    /// the source out of the database — two copies, which a re-index can put out of
    /// step. Slicing past the end is the way that would show.
    #[test]
    fn a_link_past_the_end_of_the_line_is_dropped() {
        let lines = vec![(1, "ab".to_owned())];
        let links = vec![(1, 1, 99, "/x".to_owned(), "x".to_owned())];

        let html = source(&lines, &links);
        assert!(html.contains("ab"), "{html}");
        assert!(!html.contains("<a "), "{html}");
    }

    /// Overlapping links do not nest.
    #[test]
    fn a_second_link_inside_the_first_is_dropped() {
        let lines = vec![(1, "abcdef".to_owned())];
        let links = vec![
            (1, 1, 4, "/one".to_owned(), "one".to_owned()),
            (1, 2, 2, "/two".to_owned(), "two".to_owned()),
        ];

        let html = source(&lines, &links);
        assert!(html.contains("/one"), "{html}");
        assert!(!html.contains("/two"), "{html}");
    }

    /// A line with a multi-byte character is sliced by **character**, not by byte.
    #[test]
    fn columns_count_characters() {
        let lines = vec![(1, "é foo".to_owned())];
        // `foo` is at 1-based character column 3.
        let links = vec![(1, 3, 3, "/x".to_owned(), "foo".to_owned())];

        let html = source(&lines, &links);
        assert!(
            html.contains(r#"<a href="/x" title="foo">foo</a>"#),
            "{html}"
        );
    }

    #[test]
    fn a_url_segment_escapes_what_a_path_can_contain() {
        assert_eq!(url("src/a b.cs"), "src/a%20b.cs");
        assert_eq!(url("a#b?c"), "a%23b%3Fc");
        assert_eq!(url("plain/path.cs"), "plain/path.cs");
    }
}
