//! CSS class tokens written in code.
//!
//! A class a component uses is a word in a string, and a string is no reference, so the places where a class
//! may be written are listed: the `class` and `className` of an element or an object, the arguments of the
//! helpers that join class names (`clsx`, `cn`, ...), and a member of an imported stylesheet module
//! (`styles.card`). A string anywhere else (`const s = 'card'`, `title="card"`) says nothing about a class.
//!
//! [`class_expression`] reads one such place. It follows only what a class name can come from (a string, the
//! branches of a ternary, the right side of `&&`, the keys of an object, ...) and never a condition, a variant
//! name or a part of a template literal that touches a `${}`, so a token is always a name that was written out.

use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

use super::{Import, means_secret, out_of_time, text};
use crate::code::graph::classes::selector_tokens;

/// The tokens one file may hold; a file past it keeps its first ones and is marked (see
/// [`super::Extract::add_class`]).
pub const MAX_CLASS_TOKENS_PER_FILE: usize = 1_000;

/// The longest class token, in bytes. A longer word is not a class name.
const MAX_TOKEN_BYTES: usize = 64;

/// Expressions nested deeper than this are not followed.
const MAX_CLASS_DEPTH: usize = 32;

/// The functions that join class names: every argument is a class position. `cva` also reads its variants.
const HELPERS: [&str; 6] = ["clsx", "cn", "classnames", "classNames", "twMerge", "cva"];

/// The extensions of a stylesheet module.
const SHEET_EXTENSIONS: [&str; 4] = ["css", "scss", "sass", "less"];

/// True for a name that can be a class: it starts with a letter, `_` or `-`, holds only letters, digits, `-`
/// and `_`, and is at most [`MAX_TOKEN_BYTES`] long. A Tailwind variant or utility (`md:flex`, `w-[10px]`,
/// `!x`, `w-1/2`) is none: the selector it stands for is not written as it is.
pub(super) fn is_token(word: &str) -> bool {
    let bytes = word.as_bytes();
    let starts = bytes
        .first()
        .is_some_and(|first| first.is_ascii_alphabetic() || matches!(first, b'_' | b'-'));

    starts
        && bytes.len() <= MAX_TOKEN_BYTES
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// The module path of a stylesheet import without its `?query` or `#hash`, or `None` when it is no stylesheet
/// (or not a file: a URL).
pub(super) fn sheet_source(source: &str) -> Option<&str> {
    let bare = source.split(['?', '#']).next().unwrap_or(source);
    let (_, extension) = bare.rsplit_once('.')?;

    (SHEET_EXTENSIONS
        .iter()
        .any(|known| extension.eq_ignore_ascii_case(known))
        && !bare.contains("://"))
    .then_some(bare)
}

/// The local names `imports` bring from a stylesheet (`import styles from './a.module.css'`).
pub(super) fn sheet_locals(imports: &[Import]) -> HashSet<String> {
    imports
        .iter()
        .filter(|import| !import.local.contains(':') && sheet_source(&import.source).is_some())
        .map(|import| import.local.clone())
        .collect()
}

/// Calls `emit(token, line)` for each class token in `text`, splitting at ASCII whitespace. `first` is the
/// line of the first byte; a word that is no token (it holds a `\0` where a template hole was) is skipped.
pub(super) fn split_tokens(text: &str, first: usize, emit: &mut dyn FnMut(&str, usize)) {
    let bytes = text.as_bytes();
    let mut line = first;
    let mut at = 0;

    while let Some(&byte) = bytes.get(at) {
        if byte == b'\n' {
            line += 1;
        }

        if byte.is_ascii_whitespace() {
            at += 1;

            continue;
        }

        let start = at;

        while bytes.get(at).is_some_and(|byte| !byte.is_ascii_whitespace()) {
            at += 1;
        }

        if let Some(word) = text.get(start..at).filter(|word| is_token(word)) {
            emit(word, line);
        }
    }
}

/// The names of the file's own helpers made with `classNames.bind(styles)`, each with the stylesheet module it
/// was bound to (`None` when it was bound to something that is no imported stylesheet).
pub(super) type Binders = HashMap<String, Option<String>>;

/// The helpers a file makes of `classNames.bind(x)` at its top level (`const cx = classNames.bind(styles)`,
/// also exported). Only the top level is read: the helper is a name of the whole file, so one defined later
/// than its use still counts, and one that a function shadows with another meaning is not told apart.
/// `classnames` is the library's own name for itself; so is the local of an import from `classnames/bind`.
pub(super) fn binders_of(root: Node, src: &[u8], sheets: &HashSet<String>, imports: &[Import]) -> Binders {
    let mut binders = Binders::new();
    // the locals of `classnames/bind`, found once: a file of thousands of declarations asks for each of them
    let bound: HashSet<&str> = imports
        .iter()
        .filter(|import| import.source == "classnames/bind")
        .map(|import| import.local.as_str())
        .collect();
    let own = |name: &str| matches!(name, "classNames" | "classnames") || bound.contains(name);
    let mut cursor = root.walk();

    for statement in root.named_children(&mut cursor) {
        let declaration = match statement.kind() {
            "export_statement" => statement.child_by_field_name("declaration"),
            _ => Some(statement),
        };
        let Some(declaration) =
            declaration.filter(|d| matches!(d.kind(), "lexical_declaration" | "variable_declaration"))
        else {
            continue;
        };
        let mut inner = declaration.walk();

        for declarator in declaration
            .named_children(&mut inner)
            .filter(|d| d.kind() == "variable_declarator")
        {
            let Some(name) = declarator
                .child_by_field_name("name")
                .filter(|name| name.kind() == "identifier")
            else {
                continue;
            };
            let Some(call) = declarator
                .child_by_field_name("value")
                .filter(|value| value.kind() == "call_expression")
            else {
                continue;
            };
            let Some(function) = call
                .child_by_field_name("function")
                .filter(|f| f.kind() == "member_expression")
            else {
                continue;
            };
            let object = function.child_by_field_name("object").map(|o| text(o, src));
            let property = function.child_by_field_name("property").map(|p| text(p, src));

            if property != Some("bind") || !object.is_some_and(own) {
                continue;
            }

            let sheet = call
                .child_by_field_name("arguments")
                .and_then(|arguments| arguments.named_child(0))
                .filter(|argument| argument.kind() == "identifier")
                .map(|argument| text(argument, src))
                .filter(|local| sheets.contains(*local))
                .map(str::to_string);

            binders.insert(text(name, src).to_string(), sheet);
        }
    }

    binders
}

/// The name a helper call is written with: a bare identifier on the helper list.
pub(super) fn helper_name<'a>(call: Node, src: &'a [u8]) -> Option<&'a str> {
    let function = call.child_by_field_name("function")?;

    if function.kind() != "identifier" {
        return None;
    }

    let name = text(function, src);

    HELPERS.contains(&name).then_some(name)
}

