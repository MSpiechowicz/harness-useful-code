//! SCSS and CSS: the design tokens, mixins, functions and top-level rules of a stylesheet, and where they are used.
//!
//! | definition                              | kind    | name          |
//! |-----------------------------------------|---------|---------------|
//! | `$gap: 4px;` at the top level           | `const` | `$gap`        |
//! | `--ds-color: #fff;` in any rule         | `const` | `--ds-color`  |
//! | `@mixin box { }`                        | `macro` | `box`         |
//! | `@function rem() { }`                   | `fn`    | `rem`         |
//! | a top-level rule (`.card`, `%base`)     | `class` | its first selector |
//!
//! References: `$gap` and `var(--ds-color)` are `use`, `@include box` is `macro`, a function call and
//! `@extend .card` / `@extend %base` are `call`. A reference is made by the top-level rule, mixin or
//! function it is written in. A module member (`t.$size`, `t.rem(1)`, `@include t.box`) keeps the namespace
//! as its qualifier. Parameters, loop variables and variables declared inside a rule are local: they
//! are no references.
//!
//! Only `@use "x" as ns` and `@use "x"` (namespace `x`) are imports. `@forward`, `@import` and `@use ... as *`
//! bring names without a prefix, so those resolve by name like any other unqualified reference.

use std::collections::HashSet;

use tree_sitter::{Node, Parser};

use super::{DROPPED, Extract, Import, MAX_DEPTH, Placement, fits, push_def, push_ref, text};

/// Reads a SCSS or CSS file.
pub(super) fn extract(parser: &mut Parser, _path: &str, src: &str, out: &mut Extract) {
    let Some(tree) = super::parse(parser, &tree_sitter_scss_modern::LANGUAGE.into(), src) else {
        return;
    };
    let root = tree.root_node();
    let mut walk = Walk {
        place: Placement {
            src: src.as_bytes(),
            line_off: 0,
            test: false,
        },
        out,
        locals: HashSet::new(),
    };
    let mut cursor = root.walk();

    for item in root.named_children(&mut cursor) {
        walk.locals.clear();
        walk.node(item, None, 1);
    }
}

struct Walk<'a> {
    place: Placement<'a>,
    out: &'a mut Extract,
    /// Variables of the top-level item being read that are not module-wide: parameters, loop variables and
    /// variables declared in a rule.
    locals: HashSet<String>,
}

