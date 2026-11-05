//! Less (`.less`): read by turning it into SCSS and reading that.
//!
//! Less and SCSS are both braces and semicolons, so most of a Less file is already SCSS. [`to_scss`] rewrites the
//! rest and never moves a line (a rewrite that drops text keeps its line breaks):
//!
//! | Less                                  | read as                          |
//! |---------------------------------------|----------------------------------|
//! | `@gap: 4px;`, `@gap`, `@@name`        | `$gap: 4px;`, `$gap`, `$name`    |
//! | `@{name}`                             | `#{$name}`                       |
//! | `.box(@p: 1px) when (@a) { }`         | `.box { }`                       |
//! | `.box(2px);`, `.box;`                 | `@extend .box;`                  |
//! | `.a:extend(.b all) { }`, `&:extend(.b);` | `.a { @extend .b; }`, `@extend .b;` |
//! | `~"calc(1 + 2)"`                      | `"calc(1 + 2)"`                  |
//! | `@import (reference) "x";`            | `@import "x";`                   |
//!
//! A mixin is a class that can be called, so it is the definition of its selector (`.box`) and a call is a use of
//! it, the way `@extend` is. The variables get their `@` back after the file is read, so that `@gap` is the name
//! to look for. The parameters of a mixin are not told from variables: a parameter named like a variable of the
//! file is read as a use of that variable.

use tree_sitter::Parser;

use super::{Extract, scss};

/// Lines an open `(` may reach over before it is taken for a stray one.
const MAX_PAREN_LINES: usize = 64;

/// The most `:extend(...)` a selector is searched for; a head of thousands of them is not read for the rest.
const MAX_EXTENDS: usize = 16;

/// How far an `@{...}` or `#{...}` is looked for.
const INTERPOLATION_REACH: usize = 256;

/// The at-rules of CSS. Any other `@word` is a variable.
const AT_KEYWORDS: &[&str] = &[
    "media",
    "import",
    "charset",
    "font-face",
    "keyframes",
    "supports",
    "namespace",
    "page",
    "document",
    "plugin",
    "viewport",
    "container",
    "layer",
    "property",
    "counter-style",
    "font-feature-values",
    "scope",
    "starting-style",
    "extend",
];

/// Reads a `.less` file.
pub(super) fn extract(parser: &mut Parser, path: &str, src: &str, out: &mut Extract) {
    scss::extract(parser, path, &to_scss(src), out);
    restore_variables(out);
    name_namespaced_mixins(out);
    super::sass::show_as_written(out, super::sass::Spelling::Less);
}

/// The SCSS for the Less `src`, with as many lines.
pub(super) fn to_scss(src: &str) -> String {
    let src = src.strip_prefix('\u{feff}').unwrap_or(src);

    restructure(&rename(src))
}

/// Gives the variables the names they have in Less: after the read, `$gap` is `@gap` again, in its name and in
/// its uses.
fn restore_variables(out: &mut Extract) {
    for sym in &mut out.syms {
        if let Some(rest) = sym.name.strip_prefix('$') {
            sym.name = format!("@{rest}");
        }
    }

    for r in &mut out.refs {
        if let Some(rest) = r.name.strip_prefix('$') {
            r.name = format!("@{rest}");
        }
    }
}

/// Gives a mixin of an id namespace (`#ns { .m() { } }`, read as the selector `#ns .m`) the plain name its calls
/// are written with too: `.m();`, `#ns > .m();` and `#ns.m();` all use `.m`, and the namespace is no part of a
/// call. The definition is added at the same lines as the one under the full name, so both are found at the
/// mixin; a namespace that holds a class of the same name as another makes the plain name ambiguous, which is true.
fn name_namespaced_mixins(out: &mut Extract) {
    let mut plain = Vec::new();

    for (index, sym) in out.syms.iter().enumerate() {
        if sym.kind != "class" {
            continue;
        }

        let mut parts = sym.name.split(' ').rev();
        let Some(last) = parts.next() else {
            continue;
        };
        let owners: Vec<&str> = parts.collect();
        let namespaced = !owners.is_empty() && owners.iter().all(|owner| is_id_selector(owner));

        if namespaced && is_class_selector(last) {
            plain.push((index, last.to_string()));
        }
    }

    for (index, name) in plain {
        let Some(sym) = out.syms.get(index) else {
            continue;
        };
        let mut alias = sym.clone();

        alias.sig.clone_from(&name);
        alias.name = name;
        out.add_sym(alias);
    }
}