/// What a call is as a class helper: the name on the helper list, or `""` for one of the file's own `binders`,
/// and the stylesheet module the classes of the call belong to (only a bound helper has one).
pub(super) fn helper_of<'a>(call: Node, src: &'a [u8], binders: &'a Binders) -> Option<(&'a str, Option<&'a str>)> {
    if let Some(name) = helper_name(call, src) {
        return Some((name, None));
    }

    let function = call
        .child_by_field_name("function")
        .filter(|f| f.kind() == "identifier")?;
    let sheet = binders.get(text(function, src))?;

    Some(("", sheet.as_deref()))
}

/// Calls `emit(word, qual, row)` for each word the expression `node` writes in a class position; the receiver
/// keeps the ones that are tokens. `row` is the 0-based row in the tree; `qual` is the stylesheet module a word
/// was read from.
pub(super) fn class_expression(
    node: Node,
    src: &[u8],
    sheets: &HashSet<String>,
    emit: &mut dyn FnMut(&str, Option<&str>, usize),
) {
    class_expression_with(node, src, sheets, &Binders::new(), None, emit);
}

/// [`class_expression`] where the calls of the file's own helpers (`binders`) are class positions too. The words
/// of a bound helper carry the module it was bound to, so they reach that module and nothing else. With `vars`
/// a name that stands for a same-file constant is read as the constant's value.
pub(super) fn class_expression_with<'t>(
    node: Node<'t>,
    src: &[u8],
    sheets: &HashSet<String>,
    binders: &Binders,
    vars: Option<&mut Vars<'_, 't>>,
    emit: &mut dyn FnMut(&str, Option<&str>, usize),
) {
    Reader {
        src,
        sheets,
        binders,
        qual: None,
        selector: false,
        vars,
        pin: None,
        emit,
    }
    .read(node, 0);
}

