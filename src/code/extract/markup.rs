//! The classes a template or a page writes: `class="a b"`, `class={x}`, `:class="..."`, `class:foo`, `class:list`.
//!
//! One forward pass over the markup (the blanked copy the callers make, so scripts and styles are not markup
//! and the lines are the file's own). It never goes back: every step moves on, a scan that fails is not
//! repeated, and the end of the file ends the pass. A quote that never closes ends its value at the next space
//! or `>`, so the attributes after it are still read.
//!
//! Words written out are split at spaces. An expression (`class={cond ? 'a' : 'b'}`, `:class="[a, {b: c}]"`)
//! is not read in place: it is copied to a second buffer of the same length that holds only expressions, each
//! as `;(expr)` at its own bytes, and that buffer is parsed once as TSX. Every `expression_statement` of the
//! tree is then a class position for [`class_expression`], and a row of the tree is a line of the file.

use std::collections::HashSet;
use std::path::Path;

use tree_sitter::Parser;

use super::classes::{class_expression, split_tokens};
use super::sfc::SCAN_CHECK;
use super::{Extract, out_of_time};

/// The markup dialects, which differ in where an expression may stand.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Dialect {
    Vue,
    Svelte,
    Astro,
    Html,
}

impl Dialect {
    pub(super) fn of(path: &str) -> Self {
        let extension = Path::new(path)
            .extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase);

        match extension.as_deref() {
            Some("vue") => Dialect::Vue,
            Some("astro") => Dialect::Astro,
            Some("html") => Dialect::Html,
            _ => Dialect::Svelte,
        }
    }

    /// Svelte and Astro write an expression in braces anywhere in a tag or a value.
    fn braces(self) -> bool {
        matches!(self, Dialect::Svelte | Dialect::Astro)
    }
}

/// Braces, quotes and template literals nested deeper than this in one expression are not followed.
const MAX_NESTING: usize = 32;

/// The most bytes one scan for the `}` of an expression reads. A class expression is a few lines; a longer
/// stretch is a stray `{`, and it is not read to the end of the file.
const MAX_BRACE_SPAN: usize = 1 << 16;

/// The longest attribute name that is compared with the ones that hold classes.
const MAX_NAME: usize = 32;

/// The value of an attribute, as the offsets of `markup`.
enum Value {
    /// `"..."`; `holes` are the `{...}` of a Svelte or Astro value, as (open, close).
    Quoted {
        open: usize,
        close: usize,
        holes: Vec<(usize, usize)>,
    },
    /// `{...}` (Svelte, Astro).
    Braced { open: usize, close: usize },
    /// No quote: up to a space or `>`. Also a quoted value that never closes.
    Bare { start: usize, end: usize },
    /// No value (`class:foo`, `disabled`).
    Missing,
}

/// The line of a byte, for offsets asked in increasing order.
struct Lines {
    at: usize,
    line: usize,
}

impl Lines {
    fn of(&mut self, markup: &[u8], pos: usize) -> usize {
        // an offset behind the cursor starts the count again; the pass asks in order, so it does not happen
        if pos < self.at {
            self.at = 0;
            self.line = 1;
        }

        let between = markup.get(self.at..pos).unwrap_or(&[]);

        self.line += between.iter().filter(|byte| **byte == b'\n').count();
        self.at = pos;

        self.line
    }
}

struct Scan<'a> {
    markup: &'a [u8],
    dialect: Dialect,
    owner: Option<usize>,
    out: &'a mut Extract,
    /// The expressions, each at its own bytes of an otherwise blank copy; made when the first one is found.
    expr: Option<Vec<u8>>,
    /// The bytes read by the scans for a `}` that found none, against `brace_budget`. A stray `{` costs a scan to
    /// where the scan gave up; with no bound on the sum, a file of them would cost its square.
    failed_bytes: usize,
    /// What the failed scans may read in all: twice the length of the markup.
    brace_budget: usize,
    /// The failed scans spent the budget: braces are no longer matched, and the expressions after this point are
    /// not read.
    braces_off: bool,
    /// The scan for the `}` of the value being read found none: its expression is lost.
    value_failed: bool,
    /// No `"` (index 0) or `'` (index 1) closes after the last unclosed one: the scan is not repeated.
    quote_missing: [bool; 2],
    lines: Lines,
    next_check: usize,
    stopped: bool,
}