/// True for `#name`.
fn is_id_selector(part: &str) -> bool {
    part.strip_prefix('#')
        .is_some_and(|name| !name.is_empty() && name.bytes().all(is_word))
}

/// True for `.name`.
fn is_class_selector(part: &str) -> bool {
    part.strip_prefix('.')
        .is_some_and(|name| !name.is_empty() && name.bytes().all(is_word))
}

/// True for a character of a name.
fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
}

/// The offset after the string that starts at `at`: a string ends with its line at the latest.
fn string_end(bytes: &[u8], at: usize) -> usize {
    let quote = bytes.get(at).copied().unwrap_or(b'"');
    let mut cursor = at + 1;

    while let Some(&byte) = bytes.get(cursor) {
        match byte {
            b'\\' => cursor += 2,
            b'\n' => return cursor,
            b if b == quote => return cursor + 1,
            _ => cursor += 1,
        }
    }

    bytes.len().min(cursor)
}

/// The offset of the end of the line that `at` is on (the line break itself is not part of it).
fn line_end(bytes: &[u8], at: usize) -> usize {
    bytes
        .iter()
        .skip(at)
        .position(|b| *b == b'\n')
        .map_or(bytes.len(), |end| at + end)
}

/// The offset after the block comment that starts at `at`.
fn comment_end(text: &str, at: usize) -> usize {
    text.get(at + 2..)
        .and_then(|rest| rest.find("*/"))
        .map_or(text.len(), |end| at + end + 4)
}

/// The offset after an unquoted `url(...)` whose `(` is at `open`; `None` when it is not one.
fn url_end(text: &str, open: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let name = text.get(open.checked_sub(3)?..open)?;

    if !name.eq_ignore_ascii_case("url") {
        return None;
    }

    let rest = text.get(open + 1..)?;

    if rest.trim_start().starts_with(['"', '\'']) {
        return None;
    }

    let end = bytes
        .iter()
        .skip(open)
        .position(|b| *b == b')' || *b == b'\n')
        .map_or(bytes.len(), |end| open + end);

    Some(end + usize::from(bytes.get(end) == Some(&b')')))
}

/// The offset after the `{...}` of an interpolation (`@{x}`, `#{$x}`) whose `{` is at `open`, when it closes on
/// the same line within [`INTERPOLATION_REACH`] bytes.
fn interpolation_end(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;

    for (offset, byte) in bytes.iter().enumerate().skip(open).take(INTERPOLATION_REACH) {
        match byte {
            b'\n' => return None,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;

                if depth == 0 {
                    return Some(offset + 1);
                }
            }
            _ => {}
        }
    }

    None
}

/// The names of Less in the spelling of SCSS: variables, interpolation and escaped strings. Only the characters of
/// a name change, so no line break moves. A string and an `url(...)` are changed in their `@{...}` only, a comment
/// not at all.
fn rename(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(src.len() + src.len() / 8);
    let mut at = 0;

    while let Some(&byte) = bytes.get(at) {
        match byte {
            b'"' | b'\'' => {
                let end = string_end(bytes, at);

                interpolate(bytes.get(at..end).unwrap_or_default(), &mut out);
                at = end;
            }
            b'~' if matches!(bytes.get(at + 1), Some(b'"' | b'\'')) => at += 1,
            b'/' if bytes.get(at + 1) == Some(&b'/') => {
                let end = line_end(bytes, at);

                out.extend(bytes.get(at..end).unwrap_or_default());
                at = end;
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                let end = comment_end(src, at);

                out.extend(bytes.get(at..end).unwrap_or_default());
                at = end;
            }
            b'(' if url_end(src, at).is_some() => {
                let end = url_end(src, at).unwrap_or(at + 1);

                interpolate(bytes.get(at..end).unwrap_or_default(), &mut out);
                at = end;
            }
            b'\\' => {
                out.extend(
                    bytes
                        .get(at..at + 2)
                        .unwrap_or_else(|| bytes.get(at..).unwrap_or_default()),
                );
                at += 2;
            }
            b'@' => at = variable(bytes, at, &mut out),
            _ => {
                out.push(byte);
                at += 1;
            }
        }
    }

    String::from_utf8(out).unwrap_or_else(|_| src.to_string())
}