impl<'a> Walk<'a> {
    fn src(&self) -> &'a [u8] {
        self.place.src
    }

    fn node(&mut self, node: Node, from: Option<usize>, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }

        match node.kind() {
            "use_statement" => self.import(node),
            "mixin_statement" | "function_statement" => self.callable(node, from, depth),
            "rule_set" => {
                let from = if depth == 1 { self.rule(node).or(from) } else { from };

                self.children(node, from, depth);
            }
            "declaration" => self.declaration(node, from, depth),
            "include_statement" => {
                if let Some(name) = node.named_child(0).filter(|n| n.kind() == "identifier") {
                    self.member(name, "macro", from);
                }

                self.children_after(node, 1, from, depth);
            }
            "extend_statement" => self.extend(node, from),
            "call_expression" => self.call(node, from, depth),
            "variable" => self.variable(node, from),
            "plain_value" => self.plain(node, from),
            "parameter" => {
                if let Some(variable) = node.named_child(0).filter(|n| n.kind() == "variable") {
                    self.locals.insert(text(variable, self.src()).to_string());
                }

                if let Some(default) = node.child_by_field_name("default") {
                    self.node(default, from, depth + 1);
                }
            }
            "argument" => {
                if let Some(value) = node.child_by_field_name("value") {
                    self.node(value, from, depth + 1);
                } else {
                    self.children(node, from, depth);
                }
            }
            "each_statement" | "for_statement" => self.looped(node, from, depth),
            _ => self.children(node, from, depth),
        }
    }

    fn children(&mut self, node: Node, from: Option<usize>, depth: usize) {
        self.children_after(node, 0, from, depth);
    }

    /// The named children of `node` from the `skip`-th on.
    fn children_after(&mut self, node: Node, skip: usize, from: Option<usize>, depth: usize) {
        let mut cursor = node.walk();

        for child in node.named_children(&mut cursor).skip(skip) {
            self.node(child, from, depth + 1);
        }
    }

    /// `@use "x" as ns`: the namespace and where it comes from.
    fn import(&mut self, node: Node) {
        let src = self.src();
        let mut cursor = node.walk();
        let mut source = None;
        let mut alias = None;
        let mut star = false;

        for child in node.named_children(&mut cursor) {
            match child.kind() {
                "string_value" => source = Some(unquote(text(child, src))),
                "as_clause" => match child.named_child(0).filter(|n| n.kind() == "namespace_name") {
                    Some(name) => alias = Some(text(name, src).to_string()),
                    None => star = true,
                },
                _ => {}
            }
        }

        let Some(source) = source.filter(|s| !s.is_empty()) else {
            return;
        };

        if star {
            return;
        }

        let local = alias.unwrap_or_else(|| default_namespace(&source));

        if !local.is_empty() {
            self.out.add_import(Import {
                local,
                orig: "*".into(),
                source,
            });
        }
    }

    /// `@mixin` and `@function`: the definition, then its body with its parameters as locals.
    fn callable(&mut self, node: Node, from: Option<usize>, depth: usize) {
        let Some(name) = node.child_by_field_name("name") else {
            return;
        };
        let kind = if node.kind() == "mixin_statement" {
            "macro"
        } else {
            "fn"
        };
        let sym = push_def(self.out, node, kind, name, None, &self.place);
        let inner = if sym == DROPPED { from } else { Some(sym) };
        let mut cursor = node.walk();

        for child in node.named_children(&mut cursor) {
            if child.id() != name.id() {
                self.node(child, inner, depth + 1);
            }
        }
    }

    /// A top-level rule is a definition named by its first selector, so what it uses is attributed to it.
    fn rule(&mut self, node: Node) -> Option<usize> {
        let selectors = node.named_child(0).filter(|n| n.kind() == "selectors")?;
        let first = text(selectors, self.src()).split(',').next()?.trim();
        let name: String = first.split_whitespace().collect::<Vec<_>>().join(" ");

        if name.is_empty() || !fits(&name) {
            return None;
        }

        let sym = self.out.add_sym(super::Sym {
            name,
            kind: "class",
            parent: None,
            line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
            sig: super::first_line(node, self.src()),
            ret: None,
            test: false,
        });

        (sym != DROPPED).then_some(sym)
    }

    /// `$var: value` (a definition at the top level, a local below) and `--token: value` (a definition anywhere).
    fn declaration(&mut self, node: Node, from: Option<usize>, depth: usize) {
        let Some(property) = node.named_child(0).filter(|n| n.kind() == "property_name") else {
            return self.children(node, from, depth);
        };
        let name = text(property, self.src());
        let mut inner = from;

        if name.starts_with("--") || (name.starts_with('$') && depth == 1) {
            let sym = push_def(self.out, node, "const", property, None, &self.place);

            if sym != DROPPED {
                inner = Some(sym);
            }
        } else if name.starts_with('$') {
            self.locals.insert(name.to_string());
        }

        self.children_after(node, 1, inner, depth);
    }

    /// `@each $k, $v in $map` and `@for $i from`: the loop variables are local, the rest is read.
    fn looped(&mut self, node: Node, from: Option<usize>, depth: usize) {
        let mut cursor = node.walk();
        let mut vars = Vec::new();

        if node.kind() == "for_statement" {
            vars.extend(node.named_child(0).filter(|n| n.kind() == "variable"));
        } else {
            vars.extend(node.child_by_field_name("key"));
            vars.extend(node.child_by_field_name("value"));
        }

        for var in &vars {
            self.locals.insert(text(*var, self.src()).to_string());
        }

        for child in node.named_children(&mut cursor) {
            if !vars.iter().any(|var| var.id() == child.id()) {
                self.node(child, from, depth + 1);
            }
        }
    }

    /// `@extend %base` and `@extend .card`.
    fn extend(&mut self, node: Node, from: Option<usize>) {
        let Some(target) = node.named_child(0) else {
            return;
        };

        if matches!(target.kind(), "placeholder" | "class_selector") {
            let name = text(target, self.src()).trim().to_string();

            push_ref(self.out, from, &name, "call", None, target.start_position().row + 1);
        }
    }

    /// A function call; `var(--token)` is a use of the token.
    fn call(&mut self, node: Node, from: Option<usize>, depth: usize) {
        let Some(function) = node.named_child(0).filter(|n| n.kind() == "function_name") else {
            return self.children(node, from, depth);
        };

        if text(function, self.src()) == "var" {
            let token = node
                .named_child(1)
                .and_then(|arguments| arguments.named_child(0))
                .map(|arg| text(arg, self.src()).trim().to_string())
                .filter(|token| token.starts_with("--"));

            if let Some(token) = token {
                push_ref(self.out, from, &token, "use", None, function.start_position().row + 1);
            }
        } else {
            self.member(function, "call", from);
        }

        self.children_after(node, 1, from, depth);
    }

    /// A reference written `name` or `ns.name`.
    fn member(&mut self, node: Node, kind: &'static str, from: Option<usize>) {
        let written = text(node, self.src()).trim();
        let line = node.start_position().row + 1;

        match written.rsplit_once('.') {
            Some((qual, name)) => push_ref(self.out, from, name, kind, Some(qual.to_string()), line),
            None => push_ref(self.out, from, written, kind, None, line),
        }
    }

    /// `$var` or `ns.$var`.
    fn variable(&mut self, node: Node, from: Option<usize>) {
        let written = text(node, self.src()).trim();
        let line = node.start_position().row + 1;

        match written.rsplit_once('.') {
            Some((qual, name)) => push_ref(self.out, from, name, "use", Some(qual.to_string()), line),
            None if !self.locals.contains(written) => push_ref(self.out, from, written, "use", None, line),
            None => {}
        }
    }

    /// A module variable the grammar reads as a plain value: `t.$size`.
    fn plain(&mut self, node: Node, from: Option<usize>) {
        let written = text(node, self.src());

        if written.contains(".$") && !written.contains(char::is_whitespace) {
            self.variable(node, from);
        }
    }
}

