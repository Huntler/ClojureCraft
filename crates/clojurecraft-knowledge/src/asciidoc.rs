//! Conversion of the AsciiDoc pages of clojure.org into plain-text sections.
//!
//! Only the subset used by the documentation pages is understood: section headings, paragraphs,
//! lists, admonitions, listing and literal blocks, example/sidebar/quote blocks, tables, inline
//! links and cross references. Unknown constructs degrade to their text. Pass-through HTML and
//! comments are dropped, so no markup from a page reaches the index as anything but text.

/// A block of a section, in document order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    /// Prose: a paragraph, list item, admonition, or table row group.
    Text(String),
    /// Source code or literal text, kept verbatim.
    Code(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    /// Headings from the page's first-level sections down to this one; empty for the preamble.
    pub headings: Vec<String>,
    /// Fragment identifier of the section on the published page.
    pub anchor: Option<String>,
    pub blocks: Vec<Block>,
    /// API symbols the section links to, as `namespace/name`.
    pub mentions: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub title: String,
    pub sections: Vec<Section>,
}

/// The identifier Asciidoctor derives from a heading (`_the_seq_library`).
pub fn auto_anchor(heading: &str) -> String {
    let mut out = String::from("_");
    for c in heading.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    while out.len() > 1 && out.ends_with('_') {
        out.pop();
    }
    out
}

fn heading_level(line: &str) -> Option<(usize, &str)> {
    let level = line.bytes().take_while(|b| *b == b'=').count();
    let rest = &line[level..];
    ((1..=6).contains(&level) && rest.starts_with(' ')).then(|| (level, rest.trim()))
}

fn is_delimiter(line: &str, ch: char) -> bool {
    line.len() >= 4 && line.chars().all(|c| c == ch)
}

/// Extracts `namespace/name` from a link to the API documentation.
fn api_mention(url: &str) -> Option<String> {
    let (_, fragment) = url.split_once("-api.html#")?;
    let fragment = fragment.trim();
    (fragment.contains('/') && !fragment.contains(' ')).then(|| fragment.to_owned())
}

/// Converts inline markup to text, collecting linked API symbols.
pub fn inline(text: &str, mentions: &mut Vec<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        // Cross references: <<target#anchor,label>> or <<target>>.
        if let Some(after) = rest.strip_prefix("<<")
            && let Some(end) = after.find(">>")
        {
            let inner = &after[..end];
            let label = inner.split_once(',').map_or_else(
                || inner.trim_end_matches('#').replace(['#', '_'], " "),
                |(_, l)| l.trim().to_owned(),
            );
            out.push_str(&label);
            rest = &after[end + 2..];
            continue;
        }
        // URLs and link/xref macros followed by [label].
        let macro_len = ["https://", "http://", "link:", "xref:", "mailto:"]
            .iter()
            .find(|p| rest.starts_with(**p))
            .map(|_| ());
        if macro_len.is_some() {
            let target_end = rest
                .find(|c: char| c.is_whitespace() || c == '[')
                .unwrap_or(rest.len());
            let target = &rest[..target_end];
            let after = &rest[target_end..];
            if let Some(m) = api_mention(target)
                && !mentions.contains(&m)
            {
                mentions.push(m);
            }
            if let Some(label_part) = after.strip_prefix('[')
                && let Some(close) = label_part.find(']')
            {
                let label = label_part[..close].split(',').next().unwrap_or("").trim();
                let label = label.trim_matches('"').trim_end_matches('^');
                if label.is_empty() {
                    out.push_str(target.trim_start_matches("link:").trim_start_matches("xref:"));
                } else {
                    out.push_str(label);
                }
                rest = &label_part[close + 1..];
            } else {
                out.push_str(target);
                rest = after;
            }
            continue;
        }
        if let Some(after) = rest.strip_prefix("footnote:[")
            && let Some(close) = after.find(']')
        {
            out.push_str(" (");
            out.push_str(&after[..close]);
            out.push(')');
            rest = &after[close + 1..];
            continue;
        }
        if let Some(after) = rest.strip_prefix("pass:[")
            && let Some(close) = after.find(']')
        {
            out.push_str(&after[..close]);
            rest = &after[close + 1..];
            continue;
        }
        if let Some(after) = rest.strip_prefix("+++")
            && let Some(close) = after.find("+++")
        {
            out.push_str(&after[..close]);
            rest = &after[close + 3..];
            continue;
        }
        if let Some(after) = rest.strip_prefix('{')
            && let Some(close) = after.find('}')
            && after[..close]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            match &after[..close] {
                "nbsp" | "sp" | "blank" | "zwsp" => out.push(' '),
                "plus" => out.push('+'),
                "star" | "asterisk" => out.push('*'),
                "backtick" => out.push('`'),
                other => {
                    out.push('{');
                    out.push_str(other);
                    out.push('}');
                }
            }
            rest = &after[close + 1..];
            continue;
        }
        let mut chars = rest.chars();
        let c = chars.next().unwrap_or_default();
        // Monospace markers carry no meaning in plain text.
        if c != '`' {
            out.push(c);
        }
        rest = chars.as_str();
    }
    out
}

