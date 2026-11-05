//! SCSS and CSS: the design tokens, mixins, functions, selectors and keyframes of a stylesheet, and where they are used.
//!
//! | definition                              | kind    | name          |
//! |-----------------------------------------|---------|---------------|
//! | `$gap: 4px;` at the top level           | `const` | `$gap`        |
//! | `--ds-color: #fff;` in any rule         | `const` | `--ds-color`  |
//! | `@mixin box { }`                        | `macro` | `box`         |
//! | `@function rem() { }`                   | `fn`    | `rem`         |
//! | each selector of a rule (`.a, %base { }`) | `class` | the selector  |
//! | `@keyframes spin { }`                   | `const` | `spin`        |
//!
//! A rule is read wherever it is written: at the top level, nested, or in `@media`, `@layer`, `@supports` and
//! the like. A nested selector is named as it applies, from each selector of the rule around it (at most
//! [`MAX_SCOPE`] of them): `.nav { .item { } &--on { } }` defines `.nav .item` and `.nav--on`. A selector with
//! a `&` no rule around it explains (in a mixin) has no name to find it by and is left out, and so is what is
//! nested in it.
//!
//! References: `$gap` and `var(--ds-color)` are `use`, `@include box` is `macro`, a function call and
//! `@extend .card` / `@extend %base` are `call`. A reference is made by the outermost rule (its first
//! selector), mixin, function or keyframes it is written in. A module member (`t.$size`, `t.rem(1)`, `@include t.box`) keeps the namespace
//! as its qualifier. Parameters, loop variables and variables declared inside a rule are local: they
//! are no references.
//!
//! Only `@use "x" as ns` and `@use "x"` (namespace `x`) are imports. `@forward`, `@import` and `@use ... as *`
//! bring names without a prefix, so those resolve by name like any other unqualified reference.

use std::collections::HashSet;

use tree_sitter::{Node, Parser};

use super::{DROPPED, Extract, Import, MAX_DEPTH, MAX_STR, Placement, fits, push_def, push_ref, text};

/// The selectors of a rule that nested selectors are named from. A list inside a list multiplies, so this bounds
/// what one nested rule can add however the lists around it are written.
const MAX_SCOPE: usize = 32;

