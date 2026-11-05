//! Indented Sass (`.sass`): read by turning it into SCSS and reading that.
//!
//! The grammar the index has is the SCSS one, and every question the index asks (what a rule defines, what a
//! declaration uses) is the same in both syntaxes. [`to_scss`] therefore only has to put back what the
//! indentation said: braces, semicolons and the long spelling of `=mixin` and `+include`. It never moves a
//! line, so a definition keeps the line it has in the file the user opens.
//!
//! Where the text is not valid Sass the result is still bounded and still the same number of lines: an
//! unclosed `(` ends after [`MAX_PAREN_LINES`] lines, nesting stops at [`MAX_NESTING`] levels (what is deeper is
//! read as a sibling), and every scan is a single pass over the line it reads.

use tree_sitter::Parser;

use super::{Extract, scss, shorten};

/// Lines an open `(` may reach over. Sass allows a map or an argument list over many lines; a stray `(` must not
/// swallow the rest of the file.
const MAX_PAREN_LINES: usize = 64;

/// Levels of nesting that are opened with a brace; deeper rules are written next to their parent.
const MAX_NESTING: usize = 256;

/// The at-rules that hold a block. An at-rule that is not one of these (`@include`, `@extend`, `@use`) ends with a
/// semicolon when nothing is indented under it.
const BLOCK_AT_RULES: &[&str] = &[
    "if",
    "else",
    "each",
    "for",
    "while",
    "mixin",
    "function",
    "media",
    "supports",
    "at-root",
    "container",
    "font-face",
    "page",
    "document",
    "scope",
    "starting-style",
];

/// Reads a `.sass` file.
pub(super) fn extract(parser: &mut Parser, path: &str, src: &str, out: &mut Extract) {
    scss::extract(parser, path, &to_scss(src), out);
    show_as_written(out, Spelling::Sass);
}

/// How a definition is spelled where the user wrote it, once it was read as SCSS.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Spelling {
    /// `=box($p)` for `@mixin box($p)`, and no `;`.
    Sass,
    /// `@gap` for `$gap`.
    Less,
}

/// Gives the definitions that show their first line (all but the selectors, which show their name) the spelling
/// of the file they are in: the SCSS read has a `;`, `@mixin` and `$` the file does not.
///
/// The signature is the statement that was parsed, never the row it is on: a row also holds what follows the
/// statement (`// note`, `/* c */`, a second statement) and none of that is part of a definition.
pub(super) fn show_as_written(out: &mut Extract, spelling: Spelling) {
    for sym in out.syms.iter_mut().filter(|sym| sym.kind != "class") {
        sym.sig = written_signature(&sym.sig, spelling);
    }
}

/// `sig`, a statement read as SCSS, without its comments and in the spelling of `spelling`.
///
/// A string and an unquoted `url(...)` are skipped over, so the `//` of `"a//b"` and of `url(http://x)` is
/// no comment.
fn written_signature(sig: &str, spelling: Spelling) -> String {
    let chars: Vec<char> = sig.chars().collect();
    let mut plain = String::with_capacity(sig.len());
    let mut at = 0;

    while let Some(&c) = chars.get(at) {
        let next = chars.get(at + 1).copied();

        if c == '"' || c == '\'' {
            let end = quoted_end(&chars, at);

            plain.extend(chars.get(at..end).unwrap_or_default());
            at = end;
        } else if c == '(' && ends_with_url_word(&plain) && closes_on_the_line(&chars, at) {
            let end = chars
                .iter()
                .skip(at)
                .position(|c| *c == ')')
                .map_or(chars.len(), |found| at + found + 1);

            plain.extend(chars.get(at..end).unwrap_or_default());
            at = end;
        } else if c == '/' && next == Some('/') {
            break;
        } else if c == '/' && next == Some('*') {
            let end = (at + 2..chars.len())
                .find(|&i| chars.get(i) == Some(&'*') && chars.get(i + 1) == Some(&'/'))
                .map_or(chars.len(), |i| i + 2);
            let after = chars.get(end..).unwrap_or_default().iter().find(|c| !c.is_whitespace());

            plain.truncate(plain.trim_end().len());

            // what was around the comment stays apart, but not from a `;`, `,` or `)` after it
            if !plain.is_empty() && after.is_some_and(|after| !matches!(after, ';' | ',' | ')')) {
                plain.push(' ');
            }

            at = end;

            while chars.get(at).is_some_and(|c| c.is_whitespace()) {
                at += 1;
            }
        } else if c == '$'
            && spelling == Spelling::Less
            && next.is_some_and(|n| n.is_alphabetic() || n == '_' || n == '-')
        {
            plain.push('@');
            at += 1;
        } else {
            plain.push(c);
            at += 1;
        }
    }

    let plain = plain.trim();

    match spelling {
        Spelling::Less => shorten(plain),
        Spelling::Sass => {
            let statement = plain.trim_end_matches('{').trim_end().trim_end_matches(';');

            shorten(&match statement.strip_prefix("@mixin ") {
                Some(mixin) => format!("={mixin}"),
                None => statement.to_string(),
            })
        }
    }
}