struct Builder {
    title: String,
    anchors: std::collections::HashSet<String>,
    sections: Vec<Section>,
    current: Section,
    paragraph: Vec<String>,
    hardbreaks: bool,
}

impl Builder {
    fn flush_paragraph(&mut self) {
        if self.paragraph.is_empty() {
            return;
        }
        let joiner = if self.hardbreaks { "\n" } else { " " };
        let raw = self.paragraph.join(joiner);
        self.paragraph.clear();
        self.hardbreaks = false;
        let text = inline(&raw, &mut self.current.mentions);
        let text = text.trim();
        if !text.is_empty() {
            self.current.blocks.push(Block::Text(text.to_owned()));
        }
    }

    fn start_section(&mut self, level: usize, heading: &str, anchor: Option<String>) {
        self.flush_paragraph();
        let mut headings = self.current.headings.clone();
        // Level 2 (`==`) is the first section level below the page title.
        headings.truncate(level.saturating_sub(2));
        let mut ignored = Vec::new();
        headings.push(inline(heading, &mut ignored).trim().to_owned());
        // Like Asciidoctor, repeated generated identifiers get a numeric suffix.
        let anchor = anchor.unwrap_or_else(|| {
            let base = auto_anchor(heading);
            let mut candidate = base.clone();
            let mut n = 2;
            while self.anchors.contains(&candidate) {
                candidate = format!("{base}_{n}");
                n += 1;
            }
            candidate
        });
        self.anchors.insert(anchor.clone());
        let previous = std::mem::replace(
            &mut self.current,
            Section {
                anchor: Some(anchor),
                headings,
                blocks: Vec::new(),
                mentions: ignored,
            },
        );
        if !previous.blocks.is_empty() {
            self.sections.push(previous);
        }
    }
}

