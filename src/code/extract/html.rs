//! HTML pages: the style blocks and the classes the markup uses.
//!
//! A page defines no code and has no owner symbol. Its `<script>` is not parsed (a page's scripts are no part of
//! the index), and its `<style>` blocks are read as stylesheets by the reader single-file components use, so
//! their selectors are definitions at their true lines.
//!
//! The scripts and styles are blanked in a copy of the page byte for byte. That copy is where the class
//! attributes of the page are read from (see `markup`): markup inside a script or a style is not markup.

use std::collections::HashSet;

use tree_sitter::Parser;

use super::Extract;
use super::markup::{self, Dialect};
use super::sfc::{blank, blocks, read_styles};

/// Reads an HTML page.
pub(super) fn extract(parser: &mut Parser, _path: &str, src: &str, out: &mut Extract) {
    let mut markup = src.as_bytes().to_vec();

    for script in blocks(src, "script") {
        blank(&mut markup, script.whole);
    }

    let styles = blocks(src, "style");

    for style in &styles {
        blank(&mut markup, style.whole.clone());
    }

    read_styles(parser, src, &styles, out);

    // a page has no owner definition: a class it uses is a reference of the file itself
    markup::extract(parser, &markup, Dialect::Html, &HashSet::new(), None, out);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &str) -> Extract {
        super::super::extract(&mut Parser::new(), "public/index.html", src)
    }

    #[test]
    fn style_blocks_define_selectors_at_their_lines_and_scripts_are_not_read() {
        let out = run(
            "<html>\n<head>\n<style>\n.card { color: red; }\n</style>\n<script>function boot() {}</script>\n</head>\n<style lang=\"scss\">\n$gap: 4px;\n.row { margin: $gap; }\n</style>\n</html>\n",
        );
        let defs: Vec<(&str, usize)> = out.syms.iter().map(|s| (s.name.as_str(), s.line)).collect();

        assert_eq!(defs, vec![(".card", 4), ("$gap", 9), (".row", 10)]);
        assert!(out.syms.iter().all(|s| s.style));
        assert!(out.refs.iter().all(|r| r.style && r.name != "boot"));
    }

    #[test]
    fn a_page_without_styles_defines_nothing_and_a_class_is_its_only_reference() {
        let out = run("<html><body><p class=\"a\">x</p><script>var a = 1;</script></body></html>\n");

        assert!(out.syms.is_empty() && out.imports.is_empty());
        assert_eq!(out.refs.len(), 1);
        assert!(
            out.refs
                .iter()
                .all(|r| r.kind == "cls" && r.name == "a" && r.from.is_none())
        );
    }

    #[test]
    fn the_classes_of_a_page_skip_its_scripts_and_styles() {
        let out = run(
            "<style>\n.a { top: 0 }\n</style>\n<script>\nconst s = '<p class=\"in-script\">';\n</script>\n<p class=\"x\">\n",
        );
        let found: Vec<(&str, usize)> = out
            .refs
            .iter()
            .filter(|r| r.kind == "cls")
            .map(|r| (r.name.as_str(), r.line))
            .collect();

        assert_eq!(found, vec![("x", 7)]);
        assert!(out.syms.iter().any(|s| s.name == ".a" && s.style));
    }

    #[test]
    fn a_page_written_in_capitals_is_read_like_one_in_lowercase() {
        let out = run(
            "<HTML>\n<STYLE>\n.card { color: red; }\n</STYLE>\n<Script>const s = '<p class=\"in-script\">';</Script>\n<P CLASS=\"x\">\n",
        );
        let found: Vec<&str> = out
            .refs
            .iter()
            .filter(|r| r.kind == "cls")
            .map(|r| r.name.as_str())
            .collect();

        assert_eq!(found, vec!["x"]);
        assert!(out.syms.iter().any(|s| s.name == ".card" && s.line == 3 && s.style));
    }
}