/// True when `plain` ends with the word `url` (`curl` is no URL).
fn ends_with_url_word(plain: &str) -> bool {
    let lower = plain.to_ascii_lowercase();

    lower
        .strip_suffix("url")
        .is_some_and(|before| !before.ends_with(|c: char| c.is_alphanumeric() || c == '-' || c == '_'))
}

/// True when a `)` closes the `(` at `open` before the line ends: a `url(` that is not closed is no URL, and
/// what follows it may be a comment.
fn closes_on_the_line(chars: &[char], open: usize) -> bool {
    chars.iter().skip(open).take_while(|c| **c != '\n').any(|c| *c == ')')
}

/// The offset after the string whose quote is at `open`: a string ends with its line at the latest.
fn quoted_end(chars: &[char], open: usize) -> usize {
    let quote = chars.get(open).copied().unwrap_or('"');
    let mut at = open + 1;

    while let Some(&c) = chars.get(at) {
        match c {
            '\\' => at += 2,
            '\n' => return at,
            c if c == quote => return at + 1,
            _ => at += 1,
        }
    }

    chars.len()
}

/// What ends a rule or a declaration that nothing is indented under.
#[derive(Clone, Copy)]
enum Leaf {
    /// `color: red`, `@include box`: a semicolon.
    Statement,
    /// `.card`, `&:hover`: a rule without declarations.
    Rule,
}

/// A statement whose line has been written but whose end is not yet known: the next statement tells whether it
/// opens a block.
struct Pending {
    /// The last line of the statement that holds code.
    line: usize,
    indent: usize,
    leaf: Leaf,
    /// What closes the parentheses left open when the statement was cut off.
    closing: String,
}

/// One output line: the code, then a trailing comment. A brace or semicolon is added to the code, so it never
/// ends up inside a comment.
#[derive(Default)]
struct Row {
    code: String,
    comment: String,
}

/// The SCSS for the Sass `src`, with as many lines.
pub(super) fn to_scss(src: &str) -> String {
    let src = src.strip_prefix('\u{feff}').unwrap_or(src);
    let rows: Vec<&str> = src
        .split('\n')
        .map(|row| row.trim_end_matches(['\r', ' ', '\t']))
        .collect();
    let mut writer = Writer::default();
    let mut at = 0;

    while let Some(&row) = rows.get(at) {
        let text = row.trim_start_matches([' ', '\t']);
        let indent = row.len() - text.len();

        if text.is_empty() {
            writer.rows.push(Row::default());
            at += 1;
        } else if text.starts_with("//") || text.starts_with("/*") {
            at = writer.comment(&rows, at, indent);
        } else {
            writer.settle(Some(indent));
            at = writer.statement(&rows, at, indent);
        }
    }

    writer.settle(None);

    let mut scss = String::with_capacity(src.len() + src.len() / 8);

    for (number, row) in writer.rows.iter().enumerate() {
        if number > 0 {
            scss.push('\n');
        }

        scss.push_str(&row.code);
        scss.push_str(&row.comment);
    }

    scss
}

