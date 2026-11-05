//! Single-file components (Svelte, Vue, Astro, ...): one generic handler, no framework knowledge.
//!
//! The file is a `component` symbol named after the file. Its `<script>` blocks (and an Astro-style `---`
//! frontmatter) are parsed as TypeScript. The rest of the file (the template) is scanned for component tags
//! (`<FooBar>`, `<Foo.Bar>`, `<foo-bar>`) and for quoted code names (`invoke('git_file_lines')`). The names the
//! script imports are also looked for in the template's expressions (see `sfc_template`).
//!
//! The class attributes of the template (`class`, `:class`, `class:foo`, `class:list`) are `cls` references of the
//! component (see `markup`); the stylesheet modules the script imports are honoured there (`styles.card`).
//!
//! The script, style and frontmatter regions are blanked in the template copy byte for byte (one ASCII space
//! per byte, newlines kept), so offsets and line numbers of the template never shift, whatever the text holds.
//!
//! A `<style>` block is a stylesheet: its `lang` (or `type`) picks the reader (`css`, `postcss` and `scss` as
//! they are, `sass` and `less` through their converters; `stylus` and anything unknown is left unread), and what
//! it defines and uses is marked as written in a style block, so the graph reads it by the rules of
//! stylesheets and not by those of the script around it. A block owns no definition: its rules stand on their
//! own.

use std::ops::Range;
use std::path::Path;

use tree_sitter::Parser;

use super::classes::sheet_locals;
use super::markup::Dialect;
use super::{DROPPED, Extract, Import, Sym, ident_like, out_of_time, push_ref};

/// The template scan looks at the clock this often, in bytes.
pub(super) const SCAN_CHECK: usize = 1 << 16;

/// A quoted string longer than this is not a code name.
const MAX_QUOTED: usize = 80;

/// The style blocks of one file that are read. A generated file with thousands of them would parse each in
/// turn; the rest are left out and counted in [`Extract::dropped`].
const MAX_STYLE_BLOCKS: usize = 16;

/// Reads a single-file component.
pub(super) fn extract(parser: &mut Parser, path: &str, src: &str, out: &mut Extract) {
    let stem = Path::new(path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or("component")
        .to_string();
    let component = out.add_sym(Sym {
        name: stem.clone(),
        kind: "component",
        parent: None,
        line: 1,
        end_line: src.lines().count().max(1),
        sig: format!("<{stem}>"),
        ret: None,
        test: false,
        style: false,
    });

    let owner = Some(component);
    let language = super::ts::typescript();
    let mut markup = src.as_bytes().to_vec();

    if let Some((code, region)) = frontmatter(src) {
        super::ts::extract_block(parser, &language, code, 1, owner, false, out);
        blank(&mut markup, region);
    }

    // the blocks come in order, so the lines before each one are counted once, not from the top every time
    let mut counted = (0_usize, 0_usize);

    for block in blocks(src, "script") {
        // every block is a parse of its own: the file's time is checked between them. A script that is not read
        // may hold class uses (`styles.card`, `clsx('a')`), so the count of its stylesheets may be low.
        if out_of_time() {
            out.dropped += 1;
            out.classes_cut = true;

            break;
        }

        let code = src.get(block.body.clone()).unwrap_or("");
        let between = src
            .as_bytes()
            .get(counted.0..block.body.start)
            .map_or(0, count_newlines);
        let line_off = counted.1 + between;

        counted = (block.body.start, line_off);

        let first = out.refs.len();

        super::ts::extract_block(parser, &language, code, line_off, owner, false, out);

        // the rows of the opening and the closing tag hold what is written around the script (`<p>text</p><script>f()`),
        // so the line of a reference there is no line of code
        let opening = line_off + 1;
        let closing = line_off + count_newlines(code.as_bytes()) + 1;

        for reference in out.refs.iter_mut().skip(first) {
            if reference.line == opening || reference.line == closing {
                reference.markup = true;
            }
        }

        blank(&mut markup, block.whole);
    }

    let styles = blocks(src, "style");

    for block in &styles {
        blank(&mut markup, block.whole.clone());
    }

    // everything the three readers of the template find is written in markup, where the line of source holds
    // the text of the page around it: the references are marked, the script blocks above and the styles below
    // are not
    out.in_markup = true;
    scan_template(&markup, owner, out);

    let dialect = Dialect::of(path);

    super::sfc_template::extract(&markup, dialect == Dialect::Vue, owner, out);

    // a Vue template reaches the module of its `<style module>` as `$style`
    let mut sheets = sheet_locals(&out.imports);

    if dialect == Dialect::Vue {
        sheets.insert("$style".to_string());
    }

    super::markup::extract(parser, &markup, dialect, &sheets, owner, out);
    out.in_markup = false;
    super::ts::keep_function_values(out);

    // the styles come last: nothing the script's passes do above sees a style definition or reference
    read_styles(parser, src, &styles, out);
}

/// The language of a style block.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StyleLang {
    Scss,
    Sass,
    Less,
}