/// Calls `emit(token, None, row)` for each `.token` of the CSS selector `node` writes (`querySelector('.a > .b')`):
/// a string, a template with no `${}`, or a name that stands for one, in either branch of a ternary. The
/// tokens are those of a stylesheet's selector, so what a selector excludes (`:not(.x)`) or quotes is no class.
pub(super) fn selector_expression<'t>(
    node: Node<'t>,
    src: &[u8],
    vars: Option<&mut Vars<'_, 't>>,
    emit: &mut dyn FnMut(&str, Option<&str>, usize),
) {
    let sheets = HashSet::new();
    let binders = Binders::new();

    Reader {
        src,
        sheets: &sheets,
        binders: &binders,
        qual: None,
        selector: true,
        vars,
        pin: None,
        emit,
    }
    .read(node, 0);
}

/// The declarations one file may put in the table of constants; past it the table is given up and the file is
/// marked, as a name the table does not know could be a shadow.
const MAX_SCOPE_ENTRIES: usize = 20_000;

/// How many constants one may be read through (`const a = 'x'; const b = a;` is one link).
const MAX_CHAIN: usize = 4;

/// The bytes of constant values one file may have read in class positions. A constant used a thousand times
/// would otherwise be read a thousand times; the cost of a read is the size of the value.
const VALUE_BUDGET: usize = 256 * 1024;

/// The cost of reading a value however small it is, so a name read many times is not free.
const MIN_READ_COST: usize = 32;

/// Nodes between two looks at the clock while the table is built.
const TABLE_CLOCK_EVERY: usize = 256;

/// Reads between two looks at the clock while the table is asked for names.
const READS_CLOCK_EVERY: usize = 256;

/// What a name is in the scope that declares it.
enum Binding<'t> {
    /// `const name = value`, declared in the scope numbered `scope`.
    Const { value: Node<'t>, scope: usize },
    /// Anything else (`let`, `var`, a parameter, a destructured name, a function, an import, ...) or a name
    /// declared twice: not followed, and it hides a constant of the same name further out.
    Other,
}

struct Scope<'t> {
    parent: Option<usize>,
    names: HashMap<String, Binding<'t>>,
}

/// The `const` declarations of one file or script block, by the scope that declares them, built in one pass
/// over the tree. A use asks for a name from the scope it sits in outwards, and the nearest declaration of the
/// name decides, so a parameter or a `let` of the same name hides a constant and a constant of one function is
/// no name in another.
pub(super) struct Constants<'t> {
    scopes: Vec<Scope<'t>>,
    /// The scope a node opens, by the node's id.
    opened: HashMap<usize, usize>,
    entries: usize,
    /// The table was not built to the end (too many entries, or no time): a name it does not know proves nothing.
    cut: bool,
    budget: usize,
    /// The names and nodes read through the table, to look at the clock now and then (see [`READS_CLOCK_EVERY`]).
    reads: usize,
}

impl<'t> Constants<'t> {
    /// The table of the tree under `root`. Nodes deeper than [`super::MAX_DEPTH`] are not read, as the walk
    /// that asks for the names does not reach them either.
    pub(super) fn of(root: Node<'t>, src: &[u8]) -> Self {
        let mut table = Self {
            scopes: vec![Scope {
                parent: None,
                names: HashMap::new(),
            }],
            opened: HashMap::from([(root.id(), 0)]),
            entries: 0,
            cut: false,
            budget: VALUE_BUDGET,
            reads: 0,
        };
        // (node, scope, nearest function scope, depth)
        let mut stack = vec![(root, 0, 0, 0)];
        let mut ticks = 0usize;

        while let Some((node, scope, function, depth)) = stack.pop() {
            ticks += 1;

            if ticks.is_multiple_of(TABLE_CLOCK_EVERY) && out_of_time() {
                table.cut = true;

                break;
            }

            if depth > super::MAX_DEPTH {
                continue;
            }

            let (inner, inner_function) = table.visit(node, src, scope, function);
            let mut cursor = node.walk();

            for child in node.named_children(&mut cursor) {
                stack.push((child, inner, inner_function, depth + 1));
            }
        }

        table
    }