#[derive(Default)]
struct Writer {
    rows: Vec<Row>,
    /// The indentation of each statement whose block is open, outermost first.
    open: Vec<usize>,
    pending: Option<Pending>,
    /// The line braces that close blocks are added to.
    last_code: usize,
}

impl Writer {
    /// Ends the statement waiting for its end, given the indentation of the one after it (`None` at the end of the
    /// file), and closes the blocks that statement is no longer in.
    fn settle(&mut self, next: Option<usize>) {
        if let Some(pending) = self.pending.take() {
            let opens = next.is_some_and(|next| next > pending.indent) && self.open.len() < MAX_NESTING;

            if let Some(row) = self.rows.get_mut(pending.line) {
                row.code.push_str(&pending.closing);

                match (opens, pending.leaf) {
                    (true, _) => row.code.push_str(" {"),
                    (false, Leaf::Statement) => row.code.push(';'),
                    (false, Leaf::Rule) => row.code.push_str(" {}"),
                }
            }

            if opens {
                self.open.push(pending.indent);
            }

            self.last_code = pending.line;
        }

        while let Some(&top) = self.open.last() {
            if next.is_some_and(|next| top < next) {
                break;
            }

            self.open.pop();

            if let Some(row) = self.rows.get_mut(self.last_code) {
                row.code.push('}');
            }
        }
    }

    /// A comment at `indent` and every line under it, read as `//` comments whatever they hold. Gives the line after.
    fn comment(&mut self, rows: &[&str], at: usize, indent: usize) -> usize {
        let mut end = at + 1;
        let mut look = at + 1;

        while let Some(row) = rows.get(look) {
            let text = row.trim_start_matches([' ', '\t']);

            if text.is_empty() {
                look += 1;
            } else if row.len() - text.len() > indent {
                look += 1;
                end = look;
            } else {
                break;
            }
        }

        for row in rows.get(at..end).unwrap_or_default() {
            let text = row.trim_start_matches([' ', '\t']);

            if text.is_empty() {
                self.rows.push(Row::default());
            } else if text.starts_with("//") {
                self.rows.push(Row {
                    code: String::new(),
                    comment: (*row).to_string(),
                });
            } else {
                self.rows.push(Row {
                    code: String::new(),
                    comment: format!("//{row}"),
                });
            }
        }

        end
    }

    /// A statement: its first line and the lines that continue it. Gives the line after.
    fn statement(&mut self, rows: &[&str], at: usize, indent: usize) -> usize {
        let first = rows.get(at).copied().unwrap_or_default();
        let (mut lead, text) = first.split_at(indent);
        let (head, comment, mut depth) = split_comment(text, 0);
        let head = long_form(head);
        let leaf = leaf_of(&head);
        let importing = head.starts_with("@import");
        let selector = !matches!(leaf, Leaf::Statement) || head.starts_with('@');
        let mut code = if importing { quote_imports(&head) } else { head };
        let mut comment = comment;
        let mut paren_lines = 1;
        let mut next = at + 1;

        loop {
            self.rows.push(Row {
                code: format!("{lead}{code}"),
                comment: std::mem::take(&mut comment),
            });

            lead = "";

            let line = self.rows.len() - 1;
            let continues = if depth > 0 {
                paren_lines < MAX_PAREN_LINES
            } else {
                selector && code.trim_end().ends_with(',')
            };

            if !continues {
                let closing = ")".repeat(depth);

                self.pending = Some(Pending {
                    line,
                    indent,
                    leaf,
                    closing,
                });

                return next;
            }

            // blank lines between the parts stay blank; nothing follows when the file ends
            let mut after = next;

            while rows.get(after).is_some_and(|row| row.trim().is_empty()) {
                after += 1;
            }

            let Some(&row) = rows.get(after) else {
                self.pending = Some(Pending {
                    line,
                    indent,
                    leaf,
                    closing: ")".repeat(depth),
                });

                return next;
            };

            for _ in next..after {
                self.rows.push(Row::default());
            }

            let (part, trailing, now) = split_comment(row, depth);

            depth = now;
            paren_lines += after + 1 - next;
            next = after + 1;
            code = if importing {
                quote_imports(part)
            } else {
                part.to_string()
            };
            comment = trailing;
        }
    }
}

