//! YAML syntax highlighting for document views.
//!
//! A line lexer, not a parser: sofka renders only the visible lines each
//! frame, and the only context YAML needs across lines is whether a line sits
//! inside a block scalar (`|`, `>`). [`state_at`] recovers that context
//! without allocating.

use std::collections::VecDeque;
use std::ops::Range;

use ratatui::style::Style;
use ratatui::text::Span;

use crate::theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Space,
    Key,
    Punct,
    Comment,
    Str,
    Number,
    Literal,
    Tag,
    Special,
}

/// Lexer context carried from one line to the next.
#[derive(Clone, Copy, Debug, Default)]
pub struct State {
    /// Inside a block scalar whose parent node starts at this column. Its
    /// content lines are blank or indented deeper.
    block: Option<usize>,
}

/// The lexer state before `lines[start]`.
pub fn state_at(lines: &VecDeque<String>, start: usize) -> State {
    // Block-scalar content is indented, so a non-blank line at column 0 is always outside one.
    let from = (0..start.min(lines.len()))
        .rev()
        .find(|&i| lines[i].starts_with(|c: char| c != ' '))
        .unwrap_or(0);
    let mut state = State::default();
    for line in lines.range(from..start.min(lines.len())) {
        lex(line, &mut state, &mut |_, _| {});
    }
    state
}

/// Styled spans for one line, advancing `state` past it.
pub fn highlight(line: &str, state: &mut State) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    lex(line, state, &mut |range, kind| {
        if !range.is_empty() {
            spans.push(Span::styled(line[range].to_string(), style(kind)));
        }
    });
    spans
}

/// Swatches follow catppuccin/nvim with the nvim-treesitter YAML queries, read from the active skin.
fn style(kind: Kind) -> Style {
    let fg = match kind {
        Kind::Space => return Style::default(),
        // Catppuccin uses overlay2, which the 25-swatch palette does not carry.
        Kind::Punct | Kind::Comment => return theme::dim(),
        Kind::Key => theme::lavender(),
        Kind::Str => theme::green(),
        Kind::Number | Kind::Literal => theme::peach(),
        Kind::Tag => theme::yellow(),
        Kind::Special => theme::pink(),
    };
    Style::default().fg(fg)
}

fn lex(text: &str, state: &mut State, emit: &mut impl FnMut(Range<usize>, Kind)) {
    let indent = spaces(text);
    if let Some(parent) = state.block {
        if indent == text.len() || indent > parent {
            emit(0..text.len(), Kind::Str);
            return;
        }
        state.block = None;
    }
    emit(0..indent, Kind::Space);
    let mut pos = indent;
    let rest = &text[pos..];
    if rest.starts_with('#') {
        emit(pos..text.len(), Kind::Comment);
        return;
    }
    if rest == "---" || rest.starts_with("--- ") || rest == "..." {
        emit(pos..pos + 3, Kind::Special);
        value(text, pos + 3, pos, state, emit);
        return;
    }
    // The node a block scalar on this line belongs to: the line, a `-` item, or a key.
    let mut parent = pos;
    while text[pos..] == *"-" || text[pos..].starts_with("- ") {
        parent = pos;
        emit(pos..pos + 1, Kind::Punct);
        pos += 1;
        pos = skip_spaces(text, pos, emit);
    }
    if let Some(len) = key_len(&text[pos..]) {
        parent = pos;
        emit(pos..pos + len, Kind::Key);
        emit(pos + len..pos + len + 1, Kind::Punct);
        pos = skip_spaces(text, pos + len + 1, emit);
    }
    value(text, pos, parent, state, emit);
}