/// Parses an AsciiDoc page.
pub fn parse(source: &str) -> Page {
    let mut b = Builder {
        title: String::new(),
        anchors: std::collections::HashSet::new(),
        sections: Vec::new(),
        current: Section {
            headings: Vec::new(),
            anchor: None,
            blocks: Vec::new(),
            mentions: Vec::new(),
        },
        paragraph: Vec::new(),
        hardbreaks: false,
    };
    let lines: Vec<&str> = source.lines().collect();
    let mut i = 0;
    let mut pending_anchor: Option<String> = None;
    let mut in_header = false;
    while i < lines.len() {
        let line = lines[i].trim_end();
        i += 1;
        if b.title.is_empty()
            && let Some(t) = line.strip_prefix("= ")
        {
            b.title = inline(t, &mut Vec::new()).trim().to_owned();
            in_header = true;
            continue;
        }
        if in_header {
            // Author, date, and attribute lines end at the first blank line.
            if line.is_empty() {
                in_header = false;
            }
            continue;
        }
        if line.is_empty() {
            b.flush_paragraph();
            continue;
        }
        // Comment blocks and pass-through blocks are dropped with their contents.
        if is_delimiter(line, '/') || is_delimiter(line, '+') {
            b.flush_paragraph();
            let delim = line.to_owned();
            while i < lines.len() && lines[i].trim_end() != delim {
                i += 1;
            }
            i += 1;
            continue;
        }
        if line.starts_with("//") {
            continue;
        }
        if is_delimiter(line, '-') || is_delimiter(line, '.') {
            b.flush_paragraph();
            let delim = line.to_owned();
            let mut code = Vec::new();
            while i < lines.len() && lines[i].trim_end() != delim {
                code.push(strip_callouts(lines[i]));
                i += 1;
            }
            i += 1;
            let code = code.join("\n");
            let code = code.trim_matches('\n');
            if !code.trim().is_empty() {
                b.current.blocks.push(Block::Code(code.to_owned()));
            }
            continue;
        }
        if line.starts_with("|===") {
            b.flush_paragraph();
            let mut rows: Vec<String> = Vec::new();
            let mut row: Vec<String> = Vec::new();
            while i < lines.len() && !lines[i].trim_end().starts_with("|===") {
                let l = lines[i].trim();
                i += 1;
                if l.is_empty() {
                    if !row.is_empty() {
                        rows.push(row.join(" | "));
                        row.clear();
                    }
                    continue;
                }
                for cell in l.split('|').map(str::trim).filter(|c| !c.is_empty()) {
                    row.push(inline(cell, &mut b.current.mentions));
                }
            }
            i += 1;
            if !row.is_empty() {
                rows.push(row.join(" | "));
            }
            if !rows.is_empty() {
                b.current.blocks.push(Block::Text(rows.join("\n")));
            }
            continue;
        }
        // Example, sidebar, quote, and open block delimiters: their contents are ordinary text.
        if is_delimiter(line, '=') || is_delimiter(line, '*') || is_delimiter(line, '_') || line == "--" {
            b.flush_paragraph();
            continue;
        }
        if let Some((level, heading)) = heading_level(line) {
            if level >= 2 {
                b.start_section(level, heading, pending_anchor.take());
            }
            continue;
        }
        if let Some(anchor) = line.strip_prefix("[[").and_then(|l| l.strip_suffix("]]")) {
            b.flush_paragraph();
            pending_anchor = Some(anchor.split(',').next().unwrap_or(anchor).to_owned());
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') && !line.contains(' ') || line.starts_with("[source")
        {
            b.flush_paragraph();
            if let Some(id) = line.strip_prefix("[#").and_then(|l| l.strip_suffix(']')) {
                pending_anchor = Some(id.split(['.', '%', ',']).next().unwrap_or(id).to_owned());
            }
            if line.contains("%hardbreaks") {
                b.hardbreaks = true;
            }
            continue;
        }
        if is_attribute_entry(line) || line.ends_with("::[]") || is_directive(line) {
            continue;
        }
        if line == "+" {
            // List continuation: the next block belongs to the same item.
            b.flush_paragraph();
            continue;
        }
        // Block titles (".Title") become text of their own.
        if let Some(t) = line.strip_prefix('.')
            && !t.starts_with(' ')
            && !t.starts_with('.')
            && !t.is_empty()
        {
            b.flush_paragraph();
            b.paragraph.push(t.to_owned());
            b.flush_paragraph();
            continue;
        }
        if let Some(item) = list_item(line) {
            b.flush_paragraph();
            b.paragraph.push(format!("- {item}"));
            continue;
        }
        for (label, word) in [
            ("NOTE: ", "Note: "),
            ("TIP: ", "Tip: "),
            ("WARNING: ", "Warning: "),
            ("IMPORTANT: ", "Important: "),
            ("CAUTION: ", "Caution: "),
        ] {
            if let Some(rest) = line.strip_prefix(label) {
                b.flush_paragraph();
                b.paragraph.push(format!("{word}{rest}"));
                break;
            }
        }
        if !line.starts_with("NOTE: ")
            && !line.starts_with("TIP: ")
            && !line.starts_with("WARNING: ")
            && !line.starts_with("IMPORTANT: ")
            && !line.starts_with("CAUTION: ")
        {
            b.paragraph.push(line.trim_start().to_owned());
        }
    }
    b.flush_paragraph();
    let last = b.current;
    if !last.blocks.is_empty() {
        b.sections.push(last);
    }
    Page {
        title: b.title,
        sections: b.sections,
    }
}

/// `:name: value` or `:name!:` document attribute entries.
fn is_attribute_entry(line: &str) -> bool {
    let Some(rest) = line.strip_prefix(':') else {
        return false;
    };
    let Some(end) = rest.find(':') else {
        return false;
    };
    let name = rest[..end].trim_end_matches('!');
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        && (rest.len() == end + 1 || rest[end + 1..].starts_with(' '))
}

fn is_directive(line: &str) -> bool {
    [
        "ifdef::",
        "ifndef::",
        "endif::",
        "ifeval::",
        "include::",
        "image::",
        "toc::",
        "video::",
    ]
    .iter()
    .any(|d| line.starts_with(d))
}

fn list_item(line: &str) -> Option<&str> {
    let marker_len = line
        .bytes()
        .take_while(|b| *b == b'*' || *b == b'.' || *b == b'-')
        .count();
    if marker_len == 0 || marker_len > 5 {
        return None;
    }
    let rest = &line[marker_len..];
    let marker = &line[..marker_len];
    let uniform = marker.bytes().all(|b| b == marker.as_bytes()[0]);
    (uniform && rest.starts_with(' ') && !(marker == "-" && marker_len > 1)).then(|| rest.trim())
}

/// Removes callout markers (`<1>`) from the end of a code line.
fn strip_callouts(line: &str) -> String {
    let mut s = line.trim_end();
    loop {
        let trimmed = s.trim_end();
        if let Some(open) = trimmed.rfind("<")
            && trimmed.ends_with('>')
            && trimmed[open + 1..trimmed.len() - 1]
                .bytes()
                .all(|b| b.is_ascii_digit() || b == b'.' || b == b'!')
            && trimmed.len() - open > 2
        {
            s = &trimmed[..open];
            continue;
        }
        return trimmed.to_owned();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "= Sequences\nRich Hickey\n2015-01-01\n:type: reference\n:toc: macro\n\nifdef::env-github[:outfilesuffix: .adoc]\n\ntoc::[]\n\nA seq is a logical list. The https://clojure.github.io/clojure/clojure.core-api.html#clojure.core/seq[seq] function yields one. See <<lazy#,lazy>>.\n\n== The Seq library\n\n=== Seq in, Seq out\n\n[%hardbreaks]\nShorter seq: https://clojure.github.io/clojure/clojure.core-api.html#clojure.core/filter[filter]\nLonger seq: `concat`\n\n[source,clojure]\n----\n(filter even? [1 2 3]) <1>\n;; => (2)\n----\n<1> keeps even numbers\n\n////\nhidden comment\n////\n\n++++\n<div>html</div>\n++++\n\n[[custom-id]]\n== Tables\n\n|===\n|Name |Meaning\n\n|a |first\n|===\n\nNOTE: Notes stay.\n";

    #[test]
    fn sections_keep_headings_code_and_links() {
        let page = parse(SAMPLE);
        assert_eq!(page.title, "Sequences");
        assert_eq!(page.sections.len(), 3, "{page:#?}");
        let pre = &page.sections[0];
        assert!(pre.headings.is_empty());
        assert_eq!(
            pre.blocks,
            vec![Block::Text(
                "A seq is a logical list. The seq function yields one. See lazy.".into()
            )]
        );
        assert_eq!(pre.mentions, vec!["clojure.core/seq"]);

        let seq = &page.sections[1];
        assert_eq!(seq.headings, vec!["The Seq library", "Seq in, Seq out"]);
        assert_eq!(seq.anchor.as_deref(), Some("_seq_in_seq_out"));
        assert_eq!(
            seq.blocks[0],
            Block::Text("Shorter seq: filter\nLonger seq: concat".into())
        );
        assert_eq!(
            seq.blocks[1],
            Block::Code("(filter even? [1 2 3])\n;; => (2)".into())
        );
        assert_eq!(seq.mentions, vec!["clojure.core/filter"]);
        let all: String = page
            .sections
            .iter()
            .flat_map(|s| &s.blocks)
            .map(|b| format!("{b:?}"))
            .collect();
        assert!(!all.contains("hidden comment") && !all.contains("<div>"), "{all}");

        let table = &page.sections[2];
        assert_eq!(table.anchor.as_deref(), Some("custom-id"));
        assert_eq!(table.blocks[0], Block::Text("Name | Meaning\na | first".into()));
        assert_eq!(table.blocks[1], Block::Text("Note: Notes stay.".into()));
    }

    #[test]
    fn anchors_follow_asciidoctor() {
        assert_eq!(auto_anchor("The Seq interface"), "_the_seq_interface");
        assert_eq!(auto_anchor("Seq in, Seq out"), "_seq_in_seq_out");
    }
}