/// `text` without a comment at its end, the comment (a `/*` that is not closed gets its `*/`), and the depth of
/// parentheses after it, starting from `depth`.
fn split_comment(text: &str, mut depth: usize) -> (&str, String, usize) {
    let bytes = text.as_bytes();
    let mut at = 0;

    while let Some(&byte) = bytes.get(at) {
        match byte {
            b'"' | b'\'' => at = string_end(bytes, at),
            b'\\' => at += 2,
            b'(' => {
                if at >= 3
                    && text
                        .get(at - 3..at)
                        .is_some_and(|name| name.eq_ignore_ascii_case("url"))
                {
                    let quoted = text
                        .get(at + 1..)
                        .is_some_and(|rest| rest.trim_start().starts_with(['"', '\'']));

                    if !quoted {
                        at = bytes
                            .iter()
                            .skip(at)
                            .position(|b| *b == b')')
                            .map_or(bytes.len(), |end| at + end + 1);
                        continue;
                    }
                }

                depth += 1;
                at += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                at += 1;
            }
            b'/' if bytes.get(at + 1) == Some(&b'/') => {
                let code = text.get(..at).unwrap_or_default().trim_end();

                return (code, text.get(code.len()..).unwrap_or_default().to_string(), depth);
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                let closed = text.get(at + 2..).and_then(|rest| rest.find("*/"));

                match closed {
                    Some(end) => at += end + 4,
                    None => {
                        let code = text.get(..at).unwrap_or_default().trim_end();
                        let comment = format!("{} */", text.get(code.len()..).unwrap_or_default());

                        return (code, comment, depth);
                    }
                }
            }
            _ => at += 1,
        }
    }

    (text, String::new(), depth)
}

/// The offset after the string that starts at `at`: a string ends with its line at the latest.
fn string_end(bytes: &[u8], at: usize) -> usize {
    let quote = bytes.get(at).copied().unwrap_or(b'"');
    let mut cursor = at + 1;

    while let Some(&byte) = bytes.get(cursor) {
        match byte {
            b'\\' => cursor += 2,
            b if b == quote => return cursor + 1,
            _ => cursor += 1,
        }
    }

    bytes.len()
}

/// `=name(...)` as `@mixin name(...)` and `+name(...)` as `@include name(...)`. A `+` followed by a space or a
/// selector character is the combinator of a selector and is left alone.
fn long_form(code: &str) -> String {
    let named = |rest: &str| rest.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_');

    match (code.strip_prefix('='), code.strip_prefix('+')) {
        (Some(rest), _) if named(rest) => format!("@mixin {rest}"),
        (_, Some(rest)) if named(rest) => format!("@include {rest}"),
        _ => code.to_string(),
    }
}

/// What ends the statement `head` when nothing is indented under it.
fn leaf_of(head: &str) -> Leaf {
    if let Some(rest) = head.strip_prefix('@') {
        let name = rest
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .next()
            .unwrap_or_default();

        return if BLOCK_AT_RULES.contains(&name) || name.ends_with("keyframes") {
            Leaf::Rule
        } else {
            Leaf::Statement
        };
    }

    if is_declaration(head) {
        Leaf::Statement
    } else {
        Leaf::Rule
    }
}