fn value(
    text: &str,
    pos: usize,
    parent: usize,
    state: &mut State,
    emit: &mut impl FnMut(Range<usize>, Kind),
) {
    let v = &text[pos..];
    let end = pos + comment_start(v).unwrap_or(v.len());
    let scalar_end = pos + text[pos..end].trim_end().len();
    let mut p = skip_spaces(&text[..scalar_end], pos, emit);
    // Anchors, aliases, and tags come before the scalar.
    while p < scalar_end && matches!(text.as_bytes()[p], b'&' | b'*' | b'!') {
        let token_end = text[p..scalar_end].find(' ').map_or(scalar_end, |i| p + i);
        if text.as_bytes()[p] == b'!' {
            emit(p..token_end, Kind::Tag);
        } else {
            emit(p..p + 1, Kind::Special);
            emit(p + 1..token_end, Kind::Tag);
        }
        p = skip_spaces(&text[..scalar_end], token_end, emit);
    }
    let scalar = &text[p..scalar_end];
    if is_block_indicator(scalar) {
        emit(p..scalar_end, Kind::Punct);
        state.block = Some(parent);
    } else if scalar.starts_with(['{', '[']) {
        flow(text, p, scalar_end, emit);
    } else {
        emit(p..scalar_end, scalar_kind(scalar));
    }
    emit(scalar_end..end, Kind::Space);
    emit(end..text.len(), Kind::Comment);
}

/// A flow collection such as `[a, "b"]` or `{k: v}` on one line.
fn flow(text: &str, from: usize, to: usize, emit: &mut impl FnMut(Range<usize>, Kind)) {
    let b = text.as_bytes();
    let mut run = from;
    let mut i = from;
    while i < to {
        let c = b[i];
        if matches!(c, b'"' | b'\'')
            && let Some(n) = quoted_len(&text[i..to])
        {
            i += n;
            continue;
        }
        let punct = matches!(c, b'{' | b'}' | b'[' | b']' | b',')
            || (c == b':' && (i + 1 == to || b[i + 1] == b' '));
        if punct {
            scalar_run(text, run, i, emit);
            emit(i..i + 1, Kind::Punct);
            run = i + 1;
        }
        i += 1;
    }
    scalar_run(text, run, to, emit);
}

fn scalar_run(text: &str, from: usize, to: usize, emit: &mut impl FnMut(Range<usize>, Kind)) {
    let s = &text[from..to];
    let start = from + (s.len() - s.trim_start().len());
    let end = from + s.trim_end().len();
    if start >= end {
        emit(from..to, Kind::Space);
        return;
    }
    emit(from..start, Kind::Space);
    emit(start..end, scalar_kind(&text[start..end]));
    emit(end..to, Kind::Space);
}

fn scalar_kind(s: &str) -> Kind {
    if s.starts_with(['"', '\'']) {
        return Kind::Str;
    }
    match s {
        "null" | "Null" | "NULL" | "~" | "true" | "True" | "TRUE" | "false" | "False" | "FALSE" => {
            return Kind::Literal;
        }
        ".inf" | "-.inf" | "+.inf" | ".Inf" | "-.Inf" | "+.Inf" | ".INF" | "-.INF" | "+.INF"
        | ".nan" | ".NaN" | ".NAN" => return Kind::Number,
        _ => {}
    }
    let unsigned = s.strip_prefix(['-', '+']).unwrap_or(s);
    let radix = |prefix: &str, digit: fn(&char) -> bool| {
        unsigned
            .strip_prefix(prefix)
            .is_some_and(|d| !d.is_empty() && d.chars().all(|c| digit(&c)))
    };
    // A leading digit keeps `inf` and `NaN`, which f64 parses, out of numbers.
    let decimal = unsigned.starts_with(|c: char| c.is_ascii_digit() || c == '.')
        && unsigned.chars().any(|c| c.is_ascii_digit())
        && unsigned.parse::<f64>().is_ok();
    if decimal || radix("0x", char::is_ascii_hexdigit) || radix("0o", |c| ('0'..='7').contains(c)) {
        Kind::Number
    } else {
        Kind::Str
    }
}