/// The reader of a `<style ...>` opening tag: `lang="scss"` or `type="text/less"`. A block with no attribute
/// is CSS; Stylus and anything else is not read.
fn style_lang(opening: &str) -> Option<StyleLang> {
    let bytes = opening.as_bytes();
    let mut lang: Option<&str> = None;
    let mut kind: Option<&str> = None;
    let mut at = "<style".len();

    while let Some(&byte) = bytes.get(at) {
        if byte.is_ascii_whitespace() || byte == b'/' || byte == b'>' {
            at += 1;

            continue;
        }

        let name_start = at;

        while bytes
            .get(at)
            .is_some_and(|b| !b.is_ascii_whitespace() && !matches!(b, b'=' | b'>' | b'/'))
        {
            at += 1;
        }

        let name = opening.get(name_start..at).unwrap_or("");

        if bytes.get(at) != Some(&b'=') {
            continue;
        }

        at += 1;

        let (value, next) = match bytes.get(at) {
            Some(&quote) if quote == b'"' || quote == b'\'' => {
                let close = bytes
                    .get(at + 1..)
                    .and_then(|rest| rest.iter().position(|b| *b == quote))
                    .map_or(bytes.len(), |end| at + 1 + end);

                (opening.get(at + 1..close).unwrap_or(""), close + 1)
            }
            _ => {
                let end = bytes
                    .get(at..)
                    .and_then(|rest| rest.iter().position(|b| b.is_ascii_whitespace() || *b == b'>'))
                    .map_or(bytes.len(), |end| at + end);

                (opening.get(at..end).unwrap_or(""), end)
            }
        };

        if name.eq_ignore_ascii_case("lang") {
            lang = Some(value);
        } else if name.eq_ignore_ascii_case("type") {
            kind = Some(value);
        }

        at = next;
    }

    // `lang` wins over `type`, whichever comes first
    let value = lang.or(kind).unwrap_or("css").trim().to_ascii_lowercase();

    match value.rsplit('/').next().unwrap_or("") {
        "" | "css" | "postcss" | "pcss" | "scss" => Some(StyleLang::Scss),
        "sass" => Some(StyleLang::Sass),
        "less" => Some(StyleLang::Less),
        _ => None,
    }
}

/// Reads the style `blocks` of `src` as stylesheets into `out`. Each is read apart from the file and then
/// appended, so what the script produced keeps its indexes and a converter's name restoration (Less variables
/// get their `@` back) cannot reach a script's definitions.
///
/// A style block that is not read (past [`MAX_STYLE_BLOCKS`], out of time, not parsed) loses selectors, never class
/// uses: a block holds no `cls` reference, so [`Extract::classes_cut`] is not set for it. The selector is simply
/// absent, and no count of users is shown for it.
pub(super) fn read_styles(parser: &mut Parser, src: &str, blocks: &[Block], out: &mut Extract) {
    let mut read = 0;
    let mut counted = (0_usize, 0_usize);

    for block in blocks {
        let body = src.get(block.body.clone()).unwrap_or("");

        // an empty block has nothing to read and takes no share of the cap
        if body.trim().is_empty() {
            continue;
        }

        let opening = src.get(block.whole.start..block.body.start).unwrap_or("");
        let Some(lang) = style_lang(opening) else {
            continue;
        };

        if read >= MAX_STYLE_BLOCKS {
            out.dropped += 1;
            out.selectors_cut = true;

            continue;
        }

        // every block is a parse of its own: the file's time is checked between them
        if out_of_time() {
            out.dropped += 1;
            out.selectors_cut = true;

            break;
        }

        read += 1;

        // the lines before the block are counted once for the file, not from the top for every block
        let between = src
            .as_bytes()
            .get(counted.0..block.body.start)
            .map_or(0, count_newlines);
        let line_off = counted.1 + between;

        counted = (block.body.start, line_off);

        // the copy is a run of newlines and the body, so the lines of what it defines are those of the file
        let mut copy = "\n".repeat(line_off);

        copy.push_str(body);

        // the names a file may work out are the file's, not each block's: sixteen blocks are no sixteen budgets
        let mut apart = Extract {
            names_worked: out.names_worked,
            ..Extract::default()
        };

        match lang {
            StyleLang::Scss => super::scss::extract(parser, "", &copy, &mut apart),
            StyleLang::Sass => super::sass::extract(parser, "", &copy, &mut apart),
            StyleLang::Less => super::less::extract(parser, "", &copy, &mut apart),
        }

        out.names_worked = apart.names_worked;
        append_style(apart, out);
    }
}