/// True for `color: red`, `$gap: 4px` and `--x: 1`; false for a selector. Sass tells them apart by what follows
/// the colon: `color:red` is a selector, `color: red` a declaration.
fn is_declaration(head: &str) -> bool {
    if head.starts_with('$') || head.starts_with("--") {
        return true;
    }

    let bytes = head.as_bytes();
    let mut at = 0;
    let mut depth = 0usize;

    while let Some(&byte) = bytes.get(at) {
        match byte {
            b'"' | b'\'' => {
                at = string_end(bytes, at);
                continue;
            }
            b'\\' => at += 1,
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth = depth.saturating_sub(1),
            b':' if depth == 0 => {
                let property = head.get(..at).unwrap_or_default().trim_end();
                let after = bytes.get(at + 1).copied();
                let spaced = after.is_none_or(|b| b == b' ' || b == b'\t');
                let plain = !property.is_empty()
                    && !property.contains(|c: char| {
                        c.is_whitespace() || matches!(c, '.' | '&' | '>' | '+' | '~' | '*' | ',' | '%' | '[' | '@')
                    });

                return spaced && plain;
            }
            _ => {}
        }

        at += 1;
    }

    false
}

/// `@import a, b` as `@import "a", "b"`: the grammar wants strings. A name with a space or a parenthesis (a media
/// query, `url(...)`) is left as written, and so is one that is quoted.
fn quote_imports(code: &str) -> String {
    let list = code.strip_prefix("@import").unwrap_or(code);
    let prefix = code.get(..code.len() - list.len()).unwrap_or_default();
    let mut out = String::with_capacity(code.len() + 8);
    let mut piece = 0;

    out.push_str(prefix);

    for comma in top_level_commas(list) {
        quote_piece(list.get(piece..comma).unwrap_or_default(), &mut out);
        out.push(',');
        piece = comma + 1;
    }

    quote_piece(list.get(piece..).unwrap_or_default(), &mut out);

    out
}

/// The offsets of the commas of `list` that are in no string or parentheses.
fn top_level_commas(list: &str) -> Vec<usize> {
    let bytes = list.as_bytes();
    let mut commas = Vec::new();
    let mut depth = 0usize;
    let mut at = 0;

    while let Some(&byte) = bytes.get(at) {
        match byte {
            b'"' | b'\'' => {
                at = string_end(bytes, at);
                continue;
            }
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => commas.push(at),
            _ => {}
        }

        at += 1;
    }

    commas
}