    /// Notes what `node` declares and returns the scope its children are in (and the nearest function's).
    fn visit(&mut self, node: Node<'t>, src: &[u8], scope: usize, function: usize) -> (usize, usize) {
        match node.kind() {
            "lexical_declaration" | "variable_declaration" => self.declaration(node, src, scope, function),
            "import_statement" => self.bind_identifiers(node, src, scope),
            "function_declaration" | "generator_function_declaration" => {
                self.bind_name(node, src, scope);

                let inner = self.open(node, scope);

                self.bind_parameters(node, src, inner);

                return (inner, inner);
            }
            "function_expression" | "generator_function" | "function" | "arrow_function" | "method_definition" => {
                let inner = self.open(node, scope);

                // the name of a method is a member of its class, not a name inside its body: `foo() { const foo = 'x' }`
                // must keep the constant
                if node.kind() != "method_definition" {
                    self.bind_name(node, src, inner);
                }

                self.bind_parameters(node, src, inner);

                return (inner, inner);
            }
            "class_declaration" | "abstract_class_declaration" | "enum_declaration" | "internal_module" => {
                self.bind_name(node, src, scope);
            }
            "statement_block" | "class_static_block" | "switch_body" | "for_statement" => {
                return (self.open(node, scope), function);
            }
            "for_in_statement" => {
                let inner = self.open(node, scope);
                let target = if node
                    .child_by_field_name("kind")
                    .is_some_and(|kind| text(kind, src) == "var")
                {
                    function
                } else {
                    inner
                };

                if let Some(left) = node.child_by_field_name("left") {
                    self.bind_pattern(left, src, target);
                }

                return (inner, function);
            }
            "catch_clause" => {
                let inner = self.open(node, scope);

                if let Some(parameter) = node.child_by_field_name("parameter") {
                    self.bind_pattern(parameter, src, inner);
                }

                return (inner, function);
            }
            _ => {}
        }

        (scope, function)
    }

    /// A new scope inside `parent`, opened by `node`.
    fn open(&mut self, node: Node<'t>, parent: usize) -> usize {
        self.scopes.push(Scope {
            parent: Some(parent),
            names: HashMap::new(),
        });
        self.opened.insert(node.id(), self.scopes.len() - 1);

        self.scopes.len() - 1
    }

    /// `const a = v`, `let a`, `var a`, `const { a } = v`. A `var` is a name of its function; the rest of
    /// its block.
    fn declaration(&mut self, node: Node<'t>, src: &[u8], scope: usize, function: usize) {
        let is_const = node.kind() == "lexical_declaration" && node.child(0).is_some_and(|word| word.kind() == "const");
        let target = if node.kind() == "variable_declaration" {
            function
        } else {
            scope
        };
        let mut cursor = node.walk();

        for declarator in node
            .named_children(&mut cursor)
            .filter(|declarator| declarator.kind() == "variable_declarator")
        {
            let Some(name) = declarator.child_by_field_name("name") else {
                continue;
            };

            if name.kind() != "identifier" {
                self.bind_pattern(name, src, target);

                continue;
            }

            let binding = match declarator.child_by_field_name("value") {
                Some(value) if is_const => Binding::Const { value, scope: target },
                _ => Binding::Other,
            };

            self.bind(target, text(name, src), binding);
        }
    }

    /// Declares `name` in `scope`. A name declared twice in one scope is told by nothing, so it is no constant.
    fn bind(&mut self, scope: usize, name: &str, binding: Binding<'t>) {
        if self.entries >= MAX_SCOPE_ENTRIES {
            self.cut = true;

            return;
        }

        self.entries += 1;

        let Some(names) = self.scopes.get_mut(scope).map(|scope| &mut scope.names) else {
            return;
        };

        match names.get_mut(name) {
            Some(known) => *known = Binding::Other,
            None => {
                names.insert(name.to_string(), binding);
            }
        }
    }

    /// The `name` field of a declaration, as a name that is no constant.
    fn bind_name(&mut self, node: Node<'t>, src: &[u8], scope: usize) {
        if let Some(name) = node.child_by_field_name("name") {
            self.bind(scope, text(name, src), Binding::Other);
        }
    }

    /// The parameters of a function (`parameters`, or the lone `parameter` of an arrow function).
    fn bind_parameters(&mut self, node: Node<'t>, src: &[u8], scope: usize) {
        for field in ["parameters", "parameter"] {
            if let Some(parameters) = node.child_by_field_name(field) {
                self.bind_pattern(parameters, src, scope);
            }
        }
    }

    /// Every name under an import (`import a, { b as c } from`, `import * as d from`): the local name and, as
    /// it cannot be told from here, the imported one.
    fn bind_identifiers(&mut self, node: Node<'t>, src: &[u8], scope: usize) {
        let mut stack = vec![node];

        while let Some(next) = stack.pop() {
            if next.kind() == "identifier" {
                self.bind(scope, text(next, src), Binding::Other);
            }

            // the source string holds no name
            if next.kind() != "string" {
                let mut cursor = next.walk();

                stack.extend(next.named_children(&mut cursor));
            }
        }
    }