/// Copies `piece` (a string or an `url(...)`), reading `@{name}` as `#{$name}`.
fn interpolate(piece: &[u8], out: &mut Vec<u8>) {
    let mut at = 0;

    while let Some(&byte) = piece.get(at) {
        let name = (byte == b'@' && piece.get(at + 1) == Some(&b'{')).then(|| interpolated_name(piece, at));

        match name.flatten() {
            Some((name, end)) => {
                out.extend(b"#{$");
                out.extend(name);
                out.push(b'}');
                at = end;
            }
            None => {
                out.push(byte);
                at += 1;
            }
        }
    }
}

/// The name in the `@{name}` that starts at `at` and the offset after it.
fn interpolated_name(bytes: &[u8], at: usize) -> Option<(&[u8], usize)> {
    let name_start = at + 2;
    let length = bytes.iter().skip(name_start).take_while(|b| is_word(**b)).count();
    let end = name_start + length;

    (length > 0 && bytes.get(end) == Some(&b'}')).then(|| (bytes.get(name_start..end).unwrap_or_default(), end + 1))
}

/// Writes what the `@` at `at` is in SCSS and gives the offset after it. `@{x}` is `#{$x}`, `@@x` and `@x` are
/// `$x`; the at-rules of CSS stay.
fn variable(bytes: &[u8], at: usize, out: &mut Vec<u8>) -> usize {
    if bytes.get(at + 1) == Some(&b'{') {
        return match interpolated_name(bytes, at) {
            Some((name, end)) => {
                out.extend(b"#{$");
                out.extend(name);
                out.push(b'}');

                end
            }
            None => {
                out.push(b'@');

                at + 1
            }
        };
    }

    // `@@name` is the variable that `@name` names; the index takes the variable it is written with
    let skipped = usize::from(bytes.get(at + 1) == Some(&b'@'));
    let start = at + 1 + skipped;
    let length = bytes.iter().skip(start).take_while(|b| is_word(**b)).count();
    let named = bytes
        .get(start)
        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_' || *b == b'-');

    if length == 0 || !named {
        out.push(b'@');

        return at + 1;
    }

    let name = bytes.get(start..start + length).unwrap_or_default();

    if skipped == 0 && is_at_keyword(name) {
        out.push(b'@');

        return at + 1;
    }

    out.push(b'$');
    out.extend(name);

    start + length
}

/// True when `@name` is an at-rule of CSS (with or without a vendor prefix) and no variable.
fn is_at_keyword(name: &[u8]) -> bool {
    let name = String::from_utf8_lossy(name).to_ascii_lowercase();
    let bare = ["-webkit-", "-moz-", "-ms-", "-o-"]
        .iter()
        .find_map(|prefix| name.strip_prefix(prefix))
        .unwrap_or(&name);

    AT_KEYWORDS.contains(&bare)
}