/// Reads the class attributes of `markup` into `out`, as references of `owner`. `sheets` are the names that
/// stand for a stylesheet module in the expressions.
pub(super) fn extract(
    parser: &mut Parser,
    markup: &[u8],
    dialect: Dialect,
    sheets: &HashSet<String>,
    owner: Option<usize>,
    out: &mut Extract,
) {
    let mut scan = Scan {
        markup,
        dialect,
        owner,
        out,
        expr: None,
        failed_bytes: 0,
        brace_budget: markup.len().saturating_mul(2),
        braces_off: false,
        value_failed: false,
        quote_missing: [false; 2],
        lines: Lines { at: 0, line: 1 },
        next_check: 0,
        stopped: false,
    };

    scan.run();

    let Scan { expr, out, stopped, .. } = scan;

    if stopped {
        out.dropped += 1;
        // the rest of the page was not read, and it may write classes
        out.classes_cut = true;
    }

    if let Some(expr) = expr
        && !stopped
    {
        read_expressions(parser, expr, sheets, owner, out);
    }
}

/// Parses the buffer of expressions once and reads each statement of it.
fn read_expressions(
    parser: &mut Parser,
    expr: Vec<u8>,
    sheets: &HashSet<String>,
    owner: Option<usize>,
    out: &mut Extract,
) {
    let Ok(text) = String::from_utf8(expr) else {
        return;
    };
    let Some(tree) = super::parse(parser, &super::ts::tsx(), &text) else {
        out.dropped += 1;
        // every class expression of the file is lost with the parse
        out.classes_cut = true;

        return;
    };
    let root = tree.root_node();
    let mut cursor = root.walk();

    for statement in root
        .named_children(&mut cursor)
        .filter(|node| node.kind() == "expression_statement")
    {
        let Some(expression) = statement.named_child(0) else {
            continue;
        };

        class_expression(expression, text.as_bytes(), sheets, &mut |token, qual, row| {
            out.add_class(owner, token, qual, row + 1);
        });
    }
}