/// Appends what a style block gave to `out`, marked as written in a style. The definitions keep their order,
/// so a reference's owner is found by the index its definition got in `out`; a definition that was not kept
/// takes its references with it.
fn append_style(apart: Extract, out: &mut Extract) {
    let mut placed = Vec::with_capacity(apart.syms.len());

    for mut sym in apart.syms {
        sym.style = true;

        let class = sym.kind == "class";
        let kept = out.add_sym(sym);

        // a selector the file has no room for is lost like one a block did not give
        if kept == DROPPED && class {
            out.selectors_cut = true;
        }

        placed.push(kept);
    }

    for mut reference in apart.refs {
        reference.style = true;
        reference.from = reference.from.map(|from| placed.get(from).copied().unwrap_or(DROPPED));
        out.add_ref(reference);
    }

    for import in apart.imports {
        // a namespace of a block is not one of the script; a sheet it loads is a sheet of the file
        let local = if import.local.starts_with("sheet:") {
            import.local
        } else {
            format!("style:{}", import.local)
        };

        out.add_import(Import { local, ..import });
    }

    out.dropped += apart.dropped;
    out.selectors_cut |= apart.selectors_cut;
}

fn count_newlines(bytes: &[u8]) -> usize {
    bytes.iter().filter(|b| **b == b'\n').count()
}