    /// The names a pattern declares (`a`, `{ a, b: c = 1, ...d }`, `[a, , b]`, a parameter list): not the keys
    /// before a colon, not a default value, not a type, so every node is read once and a default that holds a
    /// function is no business of the pattern.
    fn bind_pattern(&mut self, pattern: Node<'t>, src: &[u8], scope: usize) {
        let mut stack = vec![pattern];

        while let Some(next) = stack.pop() {
            let mut cursor = next.walk();

            match next.kind() {
                "identifier" | "shorthand_property_identifier_pattern" => {
                    self.bind(scope, text(next, src), Binding::Other);
                }
                "required_parameter" | "optional_parameter" => stack.extend(next.child_by_field_name("pattern")),
                "pair_pattern" => stack.extend(next.child_by_field_name("value")),
                "assignment_pattern" | "object_assignment_pattern" => stack.extend(next.child_by_field_name("left")),
                "formal_parameters" | "object_pattern" | "array_pattern" | "rest_pattern" => {
                    stack.extend(next.named_children(&mut cursor));
                }
                _ => {}
            }
        }
    }

    /// The scope a use sits in: the nearest scope among the nodes from the root down to it.
    fn scope_of(&self, ancestors: &[Node<'t>]) -> Option<usize> {
        ancestors
            .iter()
            .rev()
            .find_map(|node| self.opened.get(&node.id()).copied())
    }

    /// The constant `name` stands for as seen from `scope` (its value, and the scope it was declared in), when
    /// the nearest declaration of the name is a constant.
    fn find(&self, mut scope: usize, name: &str) -> Option<(Node<'t>, usize)> {
        loop {
            let here = self.scopes.get(scope)?;

            match here.names.get(name) {
                Some(Binding::Const { value, scope }) => return Some((*value, *scope)),
                Some(Binding::Other) => return None,
                None => scope = here.parent?,
            }
        }
    }
}

/// What the reader of one class position may ask of the table: where the use is (found once, when first needed,
/// from the nodes above it) and how far it has followed constants.
pub(super) struct Vars<'v, 't> {
    table: &'v mut Constants<'t>,
    ancestors: &'v [Node<'t>],
    here: Option<usize>,
    links: usize,
    /// A constant was not read because a limit was reached: the file lost a class use it may have had.
    pub cut: bool,
    /// The time ran out while this position was read (see [`Vars::tick`]).
    stopped: bool,
}

impl<'v, 't> Vars<'v, 't> {
    /// The names as seen from a use below `ancestors` (the nodes from the root down to its parent).
    pub(super) fn new(table: &'v mut Constants<'t>, ancestors: &'v [Node<'t>]) -> Self {
        Self {
            table,
            ancestors,
            here: None,
            links: 0,
            cut: false,
            stopped: false,
        }
    }

    /// The value of the constant `name` stands for here and the scope that value is read in, paid for out of the
    /// file's budget: every lookup costs [`MIN_READ_COST`], one that finds nothing too, so a file of names that
    /// stand for nothing cannot be asked about for free. Only a name that really means a secret is not followed
    /// (`password`, `apiKey`; not `authCard` or `keyClass`).
    fn follow(&mut self, name: &str) -> Option<(Node<'t>, usize)> {
        if means_secret(name) {
            return None;
        }

        if self.table.cut || self.links >= MAX_CHAIN || self.table.budget < MIN_READ_COST {
            self.cut = true;

            return None;
        }

        self.table.budget -= MIN_READ_COST;

        let here = match self.here {
            Some(here) => here,
            None => {
                let here = self.table.scope_of(self.ancestors)?;

                self.here = Some(here);

                here
            }
        };
        let (value, scope) = self.table.find(here, name)?;
        // the lookup is paid for already
        let cost = value.byte_range().len().saturating_sub(MIN_READ_COST);

        if self.table.budget < cost {
            self.cut = true;

            return None;
        }

        self.table.budget -= cost;

        Some((value, scope))
    }

    /// Counts one read, and looks at the clock every [`READS_CLOCK_EVERY`]. True once the time has run out: the
    /// use is marked as cut, the table is given up, and nothing more is read of this position.
    fn tick(&mut self) -> bool {
        self.table.reads += 1;

        if !self.stopped && self.table.reads.is_multiple_of(READS_CLOCK_EVERY) && out_of_time() {
            self.stopped = true;
            self.table.cut = true;
            self.cut = true;
        }

        self.stopped
    }

    /// Reads on from `scope`, one link further; the scope to go back to.
    fn enter(&mut self, scope: usize) -> Option<usize> {
        self.links += 1;

        self.here.replace(scope)
    }

    fn leave(&mut self, saved: Option<usize>) {
        self.links = self.links.saturating_sub(1);
        self.here = saved;
    }
}