impl Scan<'_> {
    /// True once the file's time has run out; the clock is read once per [`SCAN_CHECK`] bytes.
    fn tick(&mut self, at: usize) -> bool {
        if at >= self.next_check {
            self.next_check = at + SCAN_CHECK;

            if out_of_time() {
                self.stopped = true;
            }
        }

        self.stopped
    }

    fn run(&mut self) {
        let mut at = 0;

        while let Some(offset) = self
            .markup
            .get(at..)
            .and_then(|rest| rest.iter().position(|byte| *byte == b'<'))
        {
            let open = at + offset;

            if self.tick(open) {
                return;
            }

            at = self.element(open);
        }
    }

    /// What follows a `<`: a comment is skipped, a tag is read, anything else (`</div>`, `a < b`) is text.
    fn element(&mut self, open: usize) -> usize {
        let markup = self.markup;

        match markup.get(open + 1).copied() {
            Some(b'!') if markup.get(open + 1..open + 4).is_some_and(|dashes| dashes == b"!--") => markup
                .get(open + 4..)
                .and_then(|rest| rest.windows(3).position(|window| window == b"-->"))
                .map_or(markup.len(), |end| open + 4 + end + 3),
            Some(byte) if byte.is_ascii_alphabetic() => self.tag(open + 1),
            _ => open + 1,
        }
    }

    /// The attributes of the tag whose name starts at `start`; the offset after its `>`.
    fn tag(&mut self, start: usize) -> usize {
        let markup = self.markup;
        let mut at = start;

        while markup
            .get(at)
            .is_some_and(|byte| !byte.is_ascii_whitespace() && !matches!(byte, b'/' | b'>'))
        {
            at += 1;
        }

        loop {
            if self.tick(at) {
                return markup.len();
            }

            while markup.get(at).is_some_and(u8::is_ascii_whitespace) {
                at += 1;
            }

            match markup.get(at).copied() {
                None => return markup.len(),
                Some(b'>') => return at + 1,
                Some(b'/') => at += 1,
                // `{...props}` and `{shorthand}`
                Some(b'{') if self.dialect.braces() => at = self.skip_brace(at),
                _ => at = self.attribute(at),
            }
        }
    }

    /// The offset after the `{...}` that opens at `open` (one byte on when it never closes).
    fn skip_brace(&mut self, open: usize) -> usize {
        match self.brace(open) {
            Some(close) => close + 1,
            None => open + 1,
        }
    }

    /// The offset of the `}` that closes the `{` at `open`, or `None` when it does not close within
    /// [`MAX_BRACE_SPAN`] bytes. A scan that finds none costs what it read, and the sum of those is kept to the
    /// budget of the file: past it braces are no longer matched, so one stray `{` loses the expressions it
    /// swallows and not those of the rest of the file, while a file of them is still read in linear time.
    fn brace(&mut self, open: usize) -> Option<usize> {
        if self.braces_off {
            return None;
        }

        match match_brace(self.markup, open, MAX_BRACE_SPAN) {
            Ok(close) => Some(close),
            Err(gave_up) => {
                self.value_failed = true;
                self.failed_bytes = self.failed_bytes.saturating_add(gave_up.saturating_sub(open).max(1));
                self.braces_off = self.failed_bytes > self.brace_budget;

                None
            }
        }
    }

    /// One attribute starting at `start`; the offset after it.
    fn attribute(&mut self, start: usize) -> usize {
        let markup = self.markup;
        let mut at = start;

        while markup
            .get(at)
            .is_some_and(|byte| !byte.is_ascii_whitespace() && !matches!(byte, b'=' | b'>' | b'/'))
        {
            at += 1;
        }

        // a stray `=` or quote: moving on one byte is what keeps the pass finite
        if at == start {
            return start + 1;
        }

        let name = markup.get(start..at).unwrap_or(&[]);
        let mut value_at = at;

        while markup.get(value_at).is_some_and(u8::is_ascii_whitespace) {
            value_at += 1;
        }

        if markup.get(value_at) != Some(&b'=') {
            self.value_failed = false;
            self.apply(start, name, &Value::Missing);

            return at;
        }

        value_at += 1;

        while markup.get(value_at).is_some_and(u8::is_ascii_whitespace) {
            value_at += 1;
        }

        self.value_failed = false;

        let (value, end) = self.value(value_at);

        self.apply(start, name, &value);

        end
    }

    fn value(&mut self, at: usize) -> (Value, usize) {
        let markup = self.markup;

        match markup.get(at).copied() {
            Some(quote @ (b'"' | b'\'')) => self.quoted(at, quote),
            Some(b'{') if self.dialect.braces() => match self.brace(at) {
                Some(close) => (Value::Braced { open: at, close }, close + 1),
                None => self.bare(at),
            },
            Some(b'>') | None => (Value::Missing, at),
            _ => self.bare(at),
        }
    }

    fn bare(&self, start: usize) -> (Value, usize) {
        let mut end = start;

        while self
            .markup
            .get(end)
            .is_some_and(|byte| !byte.is_ascii_whitespace() && *byte != b'>')
        {
            end += 1;
        }

        (Value::Bare { start, end }, end)
    }

    /// A quoted value. The braces of a Svelte or Astro value are matched, so a quote inside one does not end it.
    fn quoted(&mut self, open: usize, quote: u8) -> (Value, usize) {
        let markup = self.markup;
        let which = usize::from(quote == b'\'');
        let mut holes = Vec::new();
        let mut at = open + 1;

        if self.quote_missing[which] {
            return self.bare(open + 1);
        }

        loop {
            match markup.get(at).copied() {
                None => break,
                Some(byte) if byte == quote => {
                    return (Value::Quoted { open, close: at, holes }, at + 1);
                }
                Some(b'{') if self.dialect.braces() => match self.brace(at) {
                    Some(close) => {
                        holes.push((at, close));
                        at = close + 1;
                    }
                    None => at += 1,
                },
                Some(_) => at += 1,
            }
        }

        // no closing quote to the end of the file: not looked for again, and the value ends where a bare one does
        self.quote_missing[which] = true;

        self.bare(open + 1)
    }

    /// True once an unclosed `{` or quote switched the matching of braces or quotes off.
    fn is_degraded(&self) -> bool {
        (self.braces_off && self.dialect.braces()) || self.quote_missing.iter().any(|missing| *missing)
    }

    /// True for a (lowercased) attribute name whose value may be an expression of classes.
    fn holds_expression(&self, lower: &[u8]) -> bool {
        match (self.dialect, lower) {
            (Dialect::Vue, b":class" | b"v-bind:class") | (Dialect::Astro, b"class:list") => true,
            (_, b"class") => self.dialect.braces(),
            _ => false,
        }
    }

    /// Reads the attribute `name` (at `start`) when it holds classes.
    fn apply(&mut self, start: usize, name: &[u8], value: &Value) {
        if name.len() > MAX_NAME {
            return;
        }

        let lower = name.to_ascii_lowercase();

        // once a brace or a quote never closed, the expressions after it are not matched, so a class attribute
        // that holds one may have lost its classes
        if (self.is_degraded() || self.value_failed) && self.holds_expression(&lower) {
            self.out.classes_cut = true;
        }

        match (self.dialect, lower.as_slice()) {
            (_, b"class") => self.class_value(value),
            (Dialect::Vue, b":class" | b"v-bind:class") => {
                if let Value::Quoted { open, close, .. } = value {
                    self.expression(*open, *close);
                }
            }
            (Dialect::Astro, b"class:list") => {
                if let Value::Braced { open, close } = value {
                    self.expression(*open, *close);
                }
            }
            // `class:foo={on}` is the class `foo`
            (Dialect::Svelte, _) if lower.starts_with(b"class:") => {
                let written = name.get(b"class:".len()..).unwrap_or(&[]);
                let word = written.split(|byte| *byte == b'|').next().unwrap_or(&[]);

                if let Ok(word) = std::str::from_utf8(word) {
                    let line = self.lines.of(self.markup, start);

                    self.out.add_class(self.owner, word, None, line);
                }
            }
            _ => {}
        }
    }

    fn class_value(&mut self, value: &Value) {
        match value {
            Value::Quoted { open, close, holes } => {
                self.words(open + 1, *close, holes);

                for (hole_open, hole_close) in holes {
                    self.expression(*hole_open, *hole_close);
                }
            }
            Value::Braced { open, close } => self.expression(*open, *close),
            Value::Bare { start, end } => self.words(*start, *end, &[]),
            Value::Missing => {}
        }
    }

    /// The words written out in `start..end`, split at spaces. A hole of Svelte or Astro, and a `{{ }}`, `{% %}`
    /// or `<% %>` of a page, count as a word that is no class, so `btn-{x}` gives nothing and `a {x} b` gives
    /// `a` and `b`.
    fn words(&mut self, start: usize, end: usize, holes: &[(usize, usize)]) {
        let Some(range) = self.markup.get(start..end) else {
            return;
        };
        let mut owned;
        let mut bytes = range;

        if !holes.is_empty() || self.dialect == Dialect::Html {
            owned = range.to_vec();

            for (open, close) in holes {
                mark(&mut owned, open - start..close + 1 - start);
            }

            if self.dialect == Dialect::Html {
                mark_templates(&mut owned);
            }

            bytes = &owned;
        }

        let Ok(text) = std::str::from_utf8(bytes) else {
            return;
        };
        let first = self.lines.of(self.markup, start);
        let owner = self.owner;
        let out = &mut *self.out;

        split_tokens(text, first, &mut |token, line| out.add_class(owner, token, None, line));
    }

    /// Copies the expression between `open` and `close` to its bytes of the expression buffer, as `;(expr)`:
    /// the `;` takes the blank before the opener (the `=` of `class={x}`) so that two expressions on a line are
    /// two statements and not a call.
    fn expression(&mut self, open: usize, close: usize) {
        let markup = self.markup;
        let Some(content) = markup.get(open + 1..close) else {
            return;
        };

        if content.iter().all(u8::is_ascii_whitespace) {
            return;
        }

        let expr = self.expr.get_or_insert_with(|| {
            markup
                .iter()
                .map(|byte| if *byte == b'\n' { b'\n' } else { b' ' })
                .collect()
        });

        // the separator is the nearest blank before the opener
        let mut at = open;
        let mut separator = None;

        for _ in 0..8 {
            if at == 0 {
                break;
            }

            at -= 1;

            match expr.get(at) {
                Some(b' ') => {
                    separator = Some(at);

                    break;
                }
                Some(b'\n') => {}
                _ => break,
            }
        }

        let Some(separator) = separator else {
            // two holes with nothing between them (`{a}{b}`): the opener is the `;` that ends the one before and
            // the closer a blank, so the expression stands as `;b `. An object literal needs its parentheses, and
            // takes the blank inside the hole for the opener's; one with none (`{{a: b}}`) is left out
            if !frame_adjacent(expr, markup, open, close) {
                self.out.dropped += 1;
                // the expression is left out, and it may write classes
                self.out.classes_cut = true;
            }

            return;
        };

        if let Some(byte) = expr.get_mut(separator) {
            *byte = b';';
        }

        if let Some(region) = expr.get_mut(open..=close) {
            region.copy_from_slice(markup.get(open..=close).unwrap_or(&[]));
        }

        if let Some(byte) = expr.get_mut(open) {
            *byte = b'(';
        }

        if let Some(byte) = expr.get_mut(close) {
            *byte = b')';
        }
    }
}