/// The block structure of Less in the spelling of SCSS: a rule's head loses its parameters, guard and `:extend`,
/// a mixin call becomes a use of the class, and `@import` loses its options. The text is cut into statements at
/// `;`, `{` and `}` outside strings, comments and parentheses.
fn restructure(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len() + text.len() / 8);
    let mut start = 0;
    let mut at = 0;
    let mut depth = 0usize;
    let mut paren_lines = 0;
    // per open `{`: true for a map or detached ruleset (`@config: { ... }`), which SCSS ends with a `;`
    let mut variable_blocks: Vec<bool> = Vec::new();

    while let Some(&byte) = bytes.get(at) {
        match byte {
            b'"' | b'\'' => {
                at = string_end(bytes, at);
                continue;
            }
            b'/' if bytes.get(at + 1) == Some(&b'/') => {
                at = line_end(bytes, at);
                continue;
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                at = comment_end(text, at);
                continue;
            }
            b'\\' => {
                at += 2;
                continue;
            }
            b'#' if bytes.get(at + 1) == Some(&b'{') => {
                if let Some(end) = interpolation_end(bytes, at + 1) {
                    at = end;
                    continue;
                }
            }
            b'(' => {
                if let Some(end) = url_end(text, at) {
                    at = end;
                    continue;
                }

                depth += 1;
            }
            b')' => depth = depth.saturating_sub(1),
            b'\n' if depth > 0 => {
                paren_lines += 1;

                // a parenthesis that reaches over this many lines is a stray one
                if paren_lines > MAX_PAREN_LINES {
                    depth = 0;
                    paren_lines = 0;
                }
            }
            b';' | b'{' | b'}' if depth == 0 => {
                let segment = text.get(start..at).unwrap_or_default();

                statement(segment, Some(byte), &mut out);

                match byte {
                    b'{' => variable_blocks.push(is_variable_head(segment)),
                    // the block of a variable ends like a declaration: without the `;` the rule after it would be
                    // read as part of it
                    b'}' if variable_blocks.pop().unwrap_or(false) => out.push(';'),
                    _ => {}
                }

                start = at + 1;
            }
            _ => {}
        }

        if depth == 0 {
            paren_lines = 0;
        }

        at += 1;
    }

    statement(text.get(start..).unwrap_or_default(), None, &mut out);

    out
}

/// True when `segment`, the text before a `{`, is only a variable and its colon (`$config:`): the block that
/// follows is its value, a map or a detached ruleset.
fn is_variable_head(segment: &str) -> bool {
    let shape = without_comments(segment);
    let head = shape.trim();

    head.strip_prefix('$')
        .and_then(|rest| rest.strip_suffix(':'))
        .is_some_and(|name| !name.is_empty() && name.bytes().all(is_word))
}

/// Writes one statement, `segment` (what is before its `delimiter`), and the delimiter.
fn statement(segment: &str, delimiter: Option<u8>, out: &mut String) {
    let lead = trivia_len(segment);
    let body = segment.get(lead..).unwrap_or_default();
    let shape = without_comments(body);
    let end = shape.trim_end().len();
    let core = body.get(..end).unwrap_or_default();
    let trailing = body.get(end..).unwrap_or_default();

    out.push_str(segment.get(..lead).unwrap_or_default());

    let mut after = String::new();
    let head = match delimiter {
        _ if core.is_empty() => String::new(),
        Some(b'{') => rule_head(core, &shape, &mut after),
        _ => call_or_import(core, &shape, delimiter),
    };

    out.push_str(&head);
    out.push_str(trailing);

    if let Some(delimiter) = delimiter {
        out.push(char::from(delimiter));
    }

    out.push_str(&after);

    // what was cut out of the head had line breaks: they are put back after it
    let kept = head.matches('\n').count() + after.matches('\n').count();

    for _ in kept..core.matches('\n').count() {
        out.push('\n');
    }
}

/// The length of the white space and comments the text starts with.
fn trivia_len(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut at = 0;

    loop {
        while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }

        if bytes.get(at..at + 2) == Some(b"//") {
            at = line_end(bytes, at);
        } else if bytes.get(at..at + 2) == Some(b"/*") {
            at = comment_end(text, at);
        } else {
            return at;
        }
    }
}

/// `text` with every comment replaced by spaces (line breaks stay), so that offsets are the same in both.
fn without_comments(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut shape = bytes.to_vec();
    let mut at = 0;

    while let Some(&byte) = bytes.get(at) {
        let end = match byte {
            b'"' | b'\'' => {
                at = string_end(bytes, at);
                continue;
            }
            b'/' if bytes.get(at + 1) == Some(&b'/') => line_end(bytes, at),
            b'/' if bytes.get(at + 1) == Some(&b'*') => comment_end(text, at),
            _ => {
                at += 1;
                continue;
            }
        };

        for slot in shape.iter_mut().take(end).skip(at).filter(|b| **b != b'\n') {
            *slot = b' ';
        }

        at = end;
    }

    String::from_utf8(shape).unwrap_or_else(|_| text.to_string())
}