struct Reader<'a, 'e, 'v, 't> {
    src: &'a [u8],
    sheets: &'a HashSet<String>,
    binders: &'a Binders,
    /// The module the words being read belong to: the one the helper call around them was bound to.
    qual: Option<&'a str>,
    /// Reads a CSS selector (`.a > .b`) instead of a list of class names.
    selector: bool,
    /// The same-file constants a name may stand for, when the caller has them.
    vars: Option<&'e mut Vars<'v, 't>>,
    /// The row of the use while the value of a constant is read: its words are written at the use, whatever row
    /// the constant is declared on.
    pin: Option<usize>,
    emit: &'e mut dyn FnMut(&str, Option<&str>, usize),
}

impl<'t> Reader<'_, '_, '_, 't> {
    fn read(&mut self, node: Node<'t>, depth: usize) {
        if depth > MAX_CLASS_DEPTH {
            return;
        }

        if self.vars.as_deref_mut().is_some_and(Vars::tick) {
            return;
        }

        let next = depth + 1;

        if self.selector {
            self.selector_node(node, next);

            return;
        }

        match node.kind() {
            "string" => self.string(node),
            "template_string" => self.template(node),
            // the condition decides which class, it is no class
            "ternary_expression" => {
                for field in ["consequence", "alternative"] {
                    if let Some(branch) = node.child_by_field_name(field) {
                        self.read(branch, next);
                    }
                }
            }
            "binary_expression" => {
                let operator = node.child_by_field_name("operator").map(|op| text(op, self.src));

                // `cond && 'a'` names a class on its right only; `a || 'b'` and `a ?? 'b'` may name both
                let sides: &[&str] = match operator {
                    Some("&&") => &["right"],
                    Some("||" | "??") => &["left", "right"],
                    _ => &[],
                };

                for side in sides {
                    if let Some(operand) = node.child_by_field_name(side) {
                        self.read(operand, next);
                    }
                }
            }
            "array"
            | "parenthesized_expression"
            | "jsx_expression"
            | "spread_element"
            | "as_expression"
            | "satisfies_expression"
            | "non_null_expression" => {
                let mut cursor = node.walk();

                // a cast has its type last: only the value is read
                let limit = if matches!(node.kind(), "as_expression" | "satisfies_expression") {
                    1
                } else {
                    usize::MAX
                };

                for child in node.named_children(&mut cursor).take(limit) {
                    self.read(child, next);
                }
            }
            "object" => self.object(node),
            "call_expression" => self.call(node, next),
            "member_expression" | "subscript_expression" => self.member(node),
            "identifier" => self.identifier(node, next),
            _ => {}
        }
    }

    /// A name: read as the value of the same-file constant it stands for, if it stands for one. The depth is the
    /// one of the name and is only passed on, so a chain of constants counts against [`MAX_CLASS_DEPTH`] as well
    /// as against its own limit.
    fn identifier(&mut self, node: Node<'t>, depth: usize) {
        let name = text(node, self.src);
        let Some(vars) = self.vars.as_deref_mut() else {
            return;
        };
        let Some((value, scope)) = vars.follow(name) else {
            return;
        };
        let saved = vars.enter(scope);
        let outer = self.pin;

        // a chain keeps the row of the first name: the use
        self.pin.get_or_insert(node.start_position().row);
        self.read(value, depth);
        self.pin = outer;

        if let Some(vars) = self.vars.as_deref_mut() {
            vars.leave(saved);
        }
    }

    /// Hands a word to the receiver, on the row of the use when a constant is being read.
    fn put(&mut self, word: &str, qual: Option<&str>, row: usize) {
        (self.emit)(word, qual, self.pin.unwrap_or(row));
    }

    /// One node of a selector: the strings, and the few wrappers a string is put in.
    fn selector_node(&mut self, node: Node<'t>, depth: usize) {
        match node.kind() {
            "string" => {
                let whole = text(node, self.src);
                let inner = whole.get(1..whole.len().saturating_sub(1)).unwrap_or("");

                self.selector_text(inner, node.start_position().row);
            }
            "template_string" => {
                let mut joined = String::new();
                let mut cursor = node.walk();

                for part in node.children(&mut cursor) {
                    match part.kind() {
                        "string_fragment" | "escape_sequence" => joined.push_str(text(part, self.src)),
                        // a selector that is partly unknown has no classes that are surely written
                        "template_substitution" => return,
                        _ => {}
                    }
                }

                self.selector_text(&joined, node.start_position().row);
            }
            "ternary_expression" => {
                for field in ["consequence", "alternative"] {
                    if let Some(branch) = node.child_by_field_name(field) {
                        self.read(branch, depth);
                    }
                }
            }
            "parenthesized_expression" | "as_expression" | "satisfies_expression" | "non_null_expression" => {
                if let Some(value) = node.named_child(0) {
                    self.read(value, depth);
                }
            }
            "identifier" => self.identifier(node, depth),
            _ => {}
        }
    }

    /// The tokens of the selector `selector` that starts on `row`; a token is on the row its text is on.
    fn selector_text(&mut self, selector: &str, row: usize) {
        for token in selector_tokens(selector) {
            let before = (token.as_ptr() as usize).saturating_sub(selector.as_ptr() as usize);
            let lines = selector
                .as_bytes()
                .get(..before)
                .map_or(0, |head| head.iter().filter(|byte| **byte == b'\n').count());

            self.put(token, None, row + lines);
        }
    }

    /// A string literal: each word is a class.
    fn string(&mut self, node: Node) {
        let whole = text(node, self.src);
        let inner = whole.get(1..whole.len().saturating_sub(1)).unwrap_or("");
        let row = node.start_position().row;
        let emit = &mut *self.emit;
        let qual = self.qual;
        let pin = self.pin;

        split_tokens(inner, row, &mut |token, line| emit(token, qual, pin.unwrap_or(line)));
    }

    /// A template literal. A `${}` is replaced by a marker no class holds, so a word that touches one (`btn-${x}`)
    /// is no token while `a ${x} b` still gives `a` and `b`. The marker is repeated around the lines the hole
    /// spans, so the lines of the words after it stay true.
    fn template(&mut self, node: Node) {
        let mut joined = String::new();
        let mut cursor = node.walk();

        for part in node.children(&mut cursor) {
            match part.kind() {
                "string_fragment" | "escape_sequence" => joined.push_str(text(part, self.src)),
                "template_substitution" => {
                    let lines = part.end_position().row - part.start_position().row;

                    joined.push('\0');
                    joined.extend(std::iter::repeat_n('\n', lines));
                    joined.push('\0');
                }
                _ => {}
            }
        }

        let row = node.start_position().row;
        let emit = &mut *self.emit;
        let qual = self.qual;
        let pin = self.pin;

        split_tokens(&joined, row, &mut |token, line| emit(token, qual, pin.unwrap_or(line)));
    }

    /// An object: the keys are classes (`{ active: on, 'is-open': open }`), the values are conditions.
    fn object(&mut self, node: Node) {
        let mut cursor = node.walk();

        for member in node.named_children(&mut cursor) {
            match member.kind() {
                "pair" => {
                    if let Some(key) = member.child_by_field_name("key") {
                        self.key(key);
                    }
                }
                "shorthand_property_identifier" => {
                    let row = member.start_position().row;

                    self.put(text(member, self.src), self.qual, row);
                }
                _ => {}
            }
        }
    }

    /// The key of a pair as classes: a name, or a string of words.
    fn key(&mut self, key: Node) {
        match key.kind() {
            "property_identifier" => {
                let row = key.start_position().row;

                self.put(text(key, self.src), self.qual, row);
            }
            "string" => self.string(key),
            _ => {}
        }
    }

    /// A helper call: every argument is read. `cva` reads its first argument, the classes under
    /// `variants.<name>.<value>` and the `class` of its `compoundVariants`, and nothing else of its options.
    fn call(&mut self, node: Node<'t>, depth: usize) {
        let Some((name, qual)) = helper_of(node, self.src, self.binders) else {
            return;
        };
        let Some(arguments) = node.child_by_field_name("arguments") else {
            return;
        };
        let mut cursor = arguments.walk();
        // the words of a helper nested in another are the nested one's: `cx('a', clsx('b'))` is a module's class
        // and a global one
        let outer = std::mem::replace(&mut self.qual, qual);

        for (index, argument) in arguments.named_children(&mut cursor).enumerate() {
            match (name, index) {
                ("cva", 0) => self.read(argument, depth),
                ("cva", 1) => self.variants(argument, depth),
                ("cva", _) => {}
                _ => self.read(argument, depth),
            }
        }

        self.qual = outer;
    }

    /// The options object of `cva`. `depth` is the one of the call and is only ever passed on, so a `cva` inside
    /// a variant counts against [`MAX_CLASS_DEPTH`] like any other nesting.
    fn variants(&mut self, options: Node<'t>, depth: usize) {
        for (key, value) in self.pairs(options) {
            match key.as_str() {
                "variants" => {
                    // variants.<name>.<value>: the names are not classes, the values are
                    for (_, group) in self.pairs(value) {
                        for (_, classes) in self.pairs(group) {
                            self.read(classes, depth);
                        }
                    }
                }
                "compoundVariants" => {
                    let mut cursor = value.walk();

                    for compound in value.named_children(&mut cursor) {
                        for (key, classes) in self.pairs(compound) {
                            if matches!(key.as_str(), "class" | "className") {
                                self.read(classes, depth);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// The `(key, value)` pairs of an object literal; the key as written, without quotes.
    fn pairs(&self, object: Node<'t>) -> Vec<(String, Node<'t>)> {
        if object.kind() != "object" {
            return Vec::new();
        }

        let mut cursor = object.walk();

        object
            .named_children(&mut cursor)
            .filter(|member| member.kind() == "pair")
            .filter_map(|member| {
                let key = member.child_by_field_name("key")?;
                let value = member.child_by_field_name("value")?;
                let name = text(key, self.src).trim_matches(['\'', '"']);

                Some((name.to_string(), value))
            })
            .collect()
    }

    /// `styles.card` and `styles['card']` of an imported stylesheet: the member is the class, the module the
    /// qualifier.
    fn member(&mut self, node: Node) {
        let Some(object) = node.child_by_field_name("object").filter(|o| o.kind() == "identifier") else {
            return;
        };
        let module = text(object, self.src);

        if !self.sheets.contains(module) {
            return;
        }

        if node.kind() == "member_expression" {
            let Some(property) = node.child_by_field_name("property") else {
                return;
            };
            let name = text(property, self.src);

            if is_token(name) {
                self.put(name, Some(module), property.start_position().row);
            }

            return;
        }

        let Some(index) = node.child_by_field_name("index").filter(|i| i.kind() == "string") else {
            return;
        };
        let whole = text(index, self.src);
        let name = whole.get(1..whole.len().saturating_sub(1)).unwrap_or("");

        if is_token(name) {
            self.put(name, Some(module), index.start_position().row);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_class_names_are_tokens() {
        for good in ["card", "btn-primary", "_x", "-x", "a_b-c9", &"a".repeat(64)] {
            assert!(is_token(good), "{good}");
        }

        for bad in [
            "",
            "9a",
            "md:flex",
            "w-[10px]",
            "!x",
            "w-1/2",
            "a.b",
            "a\0",
            "é",
            &"a".repeat(65),
        ] {
            assert!(!is_token(bad), "{bad}");
        }
    }

    #[test]
    fn a_stylesheet_source_is_told_by_its_extension_without_a_query() {
        assert_eq!(sheet_source("./a.module.css?inline"), Some("./a.module.css"));
        assert_eq!(sheet_source("./a.SCSS"), Some("./a.SCSS"));
        assert_eq!(sheet_source("./a.less#x"), Some("./a.less"));
        assert_eq!(sheet_source("./a"), None);
        assert_eq!(sheet_source("./a.ts"), None);
        assert_eq!(sheet_source("https://cdn.example/a.css"), None);
    }

    /// A `cva` inside a `cva` variant, five thousand levels deep, ends at the depth cap: the stack of a
    /// small thread holds. The levels are on lines of their own, as a minified file is not read. The depth is
    /// what a test can afford to parse; without the cap a thread of this stack overflows at it.
    #[test]
    fn nested_cva_variants_stop_at_the_depth_cap() {
        let source = format!(
            "const x =\n{}'z'{}",
            "cva('',{variants:{a:{b:\n".repeat(5_000),
            "}}})\n".repeat(5_000)
        );

        let out = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || super::super::extract(&mut tree_sitter::Parser::new(), "a.ts", &source))
            .expect("thread")
            .join()
            .expect("no stack overflow");

        assert!(out.refs.iter().filter(|r| r.kind == "cls").count() <= MAX_CLASS_DEPTH + 1);
    }
}