/// Copies the hole `open..=close` of `markup` into `expr` when no blank comes before it, framed as `;expr `: the
/// opener becomes the `;` and the closer a blank. Whether the hole is an object literal is read from its first
/// byte: that needs parentheses, so it takes the blank right after the opener for `(` and the closer for `)`
/// (`;( {a: b})`), and a hole with no such blank gives `false` and is not copied.
fn frame_adjacent(expr: &mut [u8], markup: &[u8], open: usize, close: usize) -> bool {
    let Some(content) = markup.get(open + 1..close) else {
        return false;
    };
    let object = content
        .iter()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|byte| *byte == b'{');
    let blank_first = content.first().is_some_and(|byte| *byte == b' ');

    if object && !blank_first {
        return false;
    }

    if let Some(region) = expr.get_mut(open..=close) {
        region.copy_from_slice(markup.get(open..=close).unwrap_or(&[]));
    }

    if let Some(byte) = expr.get_mut(open) {
        *byte = b';';
    }

    if object {
        if let Some(byte) = expr.get_mut(open + 1) {
            *byte = b'(';
        }

        if let Some(byte) = expr.get_mut(close) {
            *byte = b')';
        }
    } else if let Some(byte) = expr.get_mut(close) {
        *byte = b' ';
    }

    true
}