/// The offset of the `)` that closes the `(` at `open`, outside strings.
fn closing_paren(shape: &str, open: usize) -> Option<usize> {
    let bytes = shape.as_bytes();
    let mut depth = 0usize;
    let mut at = open;

    while let Some(&byte) = bytes.get(at) {
        match byte {
            b'"' | b'\'' => {
                at = string_end(bytes, at);
                continue;
            }
            b'(' => depth += 1,
            b')' => {
                depth = depth.checked_sub(1)?;

                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }

        at += 1;
    }

    None
}

/// The head of a rule: `.box(@p) when (@p > 1)` is `.box`, `.a:extend(.b all)` is `.a` and puts `@extend .b;` into
/// `after`, which is written inside the rule.
fn rule_head(core: &str, shape: &str, after: &mut String) -> String {
    // a guard ends the head
    let guarded = guard_start(shape).unwrap_or(core.len());
    let mut head = String::with_capacity(guarded);
    let mut kept = 0;
    let mut extended = Vec::new();

    while let Some(found) = shape
        .get(kept..guarded)
        .and_then(|rest| rest.find(":extend("))
        .filter(|_| extended.len() < MAX_EXTENDS)
    {
        let open = kept + found + ":extend".len();
        let Some(close) = closing_paren(shape, open).filter(|close| *close < guarded) else {
            break;
        };

        head.push_str(core.get(kept..kept + found).unwrap_or_default());
        extended.push(extend_targets(core.get(open + 1..close).unwrap_or_default()));
        kept = close + 1;
    }

    head.push_str(core.get(kept..guarded).unwrap_or_default());

    if !extended.is_empty() {
        after.push_str(&format!(" @extend {};", extended.join(", ")));
    }

    let head = head.trim_end();

    match mixin_name_end(&without_comments(head)) {
        Some(end) => head.get(..end).unwrap_or(head).to_string(),
        None => head.to_string(),
    }
}

/// The offset of the `when` that starts a guard, at the top level of the head.
fn guard_start(shape: &str) -> Option<usize> {
    let bytes = shape.as_bytes();
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
            b'w' if depth == 0 && bytes.get(at..at + 4) == Some(b"when") => {
                let before = at.checked_sub(1).and_then(|before| bytes.get(before));
                let after = bytes.get(at + 4);

                if before.is_some_and(u8::is_ascii_whitespace)
                    && after.is_none_or(|b| b.is_ascii_whitespace() || *b == b'(')
                {
                    return Some(at);
                }
            }
            _ => {}
        }

        at += 1;
    }

    None
}