/// Reads a SCSS or CSS file.
pub(super) fn extract(parser: &mut Parser, _path: &str, src: &str, out: &mut Extract) {
    let readable = for_the_grammar(src);
    let Some(tree) = super::parse(parser, &tree_sitter_scss_modern::LANGUAGE.into(), &readable) else {
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
        scope: None,
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
    /// The selectors of the rule being read, as they apply; `None` outside any rule. A rule that gave no name
    /// leaves an empty list: what is nested in it has no name either.
    scope: Option<Vec<String>>,
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
            "rule_set" => self.rule(node, from, depth),
            "at_root_statement" => self.at_root(node, from, depth),
            "keyframes_statement" => self.keyframes(node, from, depth),
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

    /// `@at-root .r { }` is a rule named from the root. `@at-root { .r { } }` only holds rules, named from the root
    /// unless its query keeps the rule around it: `(without: media)` does.
    fn at_root(&mut self, node: Node, from: Option<usize>, depth: usize) {
        if node.named_child(0).is_some_and(|n| n.kind() == "selectors") {
            let outer = self.scope.take();

            self.rule(node, from, depth);
            self.scope = outer;
        } else if keeps_the_rule(text(node, self.src()).split('{').next().unwrap_or_default()) {
            self.children(node, from, depth);
        } else {
            let outer = self.scope.take();

            self.children(node, from, depth);
            self.scope = outer;
        }
    }

    /// A rule: a definition for each of its selectors, then its body. What the body uses belongs to what `from`
    /// names, or to the first selector when the rule is the outermost.
    fn rule(&mut self, node: Node, from: Option<usize>, depth: usize) {
        let mut owner = from;
        let mut names = Vec::new();

        for (name, line) in self.selectors(node) {
            // `.a { &, &:hover { } }`: the `&` is the rule around it, already a definition, and still a scope
            if self.scope.as_ref().is_some_and(|scope| scope.contains(&name)) {
                names.push(name);
                continue;
            }

            let sym = self.out.add_sym(super::Sym {
                sig: name.clone(),
                name: name.clone(),
                kind: "class",
                parent: None,
                line,
                end_line: node.end_position().row + 1,
                ret: None,
                test: false,
            });

            if sym != DROPPED {
                owner = owner.or(Some(sym));
                names.push(name);
            }
        }

        names.truncate(MAX_SCOPE);

        let outer = self.scope.replace(names);

        self.children(node, owner, depth);
        self.scope = outer;
    }

    /// The selectors of a rule as they apply, each with the line it is written on.
    fn selectors(&self, node: Node) -> Vec<(String, usize)> {
        let Some(selectors) = node.named_child(0).filter(|n| n.kind() == "selectors") else {
            return Vec::new();
        };
        let mut cursor = selectors.walk();
        let mut names = Vec::new();

        for selector in selectors.named_children(&mut cursor) {
            let written = text(selector, self.src());

            if selector.kind() == "comment" || selector.is_error() || !fits(written) || holds_a_body(written) {
                continue;
            }

            let written = written.split_whitespace().collect::<Vec<_>>().join(" ");
            let line = selector.start_position().row + 1;

            if written.is_empty() {
                continue;
            }

            match &self.scope {
                Some(scope) => names.extend(
                    scope
                        .iter()
                        .filter(|outer| outer.len() + written.len() < MAX_STR)
                        .map(|outer| (nested(outer, &written), line)),
                ),
                None if !written.contains('&') => names.push((written, line)),
                None => {}
            }
        }

        // `.a, .a { }` and `.a { &, & { } }` say one thing twice
        let mut seen = HashSet::new();

        names.retain(|(name, _)| seen.insert(name.clone()));

        names
    }

    /// `@keyframes spin`: a definition that owns what its frames use, unless it is written in something that does. Its frames (`from`, `50%`) are no selectors.
    fn keyframes(&mut self, node: Node, from: Option<usize>, depth: usize) {
        let mut cursor = node.walk();
        let name = node.named_children(&mut cursor).find(|n| n.kind() == "keyframes_name");
        let sym = name.map(|name| push_def(self.out, node, "const", name, None, &self.place));
        let inner = from.or(sym.filter(|sym| *sym != DROPPED));

        self.children(node, inner, depth);
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

/// The pseudo-classes whose argument is a selector.
const SELECTOR_PSEUDOS: &[&[u8]] = &[
    b"not",
    b"has",
    b"is",
    b"where",
    b"matches",
    b"any",
    b"-webkit-any",
    b"-moz-any",
    b"nth-child",
    b"nth-last-child",
    b"host",
    b"host-context",
    b"slotted",
    b"global",
    b"local",
    b"deep",
];

/// The copy of `src` the grammar reads, of the same length and lines; names are read from `src` by offset. The
/// grammar fails on two things that are valid, and can lose the rest of the file to them: a non-ASCII byte in a
/// name (`.é`), and parentheses in the argument of a selector pseudo-class (`:has(a:not(:b))`). Here both are `_`.
/// Nothing else is touched, and nothing is carried from one place in the file to the next: a value such as
/// `calc((1 + $a))` keeps what it reads, whatever is written around it.
fn for_the_grammar(src: &str) -> String {
    let mut bytes: Vec<u8> = src.bytes().map(|b| if b.is_ascii() { b } else { b'_' }).collect();

    if src.starts_with('\u{feff}') {
        bytes.iter_mut().take(3).for_each(|b| *b = b' ');
    }

    let mut at = 0;

    while let Some(colon) = bytes.iter().skip(at).position(|b| *b == b':').map(|found| at + found) {
        let name = bytes.get(colon + 1..).unwrap_or_default();
        let pseudo = SELECTOR_PSEUDOS.iter().find(|pseudo| {
            name.strip_prefix(**pseudo)
                .is_some_and(|rest| rest.first() == Some(&b'('))
        });

        at = match pseudo {
            Some(pseudo) => mask_argument(&mut bytes, colon + 1 + pseudo.len()),
            None => colon + 1,
        };
    }

    String::from_utf8(bytes).unwrap_or_else(|_| src.to_string())
}

/// Masks what is in parentheses in the argument that opens at `open`, and gives the offset to go on from. An
/// argument that a `{`, `}` or `;` cuts is no selector and stays as written; going on after the cut keeps a file
/// of `:not(:not(:not(` linear.
fn mask_argument(bytes: &mut [u8], open: usize) -> usize {
    let mut depth = 0usize;
    let mut nested = Vec::new();
    let mut from = open;
    let mut at = open;

    while let Some(&byte) = bytes.get(at) {
        if let Some(end) = quiet_end(bytes, at) {
            at = end;
            continue;
        }

        match byte {
            b'#' if bytes.get(at + 1) == Some(&b'{') => {
                at = interpolation_end(bytes, at);
                continue;
            }
            b'{' | b'}' | b';' => return at,
            b'(' => {
                depth += 1;

                if depth == 2 {
                    from = at;
                }
            }
            b')' => {
                depth -= 1;

                if depth == 1 {
                    nested.push(from..at + 1);
                }

                if depth == 0 {
                    for range in nested {
                        bytes
                            .iter_mut()
                            .take(range.end)
                            .skip(range.start)
                            .filter(|b| **b != b'\n')
                            .for_each(|b| *b = b'_');
                    }

                    return at + 1;
                }
            }
            _ => {}
        }

        at += 1;
    }

    at
}

/// Where the string or block comment that starts at `at` ends, when one does. A string ends at its line at the
/// latest, as it does in CSS: a stray apostrophe does not reach over the rest of the file.
fn quiet_end(bytes: &[u8], at: usize) -> Option<usize> {
    let rest = bytes.get(at..)?;

    match rest.first()? {
        quote @ (b'"' | b'\'') => {
            let mut escaped = false;
            let end = rest.iter().skip(1).position(|byte| {
                let closes = (!escaped && byte == quote) || *byte == b'\n';

                escaped = !escaped && *byte == b'\\';

                closes
            });

            Some(end.map_or(bytes.len(), |end| at + end + 2))
        }
        b'/' if rest.get(1) == Some(&b'*') => {
            let tail = rest.get(2..).unwrap_or_default();

            Some(
                tail.windows(2)
                    .position(|pair| pair == b"*/")
                    .map_or(bytes.len(), |end| at + end + 4),
            )
        }
        _ => None,
    }
}

/// The offset after the interpolation that starts at `at` with `#{`.
fn interpolation_end(bytes: &[u8], at: usize) -> usize {
    let mut depth = 0usize;

    for (offset, byte) in bytes.iter().enumerate().skip(at + 1) {
        match byte {
            b'{' => depth += 1,
            b'}' if depth <= 1 => return offset + 1,
            b'}' => depth -= 1,
            _ => {}
        }
    }

    bytes.len()
}

/// True when the selector the grammar gave holds braces that are no interpolation or string: it read a rule body
/// into it, and what it holds is no name. A `#{...}` and a `[a="{"]` are part of a selector.
fn holds_a_body(written: &str) -> bool {
    let bytes = written.as_bytes();
    let mut at = 0;

    while let Some(&byte) = bytes.get(at) {
        if let Some(end) = quiet_end(bytes, at) {
            at = end;
        } else if byte == b'#' && bytes.get(at + 1) == Some(&b'{') {
            at = interpolation_end(bytes, at);
        } else if matches!(byte, b'{' | b'}') {
            return true;
        } else {
            at += 1;
        }
    }

    false
}

/// True when the `@at-root` query in `head` keeps the rule it is written in: `(without: media)`, `(with: rule)`.
fn keeps_the_rule(head: &str) -> bool {
    let names_the_rule = |query: &str| query.contains("rule") || query.contains("all");

    match (head.split_once("without:"), head.split_once("with:")) {
        (Some((_, query)), _) => !names_the_rule(query),
        (None, Some((_, query))) => names_the_rule(query),
        (None, None) => false,
    }
}

/// The selector `written` inside the rule selector `outer`: `&` stands for it, and without one it is a descendant.
fn nested(outer: &str, written: &str) -> String {
    if written.contains('&') {
        written.replace('&', outer)
    } else {
        format!("{outer} {written}")
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
    fn every_selector_of_a_list_is_a_definition_on_its_own_line() {
        let out = run(".a, .b > c:hover,\n.d { top: 0; }\n:is(.m, .n), a[b=\"c,d\"] { top: 0; }\n");
        let lines: Vec<_> = out.syms.iter().map(|s| (s.name.as_str(), s.line)).collect();

        assert_eq!(
            lines,
            [
                (".a", 1),
                (".b > c:hover", 1),
                (".d", 2),
                (":is(.m, .n)", 3),
                ("a[b=\"c,d\"]", 3),
            ]
        );
    }

    #[test]
    fn nested_selectors_are_named_as_they_apply() {
        let out = run(
            ".nav, .bar { .item { &--on { top: 0; } } &:hover { top: 0; } > .kid, + .next { top: 0; } @media (min-width: 1px) { .deep { top: 0; } } @at-root .root { top: 0; } }\n",
        );
        let names: Vec<_> = out.syms.iter().map(|s| s.name.as_str()).collect();

        for name in [
            ".nav",
            ".bar",
            ".nav .item",
            ".bar .item",
            ".nav .item--on",
            ".bar .item--on",
            ".nav:hover",
            ".nav > .kid",
            ".bar + .next",
            ".nav .deep",
            ".root",
        ] {
            assert!(names.contains(&name), "{name} in {names:?}");
        }

        assert!(!names.iter().any(|name| name.contains('&')), "{names:?}");
    }

    #[test]
    fn rules_in_at_rules_are_definitions_and_own_what_they_use() {
        let out = run(
            "@media (min-width: 1px) { .w { top: $a; } }\n@layer base { .l { top: 0; } }\n@supports (display: grid) { .s { top: 0; } }\n@container (min-width: 1px) { .c { top: 0; } }\n",
        );

        assert_eq!(
            defs(&out),
            [("class", ".w"), ("class", ".l"), ("class", ".s"), ("class", ".c")]
        );
        assert_eq!(out.refs.first().and_then(|r| r.from), Some(0), "{:?}", out.refs);
    }

    #[test]
    fn keyframes_are_a_definition_and_their_frames_are_not() {
        let out = run("@keyframes spin { from { top: $a; } 50% { top: 1px; } }\n");

        assert_eq!(defs(&out), [("const", "spin")]);
        assert_eq!(out.refs.first().and_then(|r| r.from), Some(0), "{:?}", out.refs);
    }

    #[test]
    fn a_selector_whose_ampersand_nothing_explains_is_left_out() {
        let out = run("@mixin m { &:hover { top: $a; } .in { top: 0; } }\n");

        assert_eq!(defs(&out), [("macro", "m"), ("class", ".in")]);
    }

    #[test]
    fn lists_inside_lists_stay_bounded() {
        let list = (0..40).map(|i| format!(".s{i}")).collect::<Vec<_>>().join(", ");
        let out = run(&format!("{list} {{ .x {{ top: 0; }} }}\n"));

        assert_eq!(out.syms.len(), 40 + MAX_SCOPE);
    }

    fn names(src: &str) -> Vec<String> {
        run(src).syms.iter().map(|s| s.name.clone()).collect()
    }

    #[test]
    fn parentheses_in_a_selector_cost_nothing() {
        let src = ".u:has(a:not(:b)) { top: var(--x); }\n.g { top: 0; }\n.h:is(.m, :not(.n[a=\")\"])):not(:w),\n.i { top: calc((1px + var(--y)) * 2); }\n";
        let out = run(src);
        let lines: Vec<_> = out.syms.iter().map(|s| (s.name.as_str(), s.line)).collect();

        assert_eq!(
            lines,
            [
                (".u:has(a:not(:b))", 1),
                (".g", 2),
                (".h:is(.m, :not(.n[a=\")\"])):not(:w)", 3),
                (".i", 4),
            ]
        );

        let used: Vec<_> = out.refs.iter().map(|r| r.name.as_str()).collect();

        assert_eq!(used, ["--x", "calc", "--y"], "values are read as written");
    }

    #[test]
    fn a_non_ascii_name_is_kept_as_written() {
        assert_eq!(
            names("\u{feff}.é, .日本 { .ü { content: \"ñ\"; } }\n.after { top: 0; }\n"),
            [".é", ".日本", ".é .ü", ".日本 .ü", ".after"]
        );
    }

    #[test]
    fn what_looks_like_a_rule_in_a_comment_or_string_masks_nothing() {
        assert_eq!(
            names("/* .x:has(a:not(:b)) { */\n// don't (( {\n.a[t='q(('] { top: 0; }\n.b { top: 0; }\n"),
            [".a[t='q((']", ".b"]
        );
        assert!(!holds_a_body(".btn-#{$n}"));
        assert!(!holds_a_body("[a=\"{\"]"));
        assert!(holds_a_body(".a { top: 0; } .b"));
    }

    #[test]
    fn a_misread_selector_gives_no_name_with_a_body_in_it() {
        let out = run(
            ".f i:hover:not(:d):not(:e),\n.f t:hover:not(:d):not(:e),\n.u:has(i:hover:not(:d)):not(:w) {\n  border-color: var(--a);\n}\n\n.f i:d,\n.f t:d {\n  cursor: x;\n  opacity: 0.6;\n}\n\n.after { top: 0; }\n",
        );

        assert!(
            out.syms.iter().all(|s| !s.name.contains(['{', '}'])),
            "{:?}",
            defs(&out)
        );
    }

    #[test]
    fn an_interpolated_selector_keeps_its_name() {
        let out = run(".btn-#{$n}, .x { .b-#{$m} { top: 0; } }\n");
        let names: Vec<_> = out.syms.iter().map(|s| s.name.as_str()).collect();

        assert_eq!(names, [".btn-#{$n}", ".x", ".btn-#{$n} .b-#{$m}", ".x .b-#{$m}"]);
    }

    #[test]
    fn a_selector_said_twice_is_one_definition() {
        let out = run(".a, .a { &, & { top: 0; } &, &-b { .c { top: 0; } } }\n");

        assert_eq!(
            defs(&out),
            [
                ("class", ".a"),
                ("class", ".a-b"),
                ("class", ".a .c"),
                ("class", ".a-b .c")
            ]
        );
    }

    #[test]
    fn keyframes_in_a_rule_leave_what_they_use_to_the_rule() {
        let out = run(".a { @keyframes k { from { top: $q; } } }\n");

        assert_eq!(defs(&out), [("class", ".a"), ("const", "k")]);
        assert_eq!(out.refs.first().and_then(|r| r.from), Some(0), "{:?}", out.refs);
    }

    #[test]
    fn a_selector_nested_past_the_longest_name_is_not_built() {
        let long = format!(".{}", "a".repeat(MAX_STR - 2));
        let out = run(&format!("{long} {{ .b {{ top: 0; }} }}\n"));

        assert_eq!(out.syms.len(), 1);
        assert_eq!(out.dropped, 0);
    }

    #[test]
    fn what_is_nested_in_a_rule_without_a_name_has_none_either() {
        let out = run("@mixin m { &:hover { .x { top: 0; } } }\n");

        assert_eq!(defs(&out), [("macro", "m")]);

        let long = format!(".{}", "a".repeat(MAX_STR - 2));
        let out = run(&format!(
            "{long} {{ .b {{ .c {{ top: 0; }} }} }}\n@media (x: 1) {{ .next {{ top: 0; }} }}\n"
        ));
        let names: Vec<_> = out.syms.iter().map(|s| s.name.as_str()).collect();

        assert_eq!(names, [long.as_str(), ".next"]);
    }

    #[test]
    fn the_block_of_at_root_holds_rules_named_as_its_query_says() {
        assert_eq!(
            names(".a { @at-root { .r { .s { top: 0; } } } .t { top: 0; } }\n"),
            [".a", ".r", ".r .s", ".a .t"]
        );
        assert_eq!(names("@at-root { .r { top: 0; } }\n"), [".r"]);
        assert_eq!(
            names(".a { @at-root (without: media) { .r { top: 0; } } }\n"),
            [".a", ".a .r"]
        );
        assert_eq!(
            names(".a { @at-root (without: rule) { .r { top: 0; } } }\n"),
            [".a", ".r"]
        );
        assert_eq!(
            names(".a { @at-root (with: media) { .r { top: 0; } } }\n"),
            [".a", ".r"]
        );
    }

    #[test]
    fn what_is_written_around_a_rule_does_not_change_what_it_reads() {
        let used = |src: &str| run(src).refs.iter().map(|r| r.name.clone()).collect::<Vec<_>>();

        assert_eq!(
            used("// c\n@media (min-width: calc((1px + $a))) { .w { top: 0; } }\n"),
            used("@media (min-width: calc((1px + $a))) { .w { top: 0; } }\n"),
        );
        assert!(used("/* c */ @if (($a and $b) or $c) { .x { top: 0; } }\n").contains(&"$a".to_string()));
        assert!(used("// c\n@each $k, $v in (a: (b: $g)) { .y { top: 0; } }\n").contains(&"$g".to_string()));
        assert!(
            used(".a{top:calc((1px + $m)*2)}\n").contains(&"$m".to_string()),
            "a value without a space"
        );

        for comment in ["// it's one", "// a ( b", "/* it's ( */"] {
            let out = run(&format!(
                "$m: (\n  a: 1, {comment}\n  b: 2,\n);\n.u:has(a:not(:b)) {{ top: $z; }}\n.end {{ top: 0; }}\n"
            ));
            let names: Vec<_> = out.syms.iter().map(|s| s.name.as_str()).collect();

            assert!(
                names.contains(&".u:has(a:not(:b))") && names.contains(&".end"),
                "{comment}: {names:?}"
            );
            assert!(out.refs.iter().any(|r| r.name == "$z"), "{comment}");
        }
    }

    #[test]
    fn hostile_arguments_stay_linear_and_keep_the_length() {
        for src in [
            ":not(".repeat(40_000),
            ":not(a(b".repeat(20_000),
            ":is('".repeat(30_000),
            "é:not(#{".repeat(20_000),
        ] {
            assert_eq!(for_the_grammar(&src).len(), src.len());
        }

        assert_eq!(for_the_grammar(":not(a:not(\n:b)) {"), ":not(a:not_\n___) {");
        assert_eq!(for_the_grammar("top:calc((1px))"), "top:calc((1px))");
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