/// One name of an `@import` list, with its spaces around it.
fn quote_piece(piece: &str, out: &mut String) {
    let name = piece.trim();
    let quote = !name.is_empty()
        && !name.starts_with(['"', '\''])
        && !name.contains(|c: char| c.is_whitespace() || c == '(' || c == ')');

    if !quote {
        out.push_str(piece);

        return;
    }

    let lead = piece.len() - piece.trim_start().len();

    out.push_str(piece.get(..lead).unwrap_or_default());
    out.push('"');
    out.push_str(name);
    out.push('"');
    out.push_str(piece.get(lead + name.len()..).unwrap_or_default());
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::code::extract::FILE_LIMIT;

    fn run(src: &str) -> Extract {
        super::super::extract(&mut Parser::new(), "a.sass", src)
    }

    fn defs(out: &Extract) -> Vec<(&str, &str, usize)> {
        out.syms.iter().map(|s| (s.kind, s.name.as_str(), s.line)).collect()
    }

    fn lines(text: &str) -> usize {
        text.matches('\n').count()
    }

    #[test]
    fn every_construct_keeps_its_line() {
        let src = "\
@use \"tokens\" as t
$gap: 4px
=box($p: 1px)
  padding: $p
%base
  margin: 0
@function rem($px)
  @return $px / 16
.card,
.panel
  @extend %base
  +box(2px)
  &--on
    color: red
  &:hover
  .title
    gap: t.$size
  @if $gap == 1
    a: b
  @else
    a: c
  @media (min-width: 1px)
    .wide
      top: 0
@keyframes spin
  from
    top: 0
.btn
  animation: spin 1s ease-in infinite
";
        let out = run(src);
        let found = defs(&out);

        for wanted in [
            ("const", "$gap", 2),
            ("macro", "box", 3),
            ("class", "%base", 5),
            ("fn", "rem", 7),
            ("class", ".card", 9),
            ("class", ".panel", 10),
            ("class", ".card--on", 13),
            ("class", ".card:hover", 15),
            ("class", ".card .title", 16),
            ("class", ".panel .title", 16),
            ("class", ".card .wide", 23),
            ("const", "spin", 25),
            ("class", ".btn", 28),
        ] {
            assert!(found.contains(&wanted), "{wanted:?} in {found:?}");
        }

        let signature = |name: &str| out.syms.iter().find(|s| s.name == name).map(|s| s.sig.as_str());

        assert_eq!(signature("$gap"), Some("$gap: 4px"));
        assert_eq!(signature("box"), Some("=box($p: 1px)"));

        let used: Vec<_> = out.refs.iter().map(|r| (r.kind, r.name.as_str(), r.line)).collect();

        for wanted in [
            ("call", "%base", 11),
            ("macro", "box", 12),
            ("use", "$size", 17),
            ("use", "$gap", 18),
            ("use", "spin", 29),
        ] {
            assert!(used.contains(&wanted), "{wanted:?} in {used:?}");
        }
    }

    #[test]
    fn only_a_closed_url_hides_a_double_slash_and_curl_is_no_url() {
        let sig = |text: &str| written_signature(text, Spelling::Less);

        assert_eq!(sig("@e: url(http://x) // note"), "@e: url(http://x)");
        assert_eq!(sig("@e: url(http://x // note"), "@e: url(http:");
        assert_eq!(sig("@e: curl(http://x) // note"), "@e: curl(http:");
        assert_eq!(sig("@e: URL(http://x) // note"), "@e: URL(http://x)");
    }

    #[test]
    fn indentation_becomes_braces_without_moving_a_line() {
        let scss = to_scss(".a\n  b: c\n  .d\n    e: f\n.g\n  h: i\n");

        assert_eq!(scss, ".a {\n  b: c;\n  .d {\n    e: f;}}\n.g {\n  h: i;}\n");
    }

    #[test]
    fn comments_swallow_what_is_indented_under_them() {
        let scss = to_scss("// note {\n  .x { not code\n.a\n  /* loud\n    more\n  b: c // tail\n.z\n");

        assert_eq!(lines(&scss), 7);
        assert!(
            scss.lines()
                .take(5)
                .all(|row| row.starts_with("//") || row.starts_with(".a")),
            "{scss}"
        );

        let out = run("// note {\n  .x\n.a\n  /* loud\n    more\n  b: c // tail\n.z\n");
        let found = defs(&out);

        assert_eq!(found, [("class", ".a", 3), ("class", ".z", 7)]);
    }

    #[test]
    fn a_combinator_and_an_include_are_told_apart() {
        let out = run("=m\n  a: b\n.a\n  + .b\n    c: d\n  +m\n  +m()\n");
        let found = defs(&out);

        assert!(found.contains(&("class", ".a + .b", 4)), "{found:?}");
        assert_eq!(
            out.refs.iter().filter(|r| r.kind == "macro" && r.name == "m").count(),
            2
        );
    }

    #[test]
    fn selectors_continue_over_commas_and_values_over_parentheses() {
        let out = run("$gap: 1px\n$map: (\n  a: 1,\n  b: (c: $gap)\n)\n.a,\n\n.b,\n.c\n  d: e\n.after\n  f: g\n");
        let found = defs(&out);

        assert!(found.contains(&("const", "$map", 2)), "{found:?}");
        assert!(found.contains(&("class", ".a", 6)), "{found:?}");
        assert!(found.contains(&("class", ".b", 8)), "{found:?}");
        assert!(found.contains(&("class", ".c", 9)), "{found:?}");
        assert!(found.contains(&("class", ".after", 11)), "{found:?}");
        assert!(out.refs.iter().any(|r| r.name == "$gap" && r.line == 4));
    }

    #[test]
    fn a_colon_without_a_space_is_a_selector() {
        let out = run(".a\n  b:hover\n    c: d\n  e:f\n.g\n  h: i\n");
        let found = defs(&out);

        assert!(found.contains(&("class", ".a b:hover", 2)), "{found:?}");
        assert!(found.contains(&("class", ".a e:f", 4)), "{found:?}");
        assert!(found.contains(&("class", ".g", 5)), "{found:?}");
    }

    #[test]
    fn unquoted_imports_are_quoted() {
        let scss = to_scss("@import a, b/c\n@import \"d\", url(e.css)\n");

        assert_eq!(scss, "@import \"a\", \"b/c\";\n@import \"d\", url(e.css);\n");

        let out = run("@import base, ../theme\n");

        assert_eq!(
            out.imports.iter().map(|i| i.source.as_str()).collect::<Vec<_>>(),
            ["base", "../theme"]
        );
    }

    #[test]
    fn a_trailing_comment_is_found_outside_strings_and_urls() {
        let out =
            run("$gap: 1px\n.a\n  background: url(http://x/y.png) // tail\n  content: \"// no\"\n.b\n  top: $gap\n");
        let found = defs(&out);

        assert_eq!(found, [("const", "$gap", 1), ("class", ".a", 2), ("class", ".b", 5)]);
        assert!(out.refs.iter().any(|r| r.name == "$gap" && r.line == 6));
    }

    #[test]
    fn a_dedent_closes_every_level_it_leaves() {
        let out = run(".a\n  .b\n    .c\n      d: e\n  .f\n    g: h\n.i\n  j: k\n");
        let found = defs(&out);

        assert!(found.contains(&("class", ".a .f", 5)), "{found:?}");
        assert!(found.contains(&("class", ".i", 7)), "{found:?}");
    }

    #[test]
    fn else_chains_and_nested_suffixes_keep_their_rules() {
        let out =
            run(".a\n  b: c\n  &__x\n    d: e\n  @if true\n    f: g\n  @else if false\n    h: i\n  @else\n    j: k\n");

        assert_eq!(defs(&out), [("class", ".a", 1), ("class", ".a__x", 3)]);
    }

    #[test]
    fn nesting_and_open_parentheses_stay_bounded() {
        let deep: String = (0..1_000).map(|i| format!("{}.a{i}\n", " ".repeat(i))).collect();
        let scss = to_scss(&deep);

        assert_eq!(lines(&scss), lines(&deep));

        let mut level = 0usize;
        let mut deepest = 0usize;

        for c in scss.chars() {
            match c {
                '{' => level += 1,
                '}' => level = level.saturating_sub(1),
                _ => {}
            }

            deepest = deepest.max(level);
        }

        // an empty rule at the deepest level is one more
        assert!(deepest <= MAX_NESTING + 1, "{deepest}");

        let open = format!("$a: (\n{}.next\n  b: c\n", "k: 1,\n".repeat(200));
        let out = run(&open);

        assert_eq!(lines(&to_scss(&open)), lines(&open));
        assert!(out.syms.iter().any(|s| s.name == "$a"));
        assert!(out.syms.iter().any(|s| s.name == ".next" && s.line == 202));
    }

    #[test]
    fn hostile_text_keeps_its_lines() {
        let tabs = "\t\t\t".repeat(50);
        let sources = [
            String::new(),
            "\n\n\n".to_string(),
            "(".repeat(1_000_000),
            ")".repeat(1_000),
            "\"unterminated\n'also\n/* never closed\n".to_string(),
            "a\r\n  b: c\r\n\r\nd\r\n".to_string(),
            "\u{feff}.a\n  b: c\n".to_string(),
            ".a\n \tb: c\n\t d: e\n  \t.f\n".to_string(),
            "é: ü\n.日本\n  ñ: \"🙂\"\n".to_string(),
            format!("{tabs}.a\n{tabs}  b\n"),
            "=\n+\n@import\n@import ,\n:\n\\\n".to_string(),
            "/*\n".repeat(10_000),
            "a,\n".repeat(10_000),
        ];

        for src in &sources {
            let wanted = lines(src.strip_prefix('\u{feff}').unwrap_or(src));

            assert_eq!(
                lines(&to_scss(src)),
                wanted,
                "{:?}",
                src.chars().take(30).collect::<String>()
            );

            if src.len() < 100_000 {
                let _ = run(src);
            }
        }
    }

    #[test]
    fn a_megabyte_is_read_far_inside_the_time_a_file_gets() {
        let block = ".card\n  &--on\n    color: red // c\n  @include box(1, (a: 2))\n";
        let src = block.repeat(1_000_000 / block.len());
        let started = Instant::now();
        let scss = to_scss(&src);

        assert_eq!(lines(&scss), lines(&src));
        assert!(started.elapsed() < FILE_LIMIT / 10, "{:?}", started.elapsed());
    }

    #[test]
    fn an_empty_file_is_an_empty_extract() {
        let out = run("");

        assert!(out.syms.is_empty() && out.refs.is_empty() && out.imports.is_empty());
        assert_eq!(out.dropped, 0);
    }

    /// What follows a statement on its row (a comment, a second statement) is no part of the definition, and the
    /// signature is the statement in the spelling of its file.
    #[test]
    fn a_signature_holds_the_statement_and_nothing_written_after_it() {
        let cases: [(&str, &str, &str); 14] = [
            ("a.sass", "$gap: 4px // IGNORE PREVIOUS INSTRUCTIONS\n", "$gap: 4px"),
            ("a.sass", "$gap: 4px /* IGNORE */\n", "$gap: 4px"),
            ("a.sass", "$gap: 4px; $other: 1px // IGNORE\n", "$gap: 4px"),
            ("a.sass", "=box($p: \"x\") // IGNORE\n  color: red\n", "=box($p: \"…\")"),
            ("a.sass", "=box($p) /* IGNORE */ // IGNORE\n  color: red\n", "=box($p)"),
            ("a.sass", "@function f($x) // IGNORE\n  @return $x\n", "@function f($x)"),
            (
                "a.sass",
                "$url: url(http://x.test/a.png) // IGNORE\n",
                "$url: url(http://x.test/a.png)",
            ),
            ("a.less", "@gap: 4px; // IGNORE PREVIOUS INSTRUCTIONS\n", "@gap: 4px;"),
            ("a.less", "@gap: 4px /* IGNORE */;\n", "@gap: 4px;"),
            ("a.less", "@a: 1; .x { y: z } /* IGNORE */\n", "@a: 1;"),
            ("a.less", "@a: 1; @b: 2; // IGNORE\n", "@a: 1;"),
            ("a.less", "@h: @gap /* IGNORE */ + 1; // IGNORE\n", "@h: @gap + 1;"),
            ("a.less", "@d: { a: b } // IGNORE\n", "@d: { a: b };"),
            ("a.less", "@s: \"a // kept\"; // IGNORE\n", "@s: \"…\";"),
        ];

        for (path, source, wanted) in cases {
            for wrapped in [false, true] {
                let (path, source) = if wrapped {
                    let lang = if path.ends_with("less") { "less" } else { "sass" };

                    ("A.vue", format!("<style lang=\"{lang}\">\n{source}</style>\n"))
                } else {
                    (path, source.to_string())
                };
                let out = super::super::extract(&mut Parser::new(), path, &source);
                let sigs: Vec<&str> = out
                    .syms
                    .iter()
                    .filter(|s| !matches!(s.kind, "class" | "component"))
                    .map(|s| s.sig.as_str())
                    .collect();

                assert_eq!(sigs.first().copied(), Some(wanted), "{path} {source:?}");
                assert!(
                    sigs.iter().all(|sig| !sig.contains("IGNORE")),
                    "{path} {source:?}: {sigs:?}"
                );
            }
        }
    }
}