/// The byte length of a mapping key at the start of `s`, excluding its colon.
fn key_len(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let ends_key = |i: usize| b.get(i) == Some(&b':') && matches!(b.get(i + 1), None | Some(b' '));
    match b.first()? {
        b'"' | b'\'' => quoted_len(s).filter(|&n| ends_key(n)),
        b'{' | b'[' | b'|' | b'>' | b'&' | b'*' | b'!' | b'#' | b'%' | b'@' | b'`' | b'?' => None,
        _ => {
            for i in 0..b.len() {
                if b[i] == b'#' && i > 0 && b[i - 1] == b' ' {
                    return None;
                }
                if ends_key(i) {
                    return Some(i);
                }
            }
            None
        }
    }
}

/// The byte length of the quoted scalar at the start of `s`, quotes included.
fn quoted_len(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let quote = *b.first()?;
    let mut i = 1;
    while i < b.len() {
        match b[i] {
            b'\\' if quote == b'"' => i += 1,
            // `''` is an escaped quote inside a single-quoted scalar.
            b'\'' if quote == b'\'' && b.get(i + 1) == Some(&b'\'') => i += 1,
            c if c == quote => return Some(i + 1),
            _ => {}
        }
        i += 1;
    }
    None
}

/// Where a trailing ` #` comment starts in a value, outside any quotes.
fn comment_start(v: &str) -> Option<usize> {
    let from = if v.starts_with(['"', '\'']) {
        quoted_len(v)?
    } else {
        0
    };
    let b = v.as_bytes();
    (from..b.len()).find(|&i| b[i] == b'#' && (i == 0 || matches!(b[i - 1], b' ' | b'\t')))
}

fn is_block_indicator(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('|' | '>')) && chars.all(|c| matches!(c, '-' | '+' | '1'..='9'))
}

fn spaces(s: &str) -> usize {
    s.len() - s.trim_start_matches(' ').len()
}