/// Replaces the bytes of `range` but newlines by `\0`, a byte no class holds.
fn mark(bytes: &mut [u8], range: std::ops::Range<usize>) {
    if let Some(region) = bytes.get_mut(range) {
        for byte in region {
            if *byte != b'\n' {
                *byte = 0;
            }
        }
    }
}

/// Marks the `{{ }}`, `{% %}` and `<% %>` of a page; one that never closes marks the rest of the value.
fn mark_templates(bytes: &mut [u8]) {
    let mut at = 0;

    while at < bytes.len() {
        let closer: &[u8] = match bytes.get(at..at + 2) {
            Some(b"{{") => b"}}",
            Some(b"{%") => b"%}",
            Some(b"<%") => b"%>",
            _ => {
                at += 1;

                continue;
            }
        };
        let end = bytes
            .get(at + 2..)
            .and_then(|rest| rest.windows(2).position(|window| window == closer))
            .map_or(bytes.len(), |found| at + 2 + found + 2);

        mark(bytes, at..end);
        at = end;
    }
}

/// The offset of the `}` that closes the `{` at `open`, reading strings and template literals as a script
/// does: a brace or a quote inside one does not count. `Err` with the offset the scan gave up at when it never
/// closes, runs past `limit` bytes, nests too deep or the time of the file ran out.
///
/// A quote that does not close on its line is taken as a plain byte, so an apostrophe in a comment cannot
/// swallow the file. That scan runs to the end of the line, so it is not made again for the quotes after it
/// on the line: none of them closes before the same line break either (the scan from a later quote follows the
/// one it would have followed from the first). A line of `"\"\"\"...` therefore costs its length once, not
/// once per quote.
fn match_brace(markup: &[u8], open: usize, limit: usize) -> Result<usize, usize> {
    // true: the text of a template literal; false: code between braces
    let mut modes: Vec<bool> = vec![false];
    // per quote kind (`"`, `'`): the offset up to which no quote of that kind closes
    let mut open_until = [0_usize; 2];
    let mut at = open + 1;
    let mut steps = 0_usize;

    while let Some(&byte) = markup.get(at) {
        steps += 1;

        if steps.is_multiple_of(SCAN_CHECK) && out_of_time() {
            return Err(at);
        }

        if at - open > limit {
            return Err(at);
        }

        if modes.last().copied().unwrap_or(false) {
            match byte {
                b'\\' => at += 2,
                b'`' => {
                    modes.pop();
                    at += 1;
                }
                b'$' if markup.get(at + 1) == Some(&b'{') => {
                    modes.push(false);
                    at += 2;
                }
                _ => at += 1,
            }

            continue;
        }

        match byte {
            b'{' => {
                if modes.len() >= MAX_NESTING {
                    return Err(at);
                }

                modes.push(false);
                at += 1;
            }
            b'}' => {
                modes.pop();

                if modes.is_empty() {
                    return Ok(at);
                }

                at += 1;
            }
            b'\'' | b'"' => {
                let which = usize::from(byte == b'\'');

                if open_until.get(which).is_some_and(|until| at < *until) {
                    at += 1;

                    continue;
                }

                match after_string(markup, at, byte) {
                    Ok(end) => at = end,
                    Err(until) => {
                        if let Some(slot) = open_until.get_mut(which) {
                            *slot = until;
                        }

                        at += 1;
                    }
                }
            }
            b'`' => {
                if modes.len() >= MAX_NESTING {
                    return Err(at);
                }

                modes.push(true);
                at += 1;
            }
            _ => at += 1,
        }
    }

    Err(markup.len())
}

