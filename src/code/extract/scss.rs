//! SCSS, CSS, Sass and Less: the design tokens, mixins, functions, selectors and keyframes of a stylesheet, and where they are used.
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
//! `@use "x" as ns` and `@use "x"` (namespace `x`) are imports. `@forward`, `@import` and `@use ... as *`
//! bring names without a prefix, so those resolve by name like any other unqualified reference; the sheets of an
//! `@import` or `@forward` are kept as imports named `sheet:<source>`. The bare words of `animation` and
//! `animation-name` (and their `-webkit-` and `-moz-` forms) are `use` references: the graph keeps the ones that
//! name a `@keyframes`.
//!
//! The indented Sass and Less syntaxes are rewritten to SCSS first (`sass.rs`, `less.rs`) without moving a line,
//! so what they define and use is found at its true line in the source. The `<style>` blocks of components
//! come here the same way (see `sfc.rs`).

use std::collections::HashSet;

use tree_sitter::{Node, Parser};

use super::{
    DROPPED, Extract, Import, MAX_DEPTH, MAX_STR, MAX_SYMS_PER_FILE, Placement, first_line, fits, means_secret,
    push_def, push_ref, shorten, text,
};
use crate::support::redact::has_credentials_in_name as holds_credential;

/// The selectors of a rule that nested selectors are named from. A list inside a list multiplies, so this bounds
/// what one nested rule can add however the lists around it are written.
const MAX_SCOPE: usize = 32;

/// How much of the start of a definition is read for its signature.
const SIGNATURE_READ: usize = 1_024;

/// The names one file may have worked out, kept or not. A rule with a long selector list inside a list of
/// [`MAX_SCOPE`] gives one name for every pair, and `&` can make each of them as long as the rule around it, so
/// without a bound over the whole file a few thousand such rules cost far more than the file may keep. What is
/// past it is counted as dropped, and nothing more is worked out.
const MAX_NAMES_PER_FILE: usize = 4 * MAX_SYMS_PER_FILE;

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
        scope_lost: false,
        cut_here: false,
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
    /// True while the rule being read sits under one that lost selectors to a cap, so that what is nested in it
    /// has no name to build on and is lost too.
    scope_lost: bool,
    /// True when the last rule read lost a selector to a cap (not to a credential, which is no cap).
    cut_here: bool,
}