/// `"../tokens"` -> `../tokens`.
fn unquote(literal: &str) -> String {
    literal.trim().trim_matches(['"', '\'']).to_string()
}

/// The namespace `@use "x"` gives: the last segment of the source without a leading `_` or the extension
/// (`sass:math` -> `math`, `../core/_tokens.scss` -> `tokens`).
fn default_namespace(source: &str) -> String {
    let last = source.rsplit(['/', ':']).next().unwrap_or(source);
    let last = last.strip_prefix('_').unwrap_or(last);

    last.strip_suffix(".scss")
        .or_else(|| last.strip_suffix(".css"))
        .unwrap_or(last)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &str) -> Extract {
        super::super::extract(&mut Parser::new(), "a.scss", src)
    }

    fn defs(out: &Extract) -> Vec<(&str, &str)> {
        out.syms.iter().map(|s| (s.kind, s.name.as_str())).collect()
    }

    fn refs(out: &Extract) -> Vec<(&str, &str, Option<&str>)> {
        out.refs
            .iter()
            .map(|r| (r.kind, r.name.as_str(), r.qual.as_deref()))
            .collect()
    }

    #[test]
    fn tokens_mixins_functions_and_rules_are_definitions() {
        let out = run(
            "$gap: 4px !default;\n:root { --ds-color: #fff; }\n%base { padding: $gap; }\n@mixin box($p: 1px) { padding: $p; }\n@function rem($px) { @return $px / 16; }\n.card { color: red; $tmp: 1; }\n",
        );

        assert_eq!(
            defs(&out),
            [
                ("const", "$gap"),
                ("class", ":root"),
                ("const", "--ds-color"),
                ("class", "%base"),
                ("macro", "box"),
                ("fn", "rem"),
                ("class", ".card"),
            ]
        );
    }

    #[test]
    fn uses_belong_to_the_enclosing_top_level_item() {
        let out = run(
            "@mixin box { margin: $gap; }\n.card { color: var(--ds-color, red); width: rem(2); @include box; @extend %base; &:hover { gap: $gap; } }\n",
        );
        let owner = |name: &str| {
            out.refs
                .iter()
                .filter(|r| r.name == name)
                .filter_map(|r| r.from.and_then(|f| out.syms.get(f)))
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
        };

        assert_eq!(owner("$gap"), ["box", ".card"]);
        assert_eq!(owner("--ds-color"), [".card"]);
        assert_eq!(owner("rem"), [".card"]);
        assert_eq!(owner("box"), [".card"]);
        assert_eq!(owner("%base"), [".card"]);
        assert!(!refs(&out).iter().any(|r| r.1 == "var"));
    }

    #[test]
    fn parameters_loop_variables_and_local_variables_are_no_references() {
        let out = run(
            "@mixin m($a, $b: $c) { x: $a $b; $t: 1; y: $t; }\n@each $k, $v in $map { .i-#{$k} { w: $v } }\n@for $i from 1 through 3 { .j { w: $i } }\n",
        );
        let used: Vec<&str> = out.refs.iter().map(|r| r.name.as_str()).collect();

        assert_eq!(used, ["$c", "$map"]);
    }

    #[test]
    fn module_members_keep_their_namespace() {
        let out = run(
            "@use \"../core/tokens\" as t;\n@use \"sass:math\";\n@use \"x\" as *;\n.a { w: t.$size; h: math.div(1, 2); @include t.mx(1); }\n",
        );
        let imports: Vec<_> = out
            .imports
            .iter()
            .map(|i| (i.local.as_str(), i.source.as_str()))
            .collect();

        assert_eq!(imports, [("t", "../core/tokens"), ("math", "sass:math")]);
        assert_eq!(
            refs(&out),
            [
                ("use", "$size", Some("t")),
                ("call", "div", Some("math")),
                ("macro", "mx", Some("t")),
            ]
        );
    }

    #[test]
    fn plain_css_is_read_too() {
        let out = super::super::extract(
            &mut Parser::new(),
            "a.css",
            ":root { --a: 1px; }\n.b { margin: var(--a); }\n",
        );

        assert_eq!(defs(&out), [("class", ":root"), ("const", "--a"), ("class", ".b")]);
        assert_eq!(refs(&out), [("use", "--a", None)]);
    }
}