/// `.b all, .c` -> `.b, .c`: what an `:extend(...)` extends.
fn extend_targets(args: &str) -> String {
    args.split(',')
        .map(|target| {
            let target = target.trim();

            target.strip_suffix(" all").unwrap_or(target).trim()
        })
        .filter(|target| !target.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

/// For a head that is a whole mixin definition (`.name(...)`, nothing after the parentheses), where its name ends.
/// Parentheses elsewhere in a selector (`:not(.a)`, `.a(b) .c`) are not parameters.
fn mixin_name_end(shape: &str) -> Option<usize> {
    let bytes = shape.as_bytes();

    if !matches!(bytes.first(), Some(b'.' | b'#')) {
        return None;
    }

    let name = bytes.iter().skip(1).take_while(|b| is_word(**b)).count();
    let end = 1 + name;
    let open = end + bytes.iter().skip(end).take_while(|b| b.is_ascii_whitespace()).count();

    if name == 0 || bytes.get(open) != Some(&b'(') {
        return None;
    }

    let close = closing_paren(shape, open)?;

    shape.get(close + 1..)?.trim().is_empty().then_some(end)
}

/// A statement that is no declaration: `.box(2px)` and `.box` call a mixin, read as an `@extend` of its class;
/// `&:extend(.b)` extends; `@import (reference) "x"` loses its options. Anything else is written as it is.
fn call_or_import(core: &str, shape: &str, delimiter: Option<u8>) -> String {
    let closed = |rewritten: String| {
        if delimiter == Some(b';') {
            rewritten
        } else {
            format!("{rewritten};")
        }
    };

    if let Some(rest) = shape.strip_prefix("@import") {
        let options = rest.trim_start();
        let open = shape.len() - options.len();

        if options.starts_with('(')
            && let Some(close) = closing_paren(shape, open)
        {
            return format!(
                "@import{}{}",
                core.get("@import".len()..open).unwrap_or_default(),
                core.get(close + 1..).unwrap_or_default().trim_start()
            );
        }

        return core.to_string();
    }

    if shape.ends_with(')')
        && let Some(found) = shape.rfind(":extend(")
        && closing_paren(shape, found + ":extend".len()) == Some(shape.len() - 1)
    {
        let args = core.get(found + ":extend(".len()..shape.len() - 1).unwrap_or_default();
        let padding = "\n".repeat(core.get(..found).unwrap_or_default().matches('\n').count());

        return closed(format!("{padding}@extend {}", extend_targets(args)));
    }

    match called_class(shape) {
        Some((from, to)) => {
            let padding = "\n".repeat(core.get(..from).unwrap_or_default().matches('\n').count());
            let tail = core.get(to..).unwrap_or_default().matches('\n').count();

            closed(format!(
                "{padding}@extend {}{}",
                core.get(from..to).unwrap_or_default(),
                "\n".repeat(tail)
            ))
        }
        None => merged_property(core).unwrap_or_else(|| core.to_string()),
    }
}

/// A declaration that merges into a property of the same name (`border+: 1px`, `border+_: 1px`) without the `+`
/// or `+_`, which SCSS has no word for; `None` for any other statement. Only a statement that ends a
/// declaration gets here, so a selector such as `li+:first-child` is never one.
fn merged_property(core: &str) -> Option<String> {
    let name_len = core.bytes().take_while(|b| is_word(*b)).count();
    let name = core
        .get(..name_len)
        .filter(|name| name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '-'))?;
    let after = core.get(name_len..)?.strip_prefix('+')?;
    let after = after.strip_prefix('_').unwrap_or(after);

    after.trim_start().starts_with(':').then(|| format!("{name}{after}"))
}

/// For a statement that is a mixin call (`.box`, `.box(1px, 2px)`, `#ns > .box() !important`), the class it calls:
/// the last name before the arguments.
fn called_class(shape: &str) -> Option<(usize, usize)> {
    let bytes = shape.as_bytes();
    let skip_space = |mut at: usize| {
        while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }

        at
    };
    let mut at = skip_space(0);
    let mut called = None;

    loop {
        if !matches!(bytes.get(at), Some(b'.' | b'#')) {
            break;
        }

        let name = bytes.iter().skip(at + 1).take_while(|b| is_word(**b)).count();
        let starts_a_name = bytes
            .get(at + 1)
            .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_' || *b == b'-');

        if name == 0 || !starts_a_name {
            return None;
        }

        called = Some((at, at + 1 + name));
        at = skip_space(at + 1 + name);

        match bytes.get(at) {
            Some(b'>') => at = skip_space(at + 1),
            Some(b'.' | b'#') => {}
            _ => break,
        }
    }

    if bytes.get(at) == Some(&b'(') {
        at = closing_paren(shape, at)? + 1;
    }

    at = skip_space(at);

    if shape.get(at..)?.starts_with("!important") {
        at = skip_space(at + "!important".len());
    }

    (at == shape.len()).then_some(called?)
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::code::extract::FILE_LIMIT;

    fn run(src: &str) -> Extract {
        super::super::extract(&mut Parser::new(), "a.less", src)
    }

    fn defs(out: &Extract) -> Vec<(&str, &str, usize)> {
        out.syms.iter().map(|s| (s.kind, s.name.as_str(), s.line)).collect()
    }

    fn used(out: &Extract) -> Vec<(&str, &str, usize)> {
        out.refs.iter().map(|r| (r.kind, r.name.as_str(), r.line)).collect()
    }

    fn lines(text: &str) -> usize {
        text.matches('\n').count()
    }

    #[test]
    fn a_map_or_a_detached_ruleset_and_a_merging_property_do_not_swallow_the_rules_after_them() {
        let map = run("@config: { key1: value; key2: 2 }\n.a { top: 0; }\n");

        assert!(defs(&map).contains(&("class", ".a", 2)), "{:?}", defs(&map));

        let detached = run("@detached: {\n  .inner { top: 0; }\n}\n.b { top: 0; }\n");

        assert!(defs(&detached).contains(&("class", ".b", 4)), "{:?}", defs(&detached));
        assert!(
            defs(&detached).contains(&("class", ".inner", 2)),
            "{:?}",
            defs(&detached)
        );

        for merge in ["+", "+_"] {
            let out = run(&format!(".k {{ border{merge}: 1px; }}\n.z {{ top: 0; }}\n"));
            let found = defs(&out);

            assert!(found.contains(&("class", ".k", 1)), "{merge}: {found:?}");
            assert!(found.contains(&("class", ".z", 2)), "{merge}: {found:?}");
            assert!(!found.iter().any(|d| d.1.contains(".k .z")), "{merge}: {found:?}");
        }

        assert_eq!(lines(&to_scss("@c: {\n k: 1;\n}\n.a { border+_: 1px; }\n")), 4);
    }

    #[test]
    fn every_construct_keeps_its_line() {
        let src = "\
@import (reference) \"base\";
@gap: 4px;
@w: @gap * 2;
.box(@p: 1px; @q: 2px) when (@p > 0) {
  padding: @p;
}
.card:extend(.box all) {
  margin: @gap;
  .box(2px);
  &:extend(.base);
  &-on { top: 0; }
  .title { width: ~\"calc(100% - @{gap})\"; }
}
.@{name}-x { top: 0; }
@media @phone {
  .wide { top: 0; }
}
@keyframes spin { from { top: 0; } }
.btn { animation: spin 1s ease-in infinite; }
";
        let out = run(src);
        let found = defs(&out);

        for wanted in [
            ("const", "@gap", 2),
            ("const", "@w", 3),
            ("class", ".box", 4),
            ("class", ".card", 7),
            ("class", ".card-on", 11),
            ("class", ".card .title", 12),
            ("class", ".#{$name}-x", 14),
            ("class", ".wide", 16),
            ("const", "spin", 18),
            ("class", ".btn", 19),
        ] {
            assert!(found.contains(&wanted), "{wanted:?} in {found:?}");
        }

        let refs = used(&out);

        for wanted in [
            ("use", "@gap", 3),
            ("use", "@gap", 8),
            ("call", ".box", 9),
            ("call", ".base", 10),
            ("call", ".box", 7),
            ("use", "spin", 19),
        ] {
            assert!(refs.contains(&wanted), "{wanted:?} in {refs:?}");
        }

        assert_eq!(
            out.syms.iter().find(|s| s.name == "@gap").map(|s| s.sig.as_str()),
            Some("@gap: 4px;")
        );
        assert!(out.imports.iter().any(|i| i.local == "sheet:base"), "{:?}", out.imports);
    }

    #[test]
    fn the_rewrites_of_a_fragment_table_keep_the_lines() {
        let table = [
            ("@a: 1;\n@b: @a;\n", "$a: 1;\n$b: $a;\n"),
            ("@@name\n", "$name\n"),
            (".a { b: @{c}; }\n", ".a { b: #{$c}; }\n"),
            (
                ".a { b: \"x @{c} y\"; d: 'z @e'; }\n",
                ".a { b: \"x #{$c} y\"; d: 'z @e'; }\n",
            ),
            (".a { b: url(@{base}/x.png); }\n", ".a { b: url(#{$base}/x.png); }\n"),
            (".m(\n  @a,\n  @b\n) {\n  c: d;\n}\n", ".m {\n\n\n\n  c: d;\n}\n"),
            (".m() when (default()) { a: b }\n", ".m { a: b }\n"),
            (".a:not(.b) { c: d }\n", ".a:not(.b) { c: d }\n"),
            (".a:extend(.b, .c all) { d: e }\n", ".a { @extend .b, .c; d: e }\n"),
            (".x { .m(1; 2); }\n", ".x { @extend .m; }\n"),
            (".x { .m }\n", ".x { @extend .m; }\n"),
            (".x { #ns > .m() !important; }\n", ".x { @extend .m; }\n"),
            (".x { color: ~'red'; }\n", ".x { color: 'red'; }\n"),
            ("@import (css, optional) url(\"a.css\");\n", "@import url(\"a.css\");\n"),
            ("@media screen { .a { b: c } }\n", "@media screen { .a { b: c } }\n"),
            ("// @a { not code\n.a { b: c }\n", "// @a { not code\n.a { b: c }\n"),
            ("/* @a;\n{ */ .a { b: c }\n", "/* @a;\n{ */ .a { b: c }\n"),
            ("@c: { k: v; }\n.a { b: c }\n", "$c: { k: v; };\n.a { b: c }\n"),
            ("@d: {\n  .i { b: c }\n}\n", "$d: {\n  .i { b: c }\n};\n"),
            (".a { border+: 1px; b+_ : 2px }\n", ".a { border: 1px; b : 2px }\n"),
            ("li+:first-child { b: c }\n", "li+:first-child { b: c }\n"),
        ];

        for (less, wanted) in table {
            let scss = to_scss(less);

            assert_eq!(scss, wanted, "{less:?}");
            assert_eq!(lines(&scss), lines(less), "{less:?}");
        }
    }

    #[test]
    fn a_mixin_is_the_class_it_is_called_by() {
        let out = run(".box(@p) {\n  padding: @p;\n}\n.a {\n  .box(1px);\n  .box;\n}\n");

        assert_eq!(defs(&out), [("class", ".box", 1), ("class", ".a", 4)]);
        assert_eq!(
            used(&out)
                .iter()
                .filter(|r| r.1 == ".box")
                .map(|r| r.2)
                .collect::<Vec<_>>(),
            [5, 6]
        );
    }

    #[test]
    fn variables_keep_their_at_sign_in_names_uses_and_signatures() {
        let out = run("@gap: 4px;\n.a {\n  margin: @gap;\n  @local: 1;\n}\n@w: @gap;\n");
        let names: Vec<_> = out.syms.iter().map(|s| (s.name.as_str(), s.sig.as_str())).collect();

        assert_eq!(names, [("@gap", "@gap: 4px;"), (".a", ".a"), ("@w", "@w: @gap;")]);
        assert!(out.refs.iter().all(|r| !r.name.starts_with('$')), "{:?}", out.refs);
        assert_eq!(out.refs.iter().filter(|r| r.name == "@gap").count(), 2);
    }

    #[test]
    fn hostile_text_keeps_its_lines() {
        let sources = [
            String::new(),
            "(".repeat(1_000_000),
            "{".repeat(100_000),
            "@{".repeat(100_000),
            "#{".repeat(100_000),
            ".a(".repeat(50_000),
            "\"unterminated\n'also\n/* never closed\n".to_string(),
            ".a {\r\n  b: c;\r\n}\r\n".to_string(),
            "\u{feff}@a: 1;\n".to_string(),
            "\t \t.a\t{ \tb:\t@c }\n".to_string(),
            "é: ü;\n.日本 { ñ: \"🙂 @{x}\" }\n".to_string(),
            "@@@@x ~~~\"a\" @ @{ @{} @{a-}\n".to_string(),
            ".a:extend(".repeat(10_000),
            ".a:extend(.b) ".repeat(10_000),
            ".a when ".repeat(10_000),
            "(\n".repeat(10_000),
            "url(".repeat(100_000),
            "~\"\n".repeat(10_000),
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
        let block = ".box(@p: 1px) when (@p > 0) {\n  .card:extend(.a) { margin: @gap; .m(1, (2)); }\n}\n";
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

    #[test]
    fn a_mixin_of_an_id_namespace_is_found_by_the_plain_name_its_calls_use() {
        let out = run(
            "#ns {\n  .m() { top: 0 }\n  #deep { .n() { top: 1 } }\n}\n.a {\n  .m();\n  #ns > .m();\n  #ns.m();\n}\n.plain { .inner { top: 2 } }\n",
        );

        assert_eq!(
            defs(&out),
            [
                ("class", "#ns", 1),
                ("class", "#ns .m", 2),
                ("class", "#ns #deep", 3),
                ("class", "#ns #deep .n", 3),
                ("class", ".a", 5),
                ("class", ".plain", 10),
                ("class", ".plain .inner", 10),
                ("class", ".m", 2),
                ("class", ".n", 3),
            ]
        );
        assert_eq!(used(&out), [("call", ".m", 6), ("call", ".m", 7), ("call", ".m", 8)]);
    }
}