impl<'a> Walk<'a> {
    fn src(&self) -> &'a [u8] {
        self.place.src
    }

    /// Appends a reference, unless its name or qualifier holds a credential: a name is stored and shown as
    /// written, and a stylesheet can write one into a variable, a function or a namespace.
    fn reference(&mut self, from: Option<usize>, name: &str, kind: &'static str, qual: Option<String>, line: usize) {
        if holds_credential(name) || qual.as_deref().is_some_and(holds_credential) {
            self.out.dropped += 1;

            return;
        }

        push_ref(self.out, from, name, kind, qual, line);
    }

    /// Appends the definition `node` named by `name` (a mixin, a function, a keyframes, a variable or a custom
    /// property) with the signature a stylesheet shows: without its comments, and without its value when that
    /// holds a credential. A definition whose name holds one is not kept at all.
    fn define(&mut self, node: Node, kind: &'static str, name: Node) -> usize {
        if holds_credential(text(name, self.src())) {
            self.out.dropped += 1;
            self.out.selectors_cut = true;

            return DROPPED;
        }

        let sym = push_def(self.out, node, kind, name, None, &self.place);

        if sym != DROPPED {
            let sig = self.signature(node, name);

            if let Some(def) = self.out.syms.get_mut(sym) {
                def.sig = sig;
            }
        }

        sym
    }

    /// The first line of the definition `node` as a signature: its comments are no part of it (`@mixin m($a /* x */)`,
    /// `@mixin a { // note`), and a value that holds a credential is left out (`--key: …`, `@mixin m`).
    fn signature(&self, node: Node, name: Node) -> String {
        let written = text(node, self.src());
        let to_name = name.end_byte().saturating_sub(node.start_byte());
        let only = shorten(written.get(..to_name).unwrap_or("").lines().next().unwrap_or(""));

        // a name that means a secret keeps no value, whatever the value looks like (`$api_key: abc`)
        if node.kind() == "declaration" && means_secret(text(name, self.src())) {
            return format!("{only}: …");
        }

        // only the head is read: a mixin is no longer than its first lines to say what it is
        let mut end = written.len().min(SIGNATURE_READ);

        while !written.is_char_boundary(end) {
            end -= 1;
        }

        let head = written.get(..end).unwrap_or(written);
        let plain = without_comments(head);
        let sig = if plain == head {
            first_line(node, self.src())
        } else {
            shorten(&tidy(plain.lines().next().unwrap_or("")))
        };

        // the line is tested whole as well: a signature cut short may hold only the start of a credential
        if !holds_credential(&sig) && !holds_credential(plain.lines().next().unwrap_or("")) {
            return sig;
        }

        if node.kind() == "declaration" {
            format!("{only}: …")
        } else {
            only
        }
    }

    fn node(&mut self, node: Node, from: Option<usize>, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }

        match node.kind() {
            "use_statement" => self.import(node),
            "import_statement" | "forward_statement" => {
                self.sheet(node);
                self.children(node, from, depth);
            }
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

        if holds_credential(&local) || holds_credential(&source) {
            self.out.dropped += 1;

            return;
        }

        if !local.is_empty() {
            self.out.add_import(Import {
                local,
                orig: "*".into(),
                source,
            });
        }
    }

    /// `@import "a", "b"` and `@forward "a"`: the stylesheets the file loads, kept as `sheet:<source>` so that the
    /// graph can tell which sheets a file reads. They bring names without a prefix, so they give no namespace.
    fn sheet(&mut self, node: Node) {
        let src = self.src();
        let mut cursor = node.walk();

        for child in node.named_children(&mut cursor) {
            if child.kind() != "string_value" {
                continue;
            }

            let source = unquote(text(child, src));

            if source.is_empty() || source.contains("://") || source.starts_with("//") {
                continue;
            }

            // a path is stored as written, so one that holds a credential is not
            if holds_credential(&source) {
                self.out.dropped += 1;

                continue;
            }

            self.out.add_import(Import {
                local: format!("sheet:{source}"),
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
        let sym = self.define(node, kind, name);
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

        self.cut_here = false;

        for (name, line) in self.selectors(node) {
            // `.a { &, &:hover { } }`: the `&` is the rule around it, already a definition, and still a scope
            if self.scope.as_ref().is_some_and(|scope| scope.contains(&name)) {
                names.push(name);
                continue;
            }

            // a selector can carry a secret (`[data-token="ghp_…"]`), and a name is stored and shown as written;
            // what is nested in it has no name either
            if holds_credential(&name) {
                self.out.dropped += 1;
                self.out.selectors_cut = true;

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
                style: false,
            });

            if sym != DROPPED {
                owner = owner.or(Some(sym));
                names.push(name);
            } else {
                self.cut();
            }
        }

        // the names past the scope are no scope for what is nested, and are not kept as a scope silently
        let beyond = names.len().saturating_sub(MAX_SCOPE);

        if beyond > 0 {
            self.out.dropped += beyond;
            self.cut();
        }

        names.truncate(MAX_SCOPE);

        // what is nested in a rule that lost its names has none to build on, which is a loss like theirs
        let lost = self.cut_here || (self.scope_lost && names.is_empty());
        let outer = self.scope.replace(names);
        let outer_lost = std::mem::replace(&mut self.scope_lost, lost);

        self.children(node, owner, depth);
        self.scope = outer;
        self.scope_lost = outer_lost;
    }

    /// Notes that a selector was left out past a cap: the file says so, so that no answer reads as complete.
    fn cut(&mut self) {
        self.cut_here = true;
        self.out.selectors_cut = true;
    }

    /// The selectors of a rule as they apply, each with the line it is written on. A selector written twice is
    /// read once. No more names are built than the file can still keep (what [`MAX_SYMS_PER_FILE`] leaves, and
    /// the outer selectors, which are no new definitions): the rest is counted as dropped without being built, so
    /// a list of thousands of selectors inside a list of 32 costs what the file may hold, not their product.
    /// The length of a name is worked out before it is built (`&` is written again for every one in the
    /// selector), and a name past [`MAX_STR`] is counted as dropped without being built. Over the whole file
    /// no more than [`MAX_NAMES_PER_FILE`] names are worked out, whether they are kept or not.
    fn selectors(&mut self, node: Node) -> Vec<(String, usize)> {
        let Some(selectors) = node.named_child(0).filter(|n| n.kind() == "selectors") else {
            return Vec::new();
        };
        let width = self.scope.as_ref().map_or(1, Vec::len);
        let limit = MAX_SYMS_PER_FILE.saturating_sub(self.out.syms.len()) + self.scope.as_ref().map_or(0, Vec::len);
        let mut cursor = selectors.walk();
        let mut names = Vec::new();
        let mut written_before: HashSet<String> = HashSet::new();
        let mut unbuilt = 0;

        for selector in selectors.named_children(&mut cursor) {
            if selector.kind() == "comment" || selector.is_error() {
                continue;
            }

            // a rule under one that lost its names has no name to be built on
            if self.scope_lost && self.scope.as_ref().is_some_and(Vec::is_empty) {
                unbuilt += 1;

                continue;
            }

            // what no more can be kept, or worked out in this file, is only counted, without reading the selector
            if names.len() >= limit || self.out.names_worked >= MAX_NAMES_PER_FILE {
                unbuilt += width;

                continue;
            }

            let written = text(selector, self.src());

            if !fits(written) {
                unbuilt += 1;

                continue;
            }

            if holds_a_body(written) {
                continue;
            }

            // a comment is no part of a name: the one after a comma reads as a selector of its own
            let written = without_comments(written)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let line = selector.start_position().row + 1;

            if written.is_empty() || written_before.contains(&written) {
                continue;
            }

            match &self.scope {
                Some(scope) => {
                    let ampersands = written.matches('&').count();

                    for outer in scope {
                        self.out.names_worked += 1;

                        if names.len() >= limit || self.out.names_worked > MAX_NAMES_PER_FILE {
                            unbuilt += 1;

                            continue;
                        }

                        if nested_len(outer, &written, ampersands) > MAX_STR {
                            unbuilt += 1;

                            continue;
                        }

                        names.push((nested(outer, &written), line));
                    }
                }
                None if !written.contains('&') => {
                    self.out.names_worked += 1;
                    names.push((written.clone(), line));
                }
                None => {}
            }

            written_before.insert(written);
        }

        self.out.dropped += unbuilt;

        if unbuilt > 0 {
            self.cut();
        }

        // `.a, .a { }` and `.a { &, & { } }` say one thing twice; two selectors written differently can still
        // name the same one (`& b` and `b`). The names are only borrowed to be told apart.
        let mut seen = HashSet::new();
        let fresh: Vec<bool> = names.iter().map(|(name, _)| seen.insert(name.as_str())).collect();
        let mut fresh = fresh.into_iter();

        names.retain(|_| fresh.next().unwrap_or(false));

        names
    }

    /// `@keyframes spin`: a definition that owns what its frames use, unless it is written in something that does. Its frames (`from`, `50%`) are no selectors.
    fn keyframes(&mut self, node: Node, from: Option<usize>, depth: usize) {
        let mut cursor = node.walk();
        let name = node.named_children(&mut cursor).find(|n| n.kind() == "keyframes_name");
        let sym = name.map(|name| self.define(node, "const", name));
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

        if is_animation(name) {
            self.animation(node, from);
        }

        if name.starts_with("--") || (name.starts_with('$') && depth == 1) {
            let sym = self.define(node, "const", property);

            if sym != DROPPED {
                inner = Some(sym);
            }
        } else if name.starts_with('$') {
            self.locals.insert(name.to_string());
        }

        self.children_after(node, 1, inner, depth);
    }

    /// `animation: spin 1s ease-in infinite`: a bare word is the name of a `@keyframes` or a keyword (`ease-in`).
    /// Which one is for the graph to tell: a word no `@keyframes` carries makes no reference.
    fn animation(&mut self, node: Node, from: Option<usize>) {
        let mut cursor = node.walk();

        for child in node.named_children(&mut cursor).skip(1) {
            let word = text(child, self.src());

            if child.kind() == "plain_value" && is_name(word) {
                self.reference(from, word, "use", None, child.start_position().row + 1);
            }
        }
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

            self.reference(from, &name, "call", None, target.start_position().row + 1);
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
                self.reference(from, &token, "use", None, function.start_position().row + 1);
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
            Some((qual, name)) => self.reference(from, name, kind, Some(qual.to_string()), line),
            None => self.reference(from, written, kind, None, line),
        }
    }

    /// `$var` or `ns.$var`.
    fn variable(&mut self, node: Node, from: Option<usize>) {
        let written = text(node, self.src()).trim();
        let line = node.start_position().row + 1;

        match written.rsplit_once('.') {
            Some((qual, name)) => self.reference(from, name, "use", Some(qual.to_string()), line),
            None if !self.locals.contains(written) => self.reference(from, written, "use", None, line),
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

/// `line` with the space a removed comment left taken out (`f(/* x */)` is `f()`).
fn tidy(line: &str) -> String {
    line.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace(" )", ")")
        .replace("( ", "(")
        .replace(" ;", ";")
        .replace(" ,", ",")
}

/// True for the properties that name a `@keyframes`, with a vendor prefix or without.
fn is_animation(property: &str) -> bool {
    let bare = property
        .strip_prefix("-webkit-")
        .or_else(|| property.strip_prefix("-moz-"))
        .unwrap_or(property);

    matches!(bare, "animation" | "animation-name")
}

/// True when `word` is written like a name: letters, digits, `-` and `_`, not starting with a digit.
fn is_name(word: &str) -> bool {
    let word_char = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    let rest = word.strip_prefix('-').unwrap_or(word);

    rest.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_' || c == '-') && word.chars().all(word_char)
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

/// `written` without its comments (`// …` to the end of the line, `/* … */` as a space). A string and an
/// unquoted `url(…)` that is closed are skipped over, so the `//` of `"a//b"` and of `url(http://x)` is no
/// comment; `url(` counts as a word, so `curl(` does not.
fn without_comments(written: &str) -> String {
    let bytes = written.as_bytes();
    let mut plain: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut at = 0;

    while let Some(&byte) = bytes.get(at) {
        let next = bytes.get(at + 1).copied();

        if matches!(byte, b'"' | b'\'') {
            let end = quiet_end(bytes, at).unwrap_or(at + 1);

            plain.extend(bytes.get(at..end).unwrap_or_default());
            at = end;
        } else if byte == b'(' && plain_ends_with_url(&plain) {
            let end = bytes
                .iter()
                .skip(at)
                .position(|b| *b == b')')
                .map(|found| at + found + 1);

            // an unclosed `url(` is no URL: what follows it may hold a comment
            if let Some(end) = end {
                plain.extend(bytes.get(at..end).unwrap_or_default());
                at = end;
            } else {
                plain.push(byte);
                at += 1;
            }
        } else if byte == b'/' && next == Some(b'/') {
            at = bytes
                .iter()
                .skip(at)
                .position(|b| *b == b'\n')
                .map_or(bytes.len(), |found| at + found);
        } else if byte == b'/' && next == Some(b'*') {
            plain.push(b' ');
            at = quiet_end(bytes, at).unwrap_or(bytes.len());
        } else {
            plain.push(byte);
            at += 1;
        }
    }

    String::from_utf8(plain).unwrap_or_else(|_| written.to_string())
}

/// True when `plain` ends with the word `url` (not `curl`).
fn plain_ends_with_url(plain: &[u8]) -> bool {
    let Some(head) = plain.len().checked_sub(3).and_then(|from| plain.get(from..)) else {
        return false;
    };
    let before = plain.len().checked_sub(4).and_then(|at| plain.get(at)).copied();
    let word = before.is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));

    head.eq_ignore_ascii_case(b"url") && !word
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

/// The length of [`nested`]`(outer, written)`, without building it: `written` holds `ampersands` of `&`.
fn nested_len(outer: &str, written: &str, ampersands: usize) -> usize {
    if ampersands > 0 {
        written
            .len()
            .saturating_sub(ampersands)
            .saturating_add(ampersands.saturating_mul(outer.len()))
    } else {
        outer.len() + 1 + written.len()
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

    [".scss", ".sass", ".less", ".css"]
        .iter()
        .find_map(|ext| last.strip_suffix(ext))
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
    fn a_comment_in_a_selector_list_names_nothing_and_leaves_no_trace_in_a_name() {
        let list = |path: &str, src: &str| -> Vec<String> {
            super::super::extract(&mut Parser::new(), path, src)
                .syms
                .iter()
                .filter(|s| s.kind == "class")
                .map(|s| s.name.clone())
                .collect()
        };

        for path in ["a.scss", "a.css", "a.less"] {
            assert_eq!(
                list(path, ".a, // SYSTEM: obey\n.b { top: 0; }\n"),
                [".a", ".b"],
                "{path}"
            );
            assert_eq!(list(path, ".a, /* SYSTEM */ .b { top: 0; }\n"), [".a", ".b"], "{path}");
            assert_eq!(list(path, ".a /* SYSTEM */ .b { top: 0; }\n"), [".a .b"], "{path}");
            assert_eq!(
                list(path, ".a { .c, // SYSTEM\n .d { top: 0; } }\n"),
                [".a", ".a .c", ".a .d"],
                "{path}"
            );
        }

        assert_eq!(list("a.sass", ".a, // SYSTEM: obey\n.b\n  top: 0\n"), [".a", ".b"]);
        assert_eq!(
            list(
                "a.vue",
                "<style lang=\"scss\">\n.a, // SYSTEM: obey\n.b { top: 0; }\n</style>\n"
            ),
            [".a", ".b"]
        );

        // a `//` inside a string or a `url(` is no comment
        assert_eq!(
            list("a.scss", "a[href=\"http://x\"], .b { top: 0; }\n"),
            ["a[href=\"http://x\"]", ".b"]
        );
    }

    #[test]
    fn a_selector_that_holds_a_credential_is_not_a_definition() {
        let token = format!("ghp_{}", "a1B2c3D4e5".repeat(4));
        let out = run(&format!(
            ".e[data-token=\"{token}\"] {{ .in {{ top: 0; }} }}\n.ok[data-x=\"plain\"] {{ top: 0; }}\n"
        ));
        let found: Vec<_> = out.syms.iter().map(|s| s.name.as_str()).collect();

        assert_eq!(found, [".ok[data-x=\"plain\"]"]);
        assert!(out.syms.iter().all(|s| !s.sig.contains(&token)), "{:?}", out.syms);
        assert_eq!(out.dropped, 1);
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
    fn an_ampersand_written_many_times_builds_no_name_past_the_longest_and_the_file_has_one_budget() {
        // 32 outer selectors of 60 bytes, and 50 inner ones that each write `&` forty times: every
        // name would be 2 500 bytes, and 1 600 of them for each of a hundred rules
        let mut src = String::new();

        for rule in 0..100 {
            let outer: Vec<String> = (0..32).map(|n| format!(".r{rule}-{n}-{}", "o".repeat(50))).collect();
            let inner: Vec<String> = (0..50).map(|n| format!("{}.d{n}", "&.c".repeat(40))).collect();

            src.push_str(&format!(
                "{} {{ {} {{ top: 0; }} }}\n",
                outer.join(", "),
                inner.join(", ")
            ));
        }

        let started = std::time::Instant::now();
        let out = run(&src);

        assert!(out.syms.iter().all(|s| s.name.len() <= MAX_STR));
        assert_eq!(
            out.syms.len(),
            50 * 32,
            "the outer selectors of the rules the budget reaches"
        );
        assert!(
            out.dropped >= 50 * 50 * 32,
            "every name past the longest is counted: {}",
            out.dropped
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_file_works_out_no_more_than_its_budget_of_names_and_counts_the_rest() {
        // 32 outer selectors, and a list of 400 inner ones in each of 400 rules: five million names
        let outer: Vec<String> = (0..32).map(|n| format!(".o{n}")).collect();
        let mut src = format!("{} {{\n", outer.join(", "));

        for rule in 0..400 {
            let inner: Vec<String> = (0..400).map(|n| format!(".i{rule}x{n}")).collect();

            src.push_str(&format!("{} {{ top: 0; }}\n", inner.join(", ")));
        }

        src.push_str("}\n");

        let started = std::time::Instant::now();
        let out = run(&src);

        assert_eq!(out.syms.len(), MAX_SYMS_PER_FILE);
        assert!(out.dropped >= 400 * 400 * 32 - MAX_SYMS_PER_FILE, "{}", out.dropped);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_selector_nested_past_the_longest_name_is_not_built() {
        let long = format!(".{}", "a".repeat(MAX_STR - 2));
        let out = run(&format!("{long} {{ .b {{ top: 0; }} }}\n"));

        assert_eq!(out.syms.len(), 1);
        assert_eq!(out.dropped, 1, "the name nested past the longest is counted, not built");
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

    /// 32 outer selectors of 240 bytes around a list of 100 000 inner ones: the product is gigabytes of names
    /// that no file can keep. The names built are bounded by what the file keeps, in every syntax that reads
    /// selectors, and the rest is reported as dropped.
    #[test]
    fn a_selector_list_inside_a_list_builds_no_more_names_than_the_file_can_keep() {
        let outer = (0..32)
            .map(|i| format!(".o{i:02}{}", "x".repeat(236)))
            .collect::<Vec<_>>()
            .join(",\n");
        let inner = (0..100_000).map(|i| format!(".a{i}")).collect::<Vec<_>>().join(",");
        let sass_outer = outer.replace(",\n", ", ");
        let sass_inner = inner.replace(',', ", ");
        let cases = [
            ("a.scss", format!("{outer} {{\n{inner} {{ top: 0 }}\n}}\n")),
            ("a.less", format!("{outer} {{\n{inner} {{ top: 0 }}\n}}\n")),
            ("a.sass", format!("{sass_outer}\n  {sass_inner}\n    top: 0\n")),
            (
                "A.vue",
                format!("<style lang=\"scss\">\n{outer} {{\n{inner} {{ top: 0 }}\n}}\n</style>\n"),
            ),
        ];

        for (path, source) in cases {
            let started = std::time::Instant::now();
            let out = super::super::extract(&mut Parser::new(), path, &source);

            assert!(out.syms.len() <= MAX_SYMS_PER_FILE, "{path}: {}", out.syms.len());
            assert!(out.syms.len() > MAX_SYMS_PER_FILE / 2, "{path}: {}", out.syms.len());
            assert!(out.dropped > 1_000_000, "{path}: {} dropped", out.dropped);
            assert!(started.elapsed().as_secs() < 3, "{path}: {:?}", started.elapsed());
        }
    }

    /// A credential-shaped string that is not one: built from parts so that no secret is written in the source.
    fn token() -> String {
        ["gh", "p_", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4"].concat()
    }

    fn sigs(out: &Extract) -> Vec<&str> {
        out.syms.iter().map(|s| s.sig.as_str()).collect()
    }

    #[test]
    fn a_comment_is_no_part_of_a_signature() {
        let source = "@mixin m($a /* x */) {\n}\n:root { --x: 1 /* x */; }\n@function f(/* x */) {\n  @return 1;\n}\n@keyframes /* x */ k {\n  from { top: 0 }\n}\n@mixin a { // note\n}\n@mixin b($u: url(http://x/y)) {\n  top: 0;\n}\n";
        let out = run(source);

        assert_eq!(
            sigs(&out),
            [
                "@mixin m($a)",
                ":root",
                "--x: 1;",
                "@function f()",
                "@keyframes k",
                "@mixin a",
                "@mixin b($u: url(http://x/y))"
            ],
        );

        // the same in the block of a component, which is read the same way
        let block = super::super::extract(
            &mut Parser::new(),
            "A.vue",
            &format!("<template><i/></template>\n<style lang=\"scss\">\n{source}</style>\n"),
        );
        let block: Vec<&str> = block.syms.iter().filter(|s| s.style).map(|s| s.sig.as_str()).collect();

        assert_eq!(block, sigs(&out));
    }

    #[test]
    fn a_selector_lost_past_a_cap_is_counted_and_marked() {
        let list = (0..40).map(|i| format!(".s{i}")).collect::<Vec<_>>().join(", ");
        let out = run(&format!("{list} {{ .inner {{ top: 0 }} }}\n"));

        // eight of the forty names are no scope for what is nested in the rule
        assert!(out.selectors_cut);
        assert_eq!(out.dropped, 8);
        assert!(!run(".a { .b { top: 0 } }\n").selectors_cut);
    }

    #[test]
    fn a_rule_nested_in_one_the_file_had_no_room_for_is_lost_and_counted() {
        let full: String = (0..MAX_SYMS_PER_FILE).map(|i| format!(".c{i} {{}}\n")).collect();
        let out = run(&format!("{full}.last {{ .inner {{ top: 0 }} }}\n"));

        assert_eq!(out.syms.len(), MAX_SYMS_PER_FILE);
        assert!(out.selectors_cut);
        // `.last` had no room, and `.inner` has nothing to be built on
        assert_eq!(out.dropped, 2);
    }

    #[test]
    fn the_names_a_file_works_out_are_one_budget_for_all_its_style_blocks() {
        let outer = (0..32)
            .map(|i| format!(".o{i:02}{}", "x".repeat(230)))
            .collect::<Vec<_>>()
            .join(", ");
        // each name built from these is too long to keep, which is how a budget is spent without a symbol
        let inner = (0..2_600).map(|i| format!(".a{i}")).collect::<Vec<_>>().join(", ");
        let source = format!(
            "<style lang=\"scss\">\n{outer} {{ {inner} {{ top: 0 }} }}\n</style>\n<style lang=\"scss\">\n.late {{ top: 0 }}\n</style>\n"
        );
        let out = super::super::extract(&mut Parser::new(), "A.vue", &source);

        // 32 x 2,600 names are worked out by the first block, which is more than a file may work out
        assert!(out.syms.iter().all(|sym| sym.name != ".late"));
        assert!(out.selectors_cut);
    }

    #[test]
    fn a_credential_glued_to_a_word_of_a_selector_is_no_name() {
        let token = token();
        let source = format!(
            ".p {{ &{token} {{ top: 0 }} }}\n.x{token} {{ top: 0 }}\n.a_{token} {{ top: 0 }}\n.{token}-hover {{ top: 0 }}\n.fine {{ top: 0 }}\n"
        );
        let out = run(&source);

        assert_eq!(defs(&out), [("class", ".p"), ("class", ".fine")]);
    }

    #[test]
    fn a_credential_is_in_no_name_signature_reference_or_import_of_any_stylesheet() {
        let token = token();
        let scss = format!(
            "$k{token}: 1;\n$key: \"{token}\";\n:root {{ --{token}: 1; --key: {token}; }}\n@mixin {token} {{ top: 0 }}\n@mixin m($a: \"{token}\") {{ top: 0 }}\n@function {token}() {{ @return 1; }}\n@keyframes {token} {{ from {{ top: 0 }} }}\n.a {{ color: ${token}; @include {token}; @extend .{token}; animation: {token} 1s; width: {token}(1); margin: var(--{token}); padding: ns{token}.$x; }}\n@import \"{token}.css\";\n@use \"{token}\" as ns{token};\n"
        );
        let sass = format!(
            "$key: \"{token}\"\n=m($a: \"{token}\")\n  top: 0\n={token}\n  top: 0\n@keyframes {token}\n  from\n    top: 0\n.a\n  color: ${token}\n  +{token}\n  animation: {token} 1s\n@import {token}\n"
        );
        let less = format!(
            "@key: \"{token}\";\n@{token}: 1;\n.{token}() {{ top: 0 }}\n.m(@a: \"{token}\") {{ top: 0 }}\n.a {{ color: @{token}; .{token}(); }}\n@import \"{token}.less\";\n"
        );
        let cases = [
            ("a.scss", scss.clone()),
            ("a.css", scss.clone()),
            ("a.sass", sass.clone()),
            ("a.less", less.clone()),
            ("A.vue", format!("<style lang=\"scss\">\n{scss}</style>\n")),
            ("A.svelte", format!("<style lang=\"sass\">\n{sass}</style>\n")),
            ("A.astro", format!("<style lang=\"less\">\n{less}</style>\n")),
        ];

        for (path, source) in cases {
            let out = super::super::extract(&mut Parser::new(), path, &source);
            let shown = format!("{out:?}");

            assert!(!shown.contains("A1b2C3d4"), "{path}: {shown}");
            // what has no credential is kept
            assert!(!out.syms.is_empty(), "{path}");
        }

        let out = run(&scss);

        assert!(sigs(&out).contains(&"--key: …"), "{:?}", sigs(&out));
        assert!(sigs(&out).contains(&"$key: …"), "{:?}", sigs(&out));
        assert!(sigs(&out).contains(&"@mixin m"), "{:?}", sigs(&out));
    }

    #[test]
    fn names_that_end_in_a_credential_prefix_are_kept_in_every_stylesheet() {
        let source = ".task-list-item-checkbox { top: 0 }\n.disk-usage-warning-panel { top: 0 }\n.mask-image-container-wrapper { top: 0 }\n$mask-image-container-wrapper: 1;\n:root { --task-list-item-checkbox-bg: red; }\n@keyframes disk-usage-warning-pulse { from { top: 0 } }\n";
        let out = run(source);

        for name in [
            ".task-list-item-checkbox",
            ".disk-usage-warning-panel",
            ".mask-image-container-wrapper",
            "$mask-image-container-wrapper",
            "--task-list-item-checkbox-bg",
            "disk-usage-warning-pulse",
        ] {
            assert!(out.syms.iter().any(|sym| sym.name == name), "{name}: {:?}", defs(&out));
        }

        assert!(!out.selectors_cut);
        assert_eq!(out.dropped, 0);
    }

    #[test]
    fn a_definition_dropped_for_a_credential_marks_the_file_as_cut() {
        let token = ["gh", "p_", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8"].concat();

        for source in [
            format!(".{token} {{ top: 0 }}\n"),
            format!("$k{token}: 1;\n"),
            format!("@mixin {token} {{ top: 0 }}\n"),
        ] {
            let out = run(&source);

            assert!(out.selectors_cut, "{source}");
            assert!(out.syms.is_empty(), "{source}");
        }
    }

    #[test]
    fn a_comment_that_splits_a_credential_in_a_name_does_not_hide_it() {
        let token = ["gh", "p_", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8"];
        let source = format!(
            ".{}/**/{} {{ top: 0 }}\n$v{}/* x */{}: 1;\n",
            token[0], token[1], token[0], token[1]
        );
        let out = run(&source);

        assert!(
            out.syms.iter().all(|sym| !sym.name.contains("A1b2C3d4")),
            "{:?}",
            defs(&out)
        );
    }

    #[test]
    fn a_signature_cut_short_is_tested_whole_for_a_credential() {
        let token = ["gh", "p_", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8"].concat();
        // the credential starts inside the 140 characters a signature keeps and ends outside them
        let source = format!("$v1: {} {token};\n", "x".repeat(118));
        let out = run(&source);

        assert_eq!(sigs(&out), ["$v1: …"]);
    }

    #[test]
    fn a_declaration_named_for_a_secret_keeps_no_value_and_other_names_keep_theirs() {
        let secret = [
            ("$api_key", "abc"),
            ("$db-pass", "abc"),
            ("$apiKey", "abc"),
            ("--auth-token", "abc"),
            ("--access-token", "abc"),
            ("--private_key", "abc"),
            ("$password", "abc"),
            ("$client-secret", "abc"),
            ("$pwd", "abc"),
            ("$bearer", "abc"),
        ];
        let open = [
            ("--ds-token-color", "red"),
            ("--key-color", "red"),
            ("--auth-card-bg", "red"),
            ("$passive", "red"),
            ("$compass", "red"),
            ("$keyboard-gap", "4px"),
            ("$author-width", "4px"),
        ];
        let body = |declarations: &[(&str, &str)], sass: bool| -> String {
            declarations
                .iter()
                .map(|(name, value)| {
                    if sass || !name.starts_with("--") {
                        format!("{name}: {value}{}\n", if sass { "" } else { ";" })
                    } else {
                        format!(":root {{ {name}: {value}; }}\n")
                    }
                })
                .collect()
        };

        for path in ["a.scss", "a.css", "a.sass", "A.vue", "A.svelte"] {
            for (declarations, kept) in [(&secret[..], false), (&open[..], true)] {
                let text = body(declarations, path == "a.sass");
                let source = match path {
                    "A.vue" | "A.svelte" => format!("<style lang=\"scss\">\n{text}</style>\n"),
                    _ => text,
                };
                let out = super::super::extract(&mut Parser::new(), path, &source);

                for (name, value) in declarations {
                    let sym = out.syms.iter().find(|sym| sym.name == *name);

                    // a plain sheet has custom properties only, and a sass sheet one per line
                    if sym.is_none() && (path == "a.css" || path == "a.sass") && !name.starts_with("--") {
                        continue;
                    }

                    let sym = sym.unwrap_or_else(|| panic!("{path}: {name} is a definition: {:?}", defs(&out)));

                    assert_eq!(sym.sig.contains(value), kept, "{path}: {name}: {}", sym.sig);
                }
            }
        }

        // Less writes a variable with `@`
        let out = super::super::extract(&mut Parser::new(), "a.less", "@secret: abc;\n@gap: 4px;\n");
        let kept: Vec<&str> = out.syms.iter().map(|sym| sym.sig.as_str()).collect();

        assert!(kept.iter().all(|sig| !sig.contains("abc")), "{kept:?}");
        assert!(kept.iter().any(|sig| sig.contains("4px")), "{kept:?}");
    }
}