fn skip_spaces(text: &str, pos: usize, emit: &mut impl FnMut(Range<usize>, Kind)) -> usize {
    let end = pos + spaces(&text[pos..]);
    emit(pos..end, Kind::Space);
    end
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The non-space tokens of each line, as `(text, kind)`.
    fn tokens(doc: &str) -> Vec<Vec<(String, Kind)>> {
        let mut state = State::default();
        doc.lines()
            .map(|line| {
                let mut out = Vec::new();
                lex(line, &mut state, &mut |range, kind| {
                    if kind != Kind::Space && !line[range.clone()].trim().is_empty() {
                        out.push((line[range].trim().to_string(), kind));
                    }
                });
                out
            })
            .collect()
    }

    fn t(text: &str, kind: Kind) -> (String, Kind) {
        (text.to_string(), kind)
    }

    #[test]
    fn serde_yaml_output_gets_nvim_like_tokens() {
        let yaml = serde_yaml::to_string(&json!({
            "metadata": {
                "labels": {"app.kubernetes.io/name": "web"},
                "annotations": {"note": "a: b # not a comment"},
            },
            "spec": {
                "args": ["--port=8080", "123"],
                "replicas": 3,
                "paused": false,
                "image": "nginx:1.25",
                "resources": {},
                "script": "line1\nkey: value\n",
                "node": null,
            }
        }))
        .unwrap();
        let lines = tokens(&yaml);
        let find = |needle: &str| {
            lines
                .iter()
                .find(|l| l.iter().any(|(text, _)| text == needle))
                .unwrap_or_else(|| panic!("{needle} missing in:\n{yaml}"))
                .clone()
        };
        use Kind::*;
        assert_eq!(
            find("app.kubernetes.io/name"),
            [
                t("app.kubernetes.io/name", Key),
                t(":", Punct),
                t("web", Str)
            ]
        );
        assert_eq!(
            find("note"),
            [
                t("note", Key),
                t(":", Punct),
                t("'a: b # not a comment'", Str)
            ]
        );
        assert_eq!(find("--port=8080"), [t("-", Punct), t("--port=8080", Str)]);
        assert_eq!(find("'123'"), [t("-", Punct), t("'123'", Str)]);
        assert_eq!(
            find("3"),
            [t("replicas", Key), t(":", Punct), t("3", Number)]
        );
        assert_eq!(
            find("false"),
            [t("paused", Key), t(":", Punct), t("false", Literal)]
        );
        assert_eq!(find("nginx:1.25")[2], t("nginx:1.25", Str));
        assert_eq!(find("{").iter().filter(|(_, k)| *k == Punct).count(), 3);
        assert_eq!(find("null")[2], t("null", Literal));
        assert_eq!(find("script")[2].1, Punct, "block indicator");
        // Block-scalar content is a string, even when it looks like a key.
        assert_eq!(find("key: value"), [t("key: value", Str)]);
    }

    #[test]
    fn helm_manifest_styles() {
        use Kind::*;
        let doc = "\
---
# Source: web/templates/cm.yaml
data:
  config: |
    a: 1

    b: 2
  flow: [a, \"b, c\", 3]
  ref: &base {k: v}
  copy: *base
  bin: !!binary aGk=
  port: 8080 # the port
\"quoted key\": 0x1F
- - nested
";
        let l = tokens(doc);
        assert_eq!(l[0], [t("---", Special)]);
        assert_eq!(l[1], [t("# Source: web/templates/cm.yaml", Comment)]);
        assert_eq!(l[3][2], t("|", Punct));
        assert_eq!(l[4], [t("a: 1", Str)]);
        assert!(l[5].is_empty(), "a blank line stays in the scalar");
        assert_eq!(l[6], [t("b: 2", Str)]);
        assert_eq!(
            l[7],
            [
                t("flow", Key),
                t(":", Punct),
                t("[", Punct),
                t("a", Str),
                t(",", Punct),
                t("\"b, c\"", Str),
                t(",", Punct),
                t("3", Number),
                t("]", Punct),
            ]
        );
        assert_eq!(&l[8][2..4], [t("&", Special), t("base", Tag)]);
        assert_eq!(&l[9][2..], [t("*", Special), t("base", Tag)]);
        assert_eq!(&l[10][2..], [t("!!binary", Tag), t("aGk=", Str)]);
        assert_eq!(
            l[11],
            [
                t("port", Key),
                t(":", Punct),
                t("8080", Number),
                t("# the port", Comment)
            ]
        );
        assert_eq!(
            l[12],
            [t("\"quoted key\"", Key), t(":", Punct), t("0x1F", Number)]
        );
        assert_eq!(l[13], [t("-", Punct), t("-", Punct), t("nested", Str)]);
    }

    #[test]
    fn block_scalar_under_a_list_item_key() {
        let l = tokens("- name: |-\n    text: here\n  image: x\n");
        assert_eq!(l[1], [t("text: here", Kind::Str)]);
        assert_eq!(l[2][0], t("image", Kind::Key));
    }

    #[test]
    fn state_at_recovers_block_scalar_context_mid_document() {
        let lines: VecDeque<String> = "spec:\n  script: |\n    a: 1\n    b: 2\n  after: x\n"
            .lines()
            .map(String::from)
            .collect();
        let mut state = state_at(&lines, 3);
        let spans = highlight(&lines[3], &mut state);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].style.fg, Some(theme::green()));
        let spans = highlight(&lines[4], &mut state);
        assert_eq!(spans[1].content, "after");
        assert_eq!(spans[1].style.fg, Some(theme::lavender()));
    }

    #[test]
    fn numbers_exclude_words_that_f64_accepts() {
        for s in ["inf", "NaN", "1.2.3", "1_000", "-", "."] {
            assert_eq!(scalar_kind(s), Kind::Str, "{s}");
        }
        for s in ["0", "-3", "1.5", "1e3", ".5", "0o17", ".inf"] {
            assert_eq!(scalar_kind(s), Kind::Number, "{s}");
        }
    }
}