/// The offset after the string that opens at `open`; or, when it does not close on its line, the offset where
/// the scan stopped (the line break or the end), up to which no quote of this kind closes.
fn after_string(markup: &[u8], open: usize, quote: u8) -> Result<usize, usize> {
    let mut at = open + 1;

    while let Some(&byte) = markup.get(at) {
        match byte {
            b'\\' => at += 2,
            b'\n' => return Err(at),
            _ if byte == quote => return Ok(at + 1),
            _ => at += 1,
        }
    }

    Err(markup.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &str, markup: &str) -> Extract {
        let mut out = Extract::default();
        let sheets = HashSet::from(["styles".to_string(), "$style".to_string()]);

        extract(
            &mut Parser::new(),
            markup.as_bytes(),
            Dialect::of(path),
            &sheets,
            None,
            &mut out,
        );

        out
    }

    fn classes(out: &Extract) -> Vec<(String, Option<String>, usize)> {
        out.refs
            .iter()
            .filter(|r| r.kind == "cls")
            .map(|r| (r.name.clone(), r.qual.clone(), r.line))
            .collect()
    }

    fn names(out: &Extract) -> Vec<String> {
        let mut names: Vec<String> = classes(out).into_iter().map(|(name, ..)| name).collect();

        names.sort();

        names
    }

    #[test]
    fn every_dialect_reads_a_class_attribute_with_the_line_it_is_on() {
        for path in ["a.vue", "a.svelte", "a.astro", "a.html"] {
            let out = read(
                path,
                "<div\n  class=\"a  b\n c\" id=\"d\">\n<p class='e'></p><i class=f></i></div>\n",
            );

            assert_eq!(
                classes(&out),
                vec![
                    ("a".to_string(), None, 2),
                    ("b".to_string(), None, 2),
                    ("c".to_string(), None, 3),
                    ("e".to_string(), None, 4),
                    ("f".to_string(), None, 4),
                ],
                "{path}"
            );
        }
    }

    #[test]
    fn svelte_and_astro_read_the_holes_of_a_class_value_and_class_braces_as_expressions() {
        for path in ["a.svelte", "a.astro"] {
            let out = read(
                path,
                "<p class=\"a {on ? 'b' : \"c\"} d btn-{x}\" title=\"t {u}\">\n<i class={cond && 'e'} class={['f', { g: x }]}></i>\n<b {...props} data-x={`}`} class={clsx('h')}></b></p>\n",
            );

            assert_eq!(names(&out), ["a", "b", "c", "d", "e", "f", "g", "h"], "{path}");
        }
    }

    #[test]
    fn svelte_class_directives_are_classes_and_astro_class_list_is_an_expression() {
        let svelte = read(
            "a.svelte",
            "<p class:active class:is-on={on} class:x|local={y} classy:no></p>\n",
        );

        assert_eq!(names(&svelte), ["active", "is-on", "x"]);

        let astro = read(
            "a.astro",
            "<p class:list={['a', { b: on }, on && 'c']} class:other={['no']}></p>\n",
        );

        assert_eq!(names(&astro), ["a", "b", "c"]);
    }

    #[test]
    fn vue_bound_classes_are_expressions_with_their_lines_and_style_is_the_module() {
        let out = read(
            "a.vue",
            "<template>\n<p :class=\"{ 'is-open': open, bar: x }\" v-bind:class='[a ? \"c\" : \"d\", $style.card, styles.row]'\n  class=\"e\"></p>\n<i :class=\"$style['x-y']\" @class=\"no\" :title=\"'no'\"></i>\n</template>\n",
        );

        let mut found = classes(&out);

        found.sort();

        let mut expected = vec![
            ("is-open".to_string(), None, 2),
            ("bar".to_string(), None, 2),
            ("c".to_string(), None, 2),
            ("d".to_string(), None, 2),
            ("card".to_string(), Some("$style".to_string()), 2),
            ("row".to_string(), Some("styles".to_string()), 2),
            ("e".to_string(), None, 3),
            ("x-y".to_string(), Some("$style".to_string()), 4),
        ];

        expected.sort();

        assert_eq!(found, expected);
    }

    #[test]
    fn a_page_drops_its_template_tags_before_it_splits() {
        let out = read(
            "a.html",
            "<div class=\"a {{ x }} b{% if y %} c {% endif %}d <%= e %> f g-{{ h }}\"></div>\n<!-- <p class=\"hidden\"> -->\n</div class=\"no\">\n",
        );

        assert_eq!(names(&out), ["a", "c", "f"]);
    }

    #[test]
    fn a_class_of_a_page_has_no_owner_and_none_is_kept_in_a_comment() {
        let out = read("a.html", "<!-- <p class=\"hidden\"> --><p class=\"shown\">\n");

        assert_eq!(names(&out), ["shown"]);
        assert!(out.refs.iter().all(|r| r.from.is_none() && !r.style));
    }

    #[test]
    fn a_page_that_lost_class_expressions_says_so_and_one_that_read_them_all_does_not() {
        // two holes with nothing between them are both read; an object literal that has no blank to take its
        // parentheses from is the one that is left out
        assert!(!read("a.svelte", "<p class=\"{a}{b}\">\n").classes_cut);
        assert!(!read("a.svelte", "<p class=\"{a} {b}\">\n").classes_cut);
        assert!(read("a.svelte", "<p class=\"{a}{{b: c}}\">\n").classes_cut);

        // a brace that never closes: the expressions after it are not matched
        assert!(read("a.svelte", "<p class={ oops>\n<i class={x}></i>\n").classes_cut);
        assert!(!read("a.svelte", "<p class={x}>\n<i class={y}></i>\n").classes_cut);
        assert!(read("a.vue", "<p id=\"a>\n<i :class=[x]></i>\n").classes_cut);
        assert!(!read("a.vue", "<p id=\"a\">\n<i :class=\"[x]\"></i>\n").classes_cut);

        // the clock ran out before the scan reached the tag
        let _restore =
            super::super::Restore(super::super::DEADLINE.with(|slot| slot.replace(Some(std::time::Instant::now()))));
        let stopped = read("a.html", "<p class=\"a\">\n");

        assert!(stopped.classes_cut && stopped.dropped == 1);
    }

    #[test]
    fn holes_with_nothing_between_them_are_all_read() {
        for path in ["a.svelte", "a.astro"] {
            let out = read(path, "<p class=\"{a ? 'x' : 'y'}{b ? 'z' : ''}\">\n");

            assert_eq!(names(&out), ["x", "y", "z"], "{path}");
            assert!(!out.classes_cut, "{path}");

            // three in a row, and an object literal among them
            let out = read(path, "<p class=\"p {'a'}{'b'}{ {c: on} }{'d'}\">\n");

            assert_eq!(names(&out), ["a", "b", "c", "d", "p"], "{path}");
        }
    }

    #[test]
    fn a_stray_brace_loses_the_expression_it_swallows_and_not_the_ones_after_it() {
        for path in ["a.svelte", "a.astro"] {
            let out = read(
                path,
                "<p title={ stray>\n<i class={cn('a')}></i>\n<b class={cn('b')}></b>\n",
            );

            assert_eq!(names(&out), ["a", "b"], "{path}");
        }

        // only a class attribute that lost its braces says so
        assert!(!read("a.svelte", "<p title={ stray>\n<i class={cn('a')}></i>\n").classes_cut);
        assert!(read("a.svelte", "<p class={ stray>\n<i class={cn('a')}></i>\n").classes_cut);
    }

    #[test]
    fn an_unclosed_quote_or_brace_does_not_lose_the_attributes_after_it() {
        let quote = read(
            "a.html",
            "<p class=\"a b id=\"c\" class=\"d\"></p>\n<i class='e class=\"f\"></i>\n",
        );

        // the first quote closes at the next one, as in a browser, and the attributes after it are read
        assert_eq!(names(&quote), ["a", "b", "d", "e", "f"]);

        let open = read("a.html", "<p class=\"a>\n<i class=b></i>\n");

        assert_eq!(names(&open), ["a", "b"]);

        let brace = read("a.svelte", "<p class={ oops>\n<i class=\"b\"></i>\n");

        assert_eq!(names(&brace), ["b"]);
    }

    #[test]
    fn hostile_markup_is_read_in_linear_time_without_a_panic() {
        let started = std::time::Instant::now();
        let big = 1 << 20;
        let inputs = [
            "class=\"".repeat(big / 8),
            "<p class=\"".repeat(big / 11),
            "<p class='".repeat(big / 10),
            "<p class={".repeat(big / 10),
            "{".repeat(big),
            "<p ".repeat(big / 3),
            "<!--".repeat(big / 4),
            "</".repeat(big / 2),
            "<p class={'".repeat(big / 11),
            "<p class=\"{`${".repeat(big / 14),
            "<p :class=\"".repeat(big / 12),
            "<p class={`a${b}".repeat(big / 16),
            "é".repeat(big / 2),
            "<p x={ ".repeat(big / 7),
            format!("<p x={{ {}\n", "y ".repeat(200)).repeat(big / 410),
            format!("<p x={{ {}\n", "y".repeat(70_000)).repeat(16),
        ];

        for path in ["a.html", "a.svelte", "a.astro", "a.vue"] {
            for input in &inputs {
                read(path, input);
            }
        }

        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "{:?}",
            started.elapsed()
        );
    }

    /// The `;(expr)` framing: the `;` before an expression ends the one before it, so each is a statement of its
    /// own and `(a)` followed by `(b)` is not a call. The first `;` stands alone as an empty statement.
    #[test]
    fn the_framing_of_expressions_parses_as_one_statement_each() {
        let text = "          ;(a && 'b')\n   ;({c: x})   ;(['d'])\n  ;(`e ${f}`)\n ;(cond ? 'g' : 'h')\n";
        let tree = super::super::parse(&mut Parser::new(), &super::super::ts::tsx(), text).unwrap();
        let root = tree.root_node();
        let mut cursor = root.walk();
        let kinds: Vec<&str> = root.named_children(&mut cursor).map(|n| n.kind()).collect();

        let mut expected = vec!["empty_statement"];

        expected.extend(["expression_statement"; 5]);

        assert_eq!(kinds, expected);
    }

    /// A quote that never closes on its line, written again and again on it, is scanned once: the line is
    /// 890 000 bytes of `\"` and the file has short lines too (a minified file is not read at all).
    #[test]
    fn a_line_of_unclosed_quotes_in_a_brace_is_read_in_linear_time() {
        use std::time::Instant;

        let long = format!("<a class={{\"{}\n", "\\\"".repeat(445_000));
        let source = format!("{long}{}", "<p>x</p>\n".repeat(12_500));

        for quote in ['"', '\''] {
            let source = source.replace('"', &quote.to_string());
            let started = Instant::now();
            let out = read("a.svelte", &source);

            assert!(
                started.elapsed() < super::super::FILE_LIMIT / 10,
                "{:?}",
                started.elapsed()
            );
            assert!(out.refs.iter().all(|r| r.kind != "cls"));
        }
    }
}