/// Replaces every byte of `range` but newlines by an ASCII space: a multi-byte character becomes as many
/// spaces as it has bytes.
pub(super) fn blank(markup: &mut [u8], range: Range<usize>) {
    if let Some(region) = markup.get_mut(range) {
        for byte in region {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    }
}

/// The code of a leading `---` ... `---` block and the byte range to blank.
fn frontmatter(src: &str) -> Option<(&str, Range<usize>)> {
    let opening = ["---\n", "---\r\n"]
        .into_iter()
        .find(|opening| src.starts_with(opening))?;
    let rest = src.get(opening.len()..)?;
    let end = rest.find("\n---")?;

    Some((rest.get(..end)?, 0..opening.len() + end + "\n---".len()))
}

/// A `<tag ...> ... </tag>` block.
pub(super) struct Block {
    /// The text between the tags.
    pub(super) body: Range<usize>,
    /// From `<tag` through the closing `>`.
    pub(super) whole: Range<usize>,
}

/// The first place at or after `from` where `needle` is written, whatever the case of its letters. HTML does
/// not tell `<STYLE>` from `<style>`, and a page written in capitals is common; one pass over the bytes, and
/// no more than `needle` long is compared at each.
fn find_ignoring_case(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    let first = needle.first()?;

    bytes
        .get(from..)?
        .windows(needle.len())
        .position(|window| {
            window.first().is_some_and(|b| b.eq_ignore_ascii_case(first)) && window.eq_ignore_ascii_case(needle)
        })
        .map(|found| from + found)
}

/// The `<tag>` blocks of `src`, in order; `tag` is written in lowercase and found in any case. Quoted
/// attribute values may hold a `>`; a self-closing tag has no block; a tag that never closes ends the search.
pub(super) fn blocks(src: &str, tag: &str) -> Vec<Block> {
    let bytes = src.as_bytes();
    let opening = format!("<{tag}");
    let closing = format!("</{tag}");
    let mut found = Vec::new();
    let mut at = 0;

    while let Some(open) = find_ignoring_case(bytes, at, opening.as_bytes()) {
        let after = open + opening.len();

        // `<scripts>` or `<style-guide>` are other tags
        if !bytes
            .get(after)
            .is_some_and(|b| b.is_ascii_whitespace() || *b == b'>' || *b == b'/')
        {
            at = after;
            continue;
        }

        let Some((tag_end, self_closing)) = tag_end(bytes, after) else {
            break;
        };

        if self_closing {
            at = tag_end;
            continue;
        }

        let Some(close) = find_ignoring_case(bytes, tag_end, closing.as_bytes()) else {
            break;
        };
        let whole_end = src
            .get(close..)
            .and_then(|rest| rest.find('>'))
            .map_or(src.len(), |gt| close + gt + 1);

        found.push(Block {
            body: tag_end..close,
            whole: open..whole_end,
        });
        at = whole_end;
    }

    found
}

/// The offset just past the `>` that ends the opening tag starting before `from`, and whether it was `/>`.
fn tag_end(bytes: &[u8], from: usize) -> Option<(usize, bool)> {
    let mut quote: Option<u8> = None;

    for (i, byte) in bytes.get(from..)?.iter().enumerate() {
        match (quote, *byte) {
            (Some(q), b) if b == q => quote = None,
            (Some(_), _) => {}
            (None, b'"' | b'\'') => quote = Some(*byte),
            (None, b'>') => {
                let at = from + i;
                let self_closing = at > from && bytes.get(at - 1) == Some(&b'/');

                return Some((at + 1, self_closing));
            }
            _ => {}
        }
    }

    None
}

/// Component tags and quoted code names of the template.
fn scan_template(markup: &[u8], owner: Option<usize>, out: &mut Extract) {
    let mut line = 1;
    let mut i = 0;

    while let Some(&byte) = markup.get(i) {
        if i.is_multiple_of(SCAN_CHECK) && out_of_time() {
            out.dropped += 1;

            return;
        }

        match byte {
            b'\n' => line += 1,
            b'<' if markup.get(i + 1).is_some_and(u8::is_ascii_alphabetic) => {
                let tag: Vec<u8> = markup
                    .get(i + 1..)
                    .unwrap_or(&[])
                    .iter()
                    .copied()
                    .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
                    .collect();

                if let Some(name) = component_name(&String::from_utf8_lossy(&tag)) {
                    push_ref(out, owner, &name, "component", None, line);
                }
            }
            b'\'' | b'"' => {
                // a quoted code name inside a template expression: invoke('x'), emit("y"), ...
                let window = markup.get(i + 1..).unwrap_or(&[]);

                if let Some(len) = window.iter().take(MAX_QUOTED).position(|c| *c == byte)
                    && let Some(value) = window.get(..len).and_then(|v| std::str::from_utf8(v).ok())
                    && ident_like(value)
                {
                    push_ref(out, owner, value, "str", None, line);
                }
            }
            _ => {}
        }

        i += 1;
    }
}

/// The component a tag names: `<FooBar>`, `<Foo.Bar>` (Bar) or `<foo-bar>` (FooBar); plain HTML tags have
/// neither a capital nor a dash.
fn component_name(tag: &str) -> Option<String> {
    let tag = tag.rsplit('.').next().unwrap_or(tag);

    if tag.contains('-') {
        let name: String = tag
            .split('-')
            .map(|part| {
                let mut chars = part.chars();

                chars
                    .next()
                    .map(|first| first.to_ascii_uppercase().to_string() + chars.as_str())
                    .unwrap_or_default()
            })
            .collect();

        return (!name.is_empty()).then_some(name);
    }

    tag.starts_with(|c: char| c.is_ascii_uppercase())
        .then(|| tag.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(path: &str, src: &str) -> Extract {
        let mut out = Extract::default();
        extract(&mut Parser::new(), path, src, &mut out);
        out
    }

    #[test]
    fn script_functions_template_components_and_strings() {
        let out = run(
            "ui/DiffPanel.svelte",
            "<script lang=\"ts\">\n  import Card from './Card.svelte';\n  function load() { return invoke('git_file_lines'); }\n</script>\n\n<Card on:click={load} />\n<my-widget></my-widget>\n<div>{fmt('some_helper')}</div>\n",
        );

        assert_eq!(out.syms[0].kind, "component");
        assert_eq!(out.syms[0].name, "DiffPanel");

        let load = out.syms.iter().find(|s| s.name == "load").map(|s| s.line);
        assert_eq!(load, Some(3));

        let components: Vec<(&str, usize)> = out
            .refs
            .iter()
            .filter(|r| r.kind == "component")
            .map(|r| (r.name.as_str(), r.line))
            .collect();
        assert_eq!(components, vec![("Card", 6), ("MyWidget", 7)]);

        assert!(
            out.refs
                .iter()
                .any(|r| r.kind == "str" && r.name == "git_file_lines" && r.line == 3)
        );
        assert!(
            out.refs
                .iter()
                .any(|r| r.kind == "str" && r.name == "some_helper" && r.line == 8)
        );
    }

    #[test]
    fn functions_of_later_script_blocks_get_their_own_lines() {
        let out = run(
            "ui/Two.svelte",
            "<script context=\"module\">\nexport function first() {}\n</script>\n\n<p>text</p>\n<script>\n\nfunction second() {}\n</script>\n",
        );
        let line = |name: &str| out.syms.iter().find(|s| s.name == name).map(|s| s.line);

        assert_eq!(line("first"), Some(2));
        assert_eq!(line("second"), Some(8));
    }

    #[test]
    fn a_file_of_many_script_blocks_is_read_in_linear_time() {
        let blocks = 15_000;
        let src = "<script>function a(){}</script>\n".repeat(blocks);
        let started = std::time::Instant::now();
        let out = run("ui/Many.svelte", &src);

        // the lines before a block were once counted from the top of the file for every block
        assert!(
            started.elapsed() < std::time::Duration::from_secs(8),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(out.syms.iter().filter(|s| s.name == "a").count(), blocks);
        assert_eq!(
            out.syms.iter().filter(|s| s.name == "a").map(|s| s.line).max(),
            Some(blocks)
        );
    }

    #[test]
    fn multibyte_text_before_the_script_end_keeps_offsets_and_does_not_panic() {
        let src = "<script>\n// zażółć gęślą jaźń \u{1F980} 日本語\nfunction łaskawy() { return 1 }\n</script>\n<Wide />\n<p>żółć_x</p>\n";
        let out = run("multi.svelte", src);

        assert!(
            out.refs
                .iter()
                .any(|r| r.kind == "component" && r.name == "Wide" && r.line == 5)
        );
        // nothing of the blanked script leaks into the template scan
        assert!(out.refs.iter().all(|r| r.name != "script"));
    }

    #[test]
    fn blanking_keeps_the_byte_length_and_the_newlines() {
        let src = "ab\u{1F980}\ncd\u{e9}";
        let mut markup = src.as_bytes().to_vec();
        blank(&mut markup, 0..src.len());

        assert_eq!(markup.len(), src.len());
        assert_eq!(std::str::from_utf8(&markup), Ok("      \n    "));
    }

    #[test]
    fn frontmatter_and_generic_attributes_are_script() {
        let out = run(
            "page.astro",
            "---\nconst title = fetchTitle();\nfunction fetchTitle() { return 1 }\n---\n<Layout title={title} />\n",
        );
        assert!(out.syms.iter().any(|s| s.name == "fetchTitle" && s.line == 3));
        assert!(
            out.refs
                .iter()
                .any(|r| r.kind == "component" && r.name == "Layout" && r.line == 5)
        );

        let out = run(
            "gen.svelte",
            "<script lang=\"ts\" generics=\"T extends Record<string, unknown>\">\nfunction inner() {}\n</script>\n<X />\n",
        );
        assert!(out.syms.iter().any(|s| s.name == "inner" && s.line == 2));
        assert!(out.refs.iter().any(|r| r.kind == "component" && r.name == "X"));
    }

    fn template_refs(out: &Extract) -> Vec<(&str, &'static str, Option<&str>, usize)> {
        out.refs
            .iter()
            .filter(|r| matches!(r.kind, "use" | "call" | "mcall"))
            .map(|r| (r.name.as_str(), r.kind, r.qual.as_deref(), r.line))
            .collect()
    }

    #[test]
    fn imported_names_are_read_in_nested_expressions_and_never_in_strings_or_properties() {
        let out = run(
            "page.astro",
            "---\nimport Card from './Card.astro';\nimport { fmt, path as p } from './x';\nimport * as ns from './y';\n---\n<Card />\n{items.map((i) => <li class={`a ${fmt (i)}`}>{p}</li>)}\n<p title=\"fmt\">{ns.run(1)} {ns.NAME} {obj.fmt} {'fmt'} {/* fmt */}</p>\n{cond && <Card />}\n<!-- {fmt(1)} -->\n<p>fmt(2)</p>\n",
        );

        assert_eq!(
            template_refs(&out),
            vec![
                ("fmt", "call", None, 7),
                ("p", "use", None, 7),
                ("run", "mcall", Some("ns"), 8),
                ("NAME", "use", Some("ns"), 8),
            ]
        );
        // the tag `<Card />` is a component reference, not a use of the import
        assert!(out.refs.iter().all(|r| !(r.name == "Card" && r.kind == "use")));
    }

    #[test]
    fn a_name_the_script_used_on_the_same_line_is_recorded_once() {
        let out = run(
            "ui/Row.svelte",
            "<script>import { fmt } from './x'; fmt(1);</script>\n{fmt(2)}{fmt(3)}\n",
        );

        assert_eq!(
            template_refs(&out),
            vec![("fmt", "call", None, 1), ("fmt", "call", None, 2)]
        );
    }

    #[test]
    fn vue_reads_mustaches_and_dynamic_attributes_only() {
        let src = "<script setup>\nimport { fmt, go } from './x';\n</script>\n<template>\n  <a :href=\"fmt('a>b')\" @click='go()' title=\"fmt\" v-if=\"ok\">text</a>\n  <p>{{ fmt }} { go() }</p>\n</template>\n";
        let out = run("Row.vue", src);

        assert_eq!(
            template_refs(&out),
            vec![
                ("fmt", "call", None, 5),
                ("go", "call", None, 5),
                ("fmt", "use", None, 6)
            ]
        );
    }

    #[test]
    fn a_brace_that_never_closes_leaves_the_rest_of_the_template_unread() {
        let out = run(
            "a.svelte",
            "<script>import { fmt } from './x';</script>\n<p>{ fmt(1)</p>\n{fmt(2)}\n",
        );

        assert!(template_refs(&out).is_empty());
    }

    #[test]
    fn markup_in_an_expression_is_text_and_only_its_braces_are_code() {
        let out = run(
            "page.astro",
            "---\nimport { price, title, fmt } from './x';\n---\n{items.map(i => <li title={i.t} class=\"price\" {...title}>Total price: {fmt(i.p)} <br> <b>title</b></li>)}\n{a < price} {a<title}\n",
        );

        assert_eq!(
            template_refs(&out),
            vec![
                ("title", "use", None, 4),
                ("fmt", "call", None, 4),
                ("price", "use", None, 5),
                ("title", "use", None, 5),
            ]
        );
    }

    #[test]
    fn a_quote_or_a_slash_in_markup_text_in_an_expression_does_not_swallow_the_template() {
        let out = run(
            "page.astro",
            "---\nimport { fmt } from './x';\n---\n{ok && <p>use the ` key, it's <a href=http://x>here</a></p>}\n{fmt(1)}\n",
        );

        assert_eq!(template_refs(&out), vec![("fmt", "call", None, 5)]);
    }

    #[test]
    fn a_less_than_in_vue_text_does_not_hide_a_mustache() {
        let out = run(
            "Row.vue",
            "<script setup>\nimport { fmt } from './x';\n</script>\n<template>\n  <p>if a<b then {{ fmt(x) }} done</p>\n  <p>1 < 2 {{ fmt(y) }}</p>\n</template>\n",
        );

        assert_eq!(
            template_refs(&out),
            vec![("fmt", "call", None, 5), ("fmt", "call", None, 6)]
        );
    }

    #[test]
    fn calls_and_uses_of_different_namespaces_on_a_line_are_all_kept() {
        let out = run(
            "page.astro",
            "---\nimport * as ns from './a';\nimport * as other from './b';\nimport { run } from './c';\n---\n{ns.run(1)} {other.run(2)}\n{run} {ns.run()}\n",
        );

        assert_eq!(
            template_refs(&out),
            vec![
                ("run", "mcall", Some("ns"), 6),
                ("run", "mcall", Some("other"), 6),
                ("run", "use", None, 7),
                ("run", "mcall", Some("ns"), 7),
            ]
        );
    }

    #[test]
    fn a_style_block_defines_its_selectors_at_true_lines_between_two_scripts() {
        let script = "<script>\nexport function first() {}\n</script>\n";
        let after = "<script context=\"module\">\nexport function last() {}\n</script>\n";
        let cases = [
            ("A.vue", "css", ".card { color: red; }\n"),
            ("B.svelte", "scss", "$gap: 4px;\n.card { margin: $gap; }\n"),
            ("C.astro", "postcss", ".card { color: red; }\n"),
            ("D.vue", "sass", "$gap: 4px\n.card\n  margin: $gap\n"),
            ("E.svelte", "less", "@gap: 4px;\n.card { margin: @gap; }\n"),
        ];

        for (path, lang, body) in cases {
            let src = format!("{script}\n<style lang=\"{lang}\">\n{body}</style>\n{after}");
            let out = run(path, &src);
            let card = out.syms.iter().find(|s| s.name == ".card");

            // `<style>` is on line 5 and its body starts on line 6; `.card` follows the tokens
            let expected = if body.starts_with('.') { 6 } else { 7 };

            assert_eq!(card.map(|s| s.line), Some(expected), "{path} {lang}");
            assert!(card.is_some_and(|s| s.style), "{path} {lang}");

            // the scripts around it are read as before, and are no style
            let line = |name: &str| out.syms.iter().find(|s| s.name == name).map(|s| (s.line, s.style));

            assert_eq!(line("first"), Some((2, false)), "{path}");
            assert_eq!(line("last"), Some((8 + body.lines().count(), false)), "{path}");
        }
    }

    #[test]
    fn style_references_name_their_own_definition_and_the_scripts_stay_unmarked() {
        let out = run(
            "A.vue",
            "<script>\nimport { gap } from './x';\nexport function go() { return gap(1); }\n</script>\n<style lang=\"scss\">\n@use \"tokens\" as t;\n@import \"base\";\n.a { margin: t.$gap; @include box; }\n</style>\n",
        );
        let a = out.syms.iter().position(|s| s.name == ".a");

        assert!(a.is_some());

        let own: Vec<_> = out.refs.iter().filter(|r| r.style).collect();

        assert!(
            own.iter()
                .any(|r| r.name == "$gap" && r.qual.as_deref() == Some("t") && r.from == a)
        );
        assert!(own.iter().any(|r| r.name == "box" && r.kind == "macro" && r.from == a));
        assert!(out.refs.iter().any(|r| r.name == "gap" && !r.style));

        let locals: Vec<&str> = out.imports.iter().map(|i| i.local.as_str()).collect();

        assert_eq!(locals, vec!["gap", "style:t", "sheet:base"]);
    }

    #[test]
    fn a_less_block_keeps_its_at_signs_and_leaves_the_script_names_alone() {
        let out = run(
            "A.vue",
            "<script>\nexport function $price() {}\n</script>\n<style lang=\"less\">\n@gap: 4px;\n.a { margin: @gap; }\n</style>\n",
        );

        assert!(out.syms.iter().any(|s| s.name == "$price" && !s.style));
        assert!(out.syms.iter().any(|s| s.name == "@gap" && s.style));
        assert!(out.refs.iter().any(|r| r.name == "@gap" && r.style));
    }

    #[test]
    fn stylus_and_unknown_blocks_are_skipped_and_empty_blocks_are_fine() {
        for src in [
            "<style lang=\"stylus\">\n.a\n  color red\n</style>\n",
            "<style lang=\"wat\">.a { color: red }</style>\n",
            "<style lang=\"sass\"></style>\n",
            "<style></style>\n<style>   \n</style>\n",
            "<style lang=\"scss\" src=\"./x.scss\" />\n",
        ] {
            let out = run("a.vue", src);

            assert!(out.syms.iter().all(|s| !s.style), "{src}");
        }
    }

    #[test]
    fn the_language_is_read_from_lang_or_type_and_not_from_other_values() {
        let lang = |tag: &str| style_lang(tag);

        assert_eq!(lang("<style>"), Some(StyleLang::Scss));
        assert_eq!(lang("<style scoped lang=\"scss\">"), Some(StyleLang::Scss));
        assert_eq!(lang("<style lang=sass>"), Some(StyleLang::Sass));
        assert_eq!(lang("<style type='text/less'>"), Some(StyleLang::Less));
        assert_eq!(lang("<style data-x=\"lang=stylus\">"), Some(StyleLang::Scss));
        assert_eq!(lang("<style lang=\"styl\">"), None);
        assert_eq!(lang("<style lang=\"STYLUS\" scoped>"), None);
    }

    #[test]
    fn a_string_in_a_style_variable_is_elided_like_a_stylesheet_string() {
        let out = super::super::extract(
            &mut Parser::new(),
            "A.vue",
            "<script>\nexport const a = 1;\n</script>\n<style lang=\"scss\">\n$k: \"abc\";\n</style>\n",
        );
        let k = out.syms.iter().find(|s| s.name == "$k");

        assert!(k.is_some_and(|s| !s.sig.contains("abc")), "{:?}", k.map(|s| &s.sig));
    }

    #[test]
    fn a_script_the_clock_cut_marks_the_file_and_a_style_block_that_is_cut_does_not() {
        let src = "<script>\nexport const a = 1;\n</script>\n<style>.s { color: red }</style>\n";
        let spent = std::time::Instant::now();
        let out = super::super::extract_within(&mut Parser::new(), "a.vue", src, spent);

        assert!(out.classes_cut, "a script that was not read may hold class uses");

        // the styles are the last of the file: more of them than are read lose selectors, never a use
        let many: String = (0..20)
            .map(|i| format!("<style>.s{i} {{ color: red }}</style>\n"))
            .collect();

        assert!(!run("a.svelte", &many).classes_cut);
    }

    #[test]
    fn only_the_first_style_blocks_are_read_and_the_rest_are_counted() {
        let src: String = (0..20)
            .map(|i| format!("<style>.s{i} {{ color: red }}</style>\n"))
            .collect();
        let out = run("a.svelte", &src);

        assert_eq!(out.syms.iter().filter(|s| s.style).count(), MAX_STYLE_BLOCKS);
        assert_eq!(out.dropped, 4);
    }

    #[test]
    fn a_file_of_a_hundred_thousand_empty_style_blocks_is_read_quickly() {
        let src = "<style></style>\n".repeat(100_000);
        let started = std::time::Instant::now();
        let out = run("a.vue", &src);

        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(out.syms.iter().all(|s| !s.style));
        assert_eq!(out.dropped, 0);
    }

    #[test]
    fn unterminated_blocks_do_not_panic() {
        for src in [
            "<script>",
            "<script lang=\"ts\"",
            "<script>function a(",
            "---\n",
            "---\nx\n--",
            "<style>",
            "<style lang=",
            "<style lang=\"sass",
            "<script/>",
        ] {
            let _ = run("a.vue", src);
        }
    }

    #[test]
    fn a_template_class_belongs_to_the_component_and_honours_the_sheets_the_script_imports() {
        let svelte = run(
            "ui/Box.svelte",
            "<script>\n  import styles from './Box.module.css';\n  import './box.css';\n  const c = clsx('from-script');\n</script>\n\n<div class=\"plain {styles.card}\" class:on={x}>\n  <p class={cn('inner', styles['row'])}></p>\n</div>\n",
        );
        let found: Vec<(&str, Option<&str>, usize)> = svelte
            .refs
            .iter()
            .filter(|r| r.kind == "cls")
            .map(|r| (r.name.as_str(), r.qual.as_deref(), r.line))
            .collect();

        assert_eq!(
            found,
            vec![
                ("from-script", None, 4),
                ("plain", None, 7),
                ("on", None, 7),
                ("card", Some("styles"), 7),
                ("inner", None, 8),
                ("row", Some("styles"), 8),
            ]
        );
        assert!(
            svelte
                .refs
                .iter()
                .filter(|r| r.kind == "cls")
                .all(|r| r.from == Some(0) && !r.style)
        );
        assert!(svelte.imports.iter().any(|i| i.local == "sheet:./box.css"));
    }

    #[test]
    fn a_vue_template_reaches_its_style_module_as_dollar_style() {
        let out = run(
            "ui/Card.vue",
            "<template>\n  <p :class=\"[$style.card, { on: ok }]\">x</p>\n</template>\n<style module>\n.card { top: 0 }\n</style>\n",
        );
        let found: Vec<(&str, Option<&str>, usize)> = out
            .refs
            .iter()
            .filter(|r| r.kind == "cls")
            .map(|r| (r.name.as_str(), r.qual.as_deref(), r.line))
            .collect();

        assert_eq!(found, vec![("card", Some("$style"), 2), ("on", None, 2)]);
    }

    #[test]
    fn a_file_of_many_empty_style_blocks_and_class_attributes_is_read_quickly() {
        let started = std::time::Instant::now();
        let src = "<style></style><p class=\"a\"></p>\n".repeat(100_000);
        let out = run("ui/Many.svelte", &src);

        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(out.refs.iter().filter(|r| r.kind == "cls").count(), 1);
    }

    #[test]
    fn a_reference_on_the_row_of_a_script_tag_is_marked_and_one_on_a_row_of_code_is_not() {
        let out = run(
            "A.vue",
            "<template><p>text</p></template><script>f()</script>\n<script>\ng();\nh();</script>\n<script>\nplain();\n</script>\n",
        );
        let marked = |name: &str| out.refs.iter().find(|r| r.name == name).map(|r| r.markup);

        assert_eq!(marked("f"), Some(true));
        assert_eq!(marked("g"), Some(false));
        assert_eq!(marked("h"), Some(true));
        assert_eq!(marked("plain"), Some(false));
    }
}
