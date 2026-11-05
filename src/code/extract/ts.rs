//! TypeScript, JavaScript, JSX and TSX: definitions, calls, `new`, JSX components, imports and string names.
//!
//! Files are read with the TSX grammar (so JSX works in `.js` too) except `.ts`, which uses the plain
//! TypeScript grammar. [`extract_block`] parses a script block of a single-file component.

use tree_sitter::{Language, Node, Parser};

use super::classes::{
    Binders, Constants, Vars, binders_of, class_expression_with, helper_of, selector_expression, sheet_locals,
    sheet_source,
};
use super::{
    Extract, Import, MAX_DEPTH, Placement, constant_like, ident_like, is_name_of, out_of_time, push_base, push_bind,
    push_def, push_mcall, push_ref, push_ref_recv, push_use_under, push_word_under, text, written_type,
};

/// Reads a TS/JS file.
pub(super) fn extract(parser: &mut Parser, path: &str, src: &str, out: &mut Extract) {
    let plain = path.to_ascii_lowercase().ends_with(".ts");
    let language: Language = if plain {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
    } else {
        tree_sitter_typescript::LANGUAGE_TSX.into()
    };

    extract_block(parser, &language, src, 0, None, !plain, out);
    keep_function_values(out);
}

/// Drops the function-value references that cannot resolve: a name passed as an argument only means a
/// function when this file defines it or imports it. The same holds for the value of a function kept in a
/// property (`prop`): `fn`, or `ns.fn` where `ns` or `fn` is known.
pub(super) fn keep_function_values(out: &mut Extract) {
    let known: std::collections::HashSet<&str> = out
        .syms
        .iter()
        .filter(|sym| matches!(sym.kind, "fn" | "class" | "component"))
        .map(|sym| sym.name.as_str())
        .chain(out.imports.iter().map(|import| import.local.as_str()))
        .collect();
    let keep: Vec<bool> = out
        .refs
        .iter()
        .map(|r| match r.kind {
            "fnref" => known.contains(r.name.as_str()),
            "prop" => r.qual.as_deref().is_some_and(|value| {
                let first = value.split('.').next().unwrap_or(value);
                let last = value.rsplit('.').next().unwrap_or(value);

                known.contains(last) || known.contains(first)
            }),
            _ => true,
        })
        .collect();
    let mut flags = keep.into_iter();

    out.refs.retain(|_| flags.next().unwrap_or(true));
}

/// Parses `src` as TypeScript (a `.ts` file, or a script block that starts `line_off` lines into its
/// file); references made outside any definition belong to `owner`. `jsx` turns on component references.
pub(super) fn extract_block(
    parser: &mut Parser,
    language: &Language,
    src: &str,
    line_off: usize,
    owner: Option<usize>,
    jsx: bool,
    out: &mut Extract,
) {
    let Some(tree) = super::parse(parser, language, src) else {
        // nothing of the block was read, its classes included
        out.dropped += 1;
        out.classes_cut = true;

        return;
    };

    let imported = Imported::of(tree.root_node(), src.as_bytes());
    let consts = Constants::of(tree.root_node(), src.as_bytes());

    let first = out.refs.len();
    let mut walk = Walk {
        src: src.as_bytes(),
        line_off,
        jsx,
        sfc: owner.is_some(),
        imported,
        consts,
        class_sym: None,
        ticks: 0,
        ancestors: Vec::new(),
        stopped: false,
        spans: Vec::new(),
        out,
    };

    walk.node(tree.root_node(), None, owner, None, 0);

    let spans = std::mem::take(&mut walk.spans);

    mark_shared_lines(out, first, &spans, line_off);
}

/// Marks the references of this block (from `first` on) that sit on a line a JSX node is written on, whatever
/// they are: `const y = f(); const x = <div>text</div>` and `return target(<p>text</p>)` show the text of a page
/// on the line of `f` and `target`, and a line that ends a node holds its text too. `spans` (first and last 0-based
/// row of each outermost JSX node, by first row as the walk met them) are merged in one pass, then each reference
/// finds its span by bisection, so the cost is that of the references.
fn mark_shared_lines(out: &mut Extract, first: usize, spans: &[(usize, usize)], line_off: usize) {
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());

    for &(start, end) in spans {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }

    if merged.is_empty() {
        return;
    }

    for reference in out.refs.iter_mut().skip(first) {
        let Some(row) = reference.line.checked_sub(line_off + 1) else {
            continue;
        };
        let after = merged.partition_point(|&(_, end)| end < row);

        if merged.get(after).is_some_and(|&(start, _)| start <= row) {
            reference.markup = true;
        }
    }
}

/// The TypeScript grammar, for script blocks.
pub(super) fn typescript() -> Language {
    tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
}

/// The TSX grammar, for the expressions of a template.
pub(super) fn tsx() -> Language {
    tree_sitter_typescript::LANGUAGE_TSX.into()
}

fn def_kind(node: Node) -> Option<&'static str> {
    match node.kind() {
        "function_declaration" | "generator_function_declaration" => Some("fn"),
        "class_declaration" | "abstract_class_declaration" => Some("class"),
        "method_definition" | "method_signature" | "abstract_method_signature" => Some("method"),
        "interface_declaration" => Some("interface"),
        "type_alias_declaration" => Some("type"),
        "enum_declaration" => Some("enum"),
        "public_field_definition" => {
            let value = node.child_by_field_name("value").map(|value| value.kind());

            matches!(value, Some("arrow_function" | "function_expression" | "function")).then_some("method")
        }
        "variable_declarator" => {
            let value = node.child_by_field_name("value").map(|value| value.kind());

            if matches!(value, Some("arrow_function" | "function_expression" | "function")) {
                return Some("fn");
            }

            let at_top = node
                .parent()
                .and_then(|declaration| declaration.parent())
                .is_some_and(|outer| {
                    outer.kind() == "program"
                        || (outer.kind() == "export_statement"
                            && outer.parent().is_some_and(|program| program.kind() == "program"))
                });

            at_top.then_some("const")
        }
        _ => None,
    }
}

/// `(name, qualifier, is_member_call)` of the callee of a call.
fn callee(node: Node, src: &[u8]) -> Option<(String, Option<String>, bool)> {
    match node.kind() {
        "identifier" => Some((text(node, src).to_string(), None, false)),
        "member_expression" => {
            let name = text(node.child_by_field_name("property")?, src).to_string();
            let object = node.child_by_field_name("object")?;
            // `Foo.bar()` on an identifier: keep it as a qualifier (namespace import, class static)
            let qual = (object.kind() == "identifier").then(|| text(object, src).to_string());

            Some((name, qual, true))
        }
        _ => None,
    }
}

/// Whether the module path of an import or re-export is short enough to keep: an over-long one is
/// dropped (and counted) instead of being copied into every name the statement brings in.
fn source_fits(node: Node, src: &[u8]) -> bool {
    node.child_by_field_name("source")
        .is_none_or(|source| super::fits(text(source, src).trim_matches(['\'', '"'])))
}

fn collect_import(node: Node, src: &[u8], out: &mut Vec<Import>) {
    let Some(source) = node.child_by_field_name("source") else {
        return;
    };
    let source = text(source, src).trim_matches(['\'', '"']);

    // An over-long module path would be copied into every name it brings in.
    if !super::fits(source) {
        return;
    }

    let source = source.to_string();
    let mut cursor = node.walk();

    // `import './card.css'` brings no name: the sheet is read for its side effect, and its classes are the
    // file's own
    if !node.named_children(&mut cursor).any(|c| c.kind() == "import_clause") {
        if let Some(sheet) = sheet_source(&source) {
            out.push(Import {
                local: format!("sheet:{sheet}"),
                orig: "*".into(),
                source: sheet.to_string(),
            });
        }

        return;
    }

    for clause in node.named_children(&mut cursor).filter(|c| c.kind() == "import_clause") {
        let mut inner = clause.walk();

        for part in clause.named_children(&mut inner) {
            match part.kind() {
                "identifier" => {
                    // default import: the component/module itself
                    let local = text(part, src).to_string();

                    out.push(Import {
                        local: local.clone(),
                        orig: local,
                        source: source.clone(),
                    });
                }
                "named_imports" => {
                    let mut specs = part.walk();

                    for spec in part
                        .named_children(&mut specs)
                        .filter(|s| s.kind() == "import_specifier")
                    {
                        let Some(name) = spec.child_by_field_name("name") else {
                            continue;
                        };
                        let orig = text(name, src).to_string();
                        let local = spec
                            .child_by_field_name("alias")
                            .map_or_else(|| orig.clone(), |alias| text(alias, src).to_string());

                        out.push(Import {
                            local,
                            orig,
                            source: source.clone(),
                        });
                    }
                }
                "namespace_import" => {
                    let mut names = part.walk();
                    let id = part.named_children(&mut names).find(|n| n.kind() == "identifier");

                    if let Some(id) = id {
                        out.push(Import {
                            local: text(id, src).to_string(),
                            orig: "*".into(),
                            source: source.clone(),
                        });
                    }
                }
                _ => {}
            }
        }
    }
}

/// `export * from './x'` and `export { a, b as c } from './x'`: kept as imports whose local name is
/// `export:<exported name>` (`export:*` for all), so the graph can follow a name through a re-export.
fn collect_reexport(node: Node, src: &[u8], out: &mut Vec<Import>) {
    let Some(source) = node.child_by_field_name("source") else {
        return;
    };
    let source = text(source, src).trim_matches(['\'', '"']);

    // An over-long module path would be copied into every name it brings in.
    if !super::fits(source) {
        return;
    }

    let source = source.to_string();
    let mut cursor = node.walk();

    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "export_clause" => {
                let mut specs = child.walk();

                for spec in child
                    .named_children(&mut specs)
                    .filter(|s| s.kind() == "export_specifier")
                {
                    let Some(name) = spec.child_by_field_name("name") else {
                        continue;
                    };
                    let orig = text(name, src).to_string();
                    let exported = spec
                        .child_by_field_name("alias")
                        .map_or_else(|| orig.clone(), |alias| text(alias, src).to_string());

                    out.push(Import {
                        local: format!("export:{exported}"),
                        orig,
                        source: source.clone(),
                    });
                }
            }
            "namespace_export" => {}
            _ => {}
        }
    }

    // `export * from`: no clause, a `*` token
    let mut tokens = node.walk();

    if !node.children(&mut tokens).any(|c| c.kind() == "export_clause") {
        out.push(Import {
            local: "export:*".to_string(),
            orig: "*".to_string(),
            source,
        });
    }
}

/// `const Page = lazy(() => import('./Page').then((m) => ({ default: m.Page })))`: the local name is the
/// component of that module, so it is an import, not a definition. `None` for anything else.
fn lazy_import(declarator: Node, src: &[u8]) -> Option<Import> {
    let local = text(
        declarator
            .child_by_field_name("name")
            .filter(|n| n.kind() == "identifier")?,
        src,
    );
    let call = declarator
        .child_by_field_name("value")
        .filter(|v| v.kind() == "call_expression")?;
    let function = call.child_by_field_name("function")?;
    let lazy = match function.kind() {
        "identifier" => text(function, src) == "lazy",
        "member_expression" => text(function.child_by_field_name("property")?, src) == "lazy",
        _ => false,
    };

    if !lazy {
        return None;
    }

    let loader = call
        .child_by_field_name("arguments")?
        .named_child(0)
        .filter(|a| a.kind() == "arrow_function")?
        .child_by_field_name("body")?;
    let mut body = loader;
    let mut orig = local.to_string();

    // `import(..).then((m) => ({ default: m.Name }))`
    if body.kind() == "call_expression"
        && let Some(member) = body
            .child_by_field_name("function")
            .filter(|f| f.kind() == "member_expression")
        && text(member.child_by_field_name("property")?, src) == "then"
    {
        let callback = body.child_by_field_name("arguments")?.named_child(0)?;
        let mut stack = vec![callback];

        while let Some(next) = stack.pop() {
            if next.kind() == "pair"
                && text(next.child_by_field_name("key")?, src) == "default"
                && let Some(value) = next
                    .child_by_field_name("value")
                    .filter(|v| v.kind() == "member_expression")
            {
                orig = text(value.child_by_field_name("property")?, src).to_string();
                break;
            }

            let mut cursor = next.walk();
            stack.extend(next.named_children(&mut cursor));
        }

        body = member.child_by_field_name("object")?;
    }

    let import_call = body.kind() == "call_expression"
        && body
            .child_by_field_name("function")
            .is_some_and(|f| f.kind() == "import");

    if !import_call {
        return None;
    }

    let source = body.child_by_field_name("arguments")?.named_child(0)?;

    if source.kind() != "string" {
        return None;
    }

    Some(Import {
        local: local.to_string(),
        orig,
        source: text(source, src).trim_matches(['\'', '"']).to_string(),
    })
}

/// The local names the top level of a block imports, whatever their case.
struct Imported {
    /// Default and named imports (`import x`, `import { a, b as c }`): the values and functions of other files.
    values: std::collections::HashSet<String>,
    /// Namespace imports (`import * as ns`): `ns.member` names an export of the module.
    namespaces: std::collections::HashSet<String>,
    /// The names that stand for a stylesheet module (`import styles from './a.module.css'`): `styles.card` is a
    /// class.
    sheets: std::collections::HashSet<String>,
    /// The file's own class helpers made with `classNames.bind(styles)`, and the module each is bound to.
    binders: Binders,
}

impl Imported {
    fn of(root: Node, src: &[u8]) -> Self {
        let mut imports = Vec::new();
        let mut cursor = root.walk();

        for statement in root
            .named_children(&mut cursor)
            .filter(|n| n.kind() == "import_statement")
        {
            collect_import(statement, src, &mut imports);
        }

        let sheets = sheet_locals(&imports);
        let binders = binders_of(root, src, &sheets, &imports);
        let (namespaces, values): (Vec<Import>, Vec<Import>) =
            imports.into_iter().partition(|import| import.orig == "*");

        Self {
            sheets,
            binders,
            values: values.into_iter().map(|import| import.local).collect(),
            namespaces: namespaces.into_iter().map(|import| import.local).collect(),
        }
    }
}

struct Walk<'a> {
    src: &'a [u8],
    line_off: usize,
    jsx: bool,
    /// Inside a single-file component: a top-level `const x = f()` runs for the component, it is no definition.
    sfc: bool,
    /// What this file (or script block) imports: such a name is a value of another file in any case.
    imported: Imported,
    /// The `const` declarations of the file, for a class position that names one.
    consts: Constants<'a>,
    /// The class or interface whose body is being read.
    class_sym: Option<usize>,
    /// Nodes visited, so the clock is read once in [`CLOCK_EVERY`] of them.
    ticks: usize,
    /// The nodes from the root down to the one whose children are being read: the parent of a parent is one step
    /// away, where asking the tree costs the width of the parent's siblings.
    ancestors: Vec<Node<'a>>,
    /// The file's time ran out: the rest of the tree is not read.
    stopped: bool,
    /// The first and last row of each outermost JSX node, in the order the walk meets them (so by first row).
    spans: Vec<(usize, usize)>,
    out: &'a mut Extract,
}

/// True for the name of an attribute or a key that holds class names: `class`, `className`, and a name that
/// ends in `ClassName` (`wrapperClassName`). Not `classes`, `sx` or `style`.
fn class_name(name: &str) -> bool {
    matches!(name, "class" | "className") || (name.len() > "ClassName".len() && name.ends_with("ClassName"))
}

/// Nodes between two looks at the clock. A visit is cheap in a tree of ordinary shape and slow in one of a
/// hostile shape (a millisecond or more), so the clock is read often: a read costs a few visits at most.
const CLOCK_EVERY: usize = 8;

/// Deeper receiver expressions are not followed.
const MAX_EXPR_DEPTH: usize = 12;

/// The receiver expression a node stands for (see `graph/infer.rs`), when it can be told from the syntax.
fn expr(node: Node, src: &[u8], depth: usize) -> Option<String> {
    if depth > MAX_EXPR_DEPTH {
        return None;
    }

    match node.kind() {
        "identifier" => Some(text(node, src).to_string()),
        "this" => Some("this".to_string()),
        "member_expression" => {
            let property = node.child_by_field_name("property")?;

            if property.kind() != "property_identifier" {
                return None;
            }

            let object = expr(node.child_by_field_name("object")?, src, depth + 1)?;

            Some(format!("{object}.{}", text(property, src)))
        }
        "call_expression" => {
            let function = node.child_by_field_name("function")?;

            match function.kind() {
                "identifier" => Some(format!("{}()", text(function, src))),
                "member_expression" => Some(format!("{}()", expr(function, src, depth + 1)?)),
                _ => None,
            }
        }
        "await_expression" | "parenthesized_expression" | "non_null_expression" => {
            expr(node.named_child(0)?, src, depth + 1)
        }
        "new_expression" => {
            let ty = written_type(text(node.child_by_field_name("constructor")?, src))?;

            Some(format!(":{ty}"))
        }
        "as_expression" | "satisfies_expression" => {
            let ty = written_type(text(node.named_child(1)?, src))?;

            Some(format!(":{ty}"))
        }
        "string" | "template_string" => Some(":string".to_string()),
        "number" => Some(":number".to_string()),
        "true" | "false" => Some(":boolean".to_string()),
        "array" => Some(":Array".to_string()),
        "regex" => Some(":RegExp".to_string()),
        _ => None,
    }
}

/// What a variable or field initialised with `value` is: the class of a `new`, or the expression to follow.
fn initialiser(value: Node, src: &[u8]) -> Option<String> {
    let inner = match value.kind() {
        "await_expression" | "parenthesized_expression" => value.named_child(0)?,
        _ => value,
    };

    if inner.kind() == "new_expression" {
        return written_type(text(inner.child_by_field_name("constructor")?, src));
    }

    expr(inner, src, 0).map(|e| format!("={e}"))
}

/// The written type of the annotation field `type` of a declaration.
fn annotated(node: Node, src: &[u8]) -> Option<String> {
    written_type(text(node.child_by_field_name("type")?, src))
}

/// The written return type of a function-like node.
fn returns(node: Node, src: &[u8]) -> Option<String> {
    written_type(text(node.child_by_field_name("return_type")?, src))
}

/// True when an identifier stands for a value there: not a declared name, a callee, a parameter, a tag or
/// the object of a member access.
fn value_position(node: Node, parent: Node) -> bool {
    if is_name_of(node, Some(parent)) {
        return false;
    }

    match parent.kind() {
        "call_expression" => !parent
            .child_by_field_name("function")
            .is_some_and(|function| function.id() == node.id()),
        "new_expression"
        | "member_expression"
        | "required_parameter"
        | "optional_parameter"
        | "import_specifier"
        | "export_specifier"
        | "namespace_import"
        | "import_clause"
        | "jsx_opening_element"
        | "jsx_closing_element"
        | "jsx_self_closing_element"
        | "labeled_statement"
        | "rest_pattern"
        | "array_pattern"
        | "object_pattern"
        | "shorthand_property_identifier_pattern"
        | "type_query" => false,
        _ => true,
    }
}

/// True when an imported name stands for its value there: where [`value_position`] says so, or as the
/// object of a member access (`siteContent.en`).
fn imported_position(node: Node, parent: Node) -> bool {
    if parent.kind() == "member_expression" {
        return parent
            .child_by_field_name("object")
            .is_some_and(|object| object.id() == node.id());
    }

    value_position(node, parent)
}

/// The type an object literal is declared to have: `const x: T = {..}`, `{..} as T`, or the return type of
/// the function it is returned from.
fn object_type(object: Node, src: &[u8]) -> Option<String> {
    let mut at = object.parent()?;

    while at.kind() == "parenthesized_expression" {
        at = at.parent()?;
    }

    match at.kind() {
        "as_expression" | "satisfies_expression" => written_type(text(at.named_child(1)?, src)),
        "variable_declarator" => annotated(at, src),
        "return_statement" => {
            let mut up = at.parent();

            while let Some(candidate) = up {
                if matches!(
                    candidate.kind(),
                    "function_declaration"
                        | "generator_function_declaration"
                        | "method_definition"
                        | "arrow_function"
                        | "function_expression"
                        | "function"
                ) {
                    return returns(candidate, src);
                }

                up = candidate.parent();
            }

            None
        }
        "arrow_function" => returns(at, src),
        _ => None,
    }
}

/// Property values kept per object literal property (`key: a ?? b ? c : d` is three).
const MAX_PROP_VALUES: usize = 4;

/// The functions a property value may hold, as written: `fn`, `ns.fn`, `a ?? fn`, `cond ? a : b`,
/// `fn.bind(x)`, any of them in parentheses or after a cast. A value that starts at `this` is a method, which
/// a function value never is.
fn function_values(value: Node, src: &[u8], depth: usize, found: &mut Vec<String>) {
    if depth > MAX_EXPR_DEPTH || found.len() >= MAX_PROP_VALUES {
        return;
    }

    match value.kind() {
        "identifier" => found.push(text(value, src).to_string()),
        "member_expression" => {
            if let Some(chain) =
                expr(value, src, 0).filter(|chain| !chain.starts_with("this") && !chain.contains(['(', ':']))
            {
                found.push(chain);
            }
        }
        "parenthesized_expression" | "as_expression" | "satisfies_expression" | "non_null_expression" => {
            if let Some(inner) = value.named_child(0) {
                function_values(inner, src, depth + 1, found);
            }
        }
        "binary_expression" => {
            let operator = value
                .child_by_field_name("operator")
                .map(|operator| text(operator, src));

            if matches!(operator, Some("??" | "||"))
                && let (Some(left), Some(right)) =
                    (value.child_by_field_name("left"), value.child_by_field_name("right"))
            {
                function_values(left, src, depth + 1, found);
                function_values(right, src, depth + 1, found);
            }
        }
        "ternary_expression" => {
            for field in ["consequence", "alternative"] {
                if let Some(branch) = value.child_by_field_name(field) {
                    function_values(branch, src, depth + 1, found);
                }
            }
        }
        "call_expression" => {
            // `fn.bind(this)`
            if let Some(member) = value
                .child_by_field_name("function")
                .filter(|function| function.kind() == "member_expression")
                && member
                    .child_by_field_name("property")
                    .is_some_and(|property| text(property, src) == "bind")
                && let Some(object) = member.child_by_field_name("object")
            {
                function_values(object, src, depth + 1, found);
            }
        }
        _ => {}
    }
}

/// The function an object literal is passed to as an argument (`create({ .. })`, `new Runner({ .. })`,
/// `Runner.create({ .. })`): its name as written, or its last name for a member. `parent` is the parent of the
/// object, which the caller has at hand.
fn passed_to(parent: Option<Node>, src: &[u8]) -> Option<String> {
    let mut at = parent?;

    while matches!(
        at.kind(),
        "parenthesized_expression" | "as_expression" | "satisfies_expression" | "non_null_expression"
    ) {
        at = at.parent()?;
    }

    if at.kind() != "arguments" {
        return None;
    }

    let call = at.parent()?;
    let callee = match call.kind() {
        "call_expression" => call.child_by_field_name("function")?,
        "new_expression" => call.child_by_field_name("constructor")?,
        _ => return None,
    };

    match callee.kind() {
        "identifier" => Some(text(callee, src).to_string()),
        "member_expression" => Some(text(callee.child_by_field_name("property")?, src).to_string()),
        _ => None,
    }
}

/// The name of a base type as written: `B`, `B<T>`, `ns.B` and `ns.B<T>` give `B`. Anything else (a call, type
/// arguments, a comment) names none.
fn base_name(node: Node, src: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" | "type_identifier" => Some(text(node, src).to_string()),
        "generic_type" => base_name(node.child_by_field_name("name")?, src),
        "nested_type_identifier" => base_name(node.child_by_field_name("name")?, src),
        "member_expression" => Some(text(node.child_by_field_name("property")?, src).to_string()),
        _ => None,
    }
}

impl<'a> Walk<'a> {
    fn node(
        &mut self,
        node: Node<'a>,
        parent: Option<Node<'a>>,
        current: Option<usize>,
        ctx: Option<&str>,
        depth: usize,
    ) {
        if self.stopped {
            return;
        }

        // a subtree past the depth is not read, and may hold class uses: the file says so
        if depth > MAX_DEPTH {
            self.out.dropped += 1;
            self.out.classes_cut = true;

            return;
        }

        // the parse is bounded by its own clock, but a tree the parser made quickly can still be slow to walk (a
        // flat node of thousands of children asks for the parent of each): the walk gives up with the file's time
        self.ticks += 1;

        if self.ticks.is_multiple_of(CLOCK_EVERY) && out_of_time() {
            self.stopped = true;

            // what is left of the file is not read, its classes included
            self.out.dropped += 1;
            self.out.classes_cut = true;

            return;
        }

        let src = self.src;
        let line = node.start_position().row + self.line_off + 1;
        if node.kind() == "variable_declarator"
            && let Some(import) = lazy_import(node, src)
        {
            self.out.imports.push(import);
            return;
        }

        let enclosing = current;
        let saved_class = self.class_sym;
        let mut current = current;
        let mut owner: Option<String> = None;

        if let Some(kind) = def_kind(node)
            && let Some(name) = node.child_by_field_name("name")
            && matches!(name.kind(), "identifier" | "property_identifier" | "type_identifier")
            && !(kind == "const" && self.sfc)
        {
            let place = Placement {
                src,
                line_off: self.line_off,
                test: false,
            };
            // a method of an object literal belongs to no class around it
            let parent = if node.parent().is_some_and(|p| p.kind() == "object") {
                None
            } else {
                ctx.map(str::to_string)
            };
            let sym = push_def(self.out, node, kind, name, parent.as_deref(), &place);

            current = Some(sym);
            self.set_return(sym, node);

            if matches!(kind, "class" | "interface" | "enum") {
                owner = Some(text(name, src).to_string());
            }

            if matches!(kind, "class" | "interface") {
                self.class_sym = Some(sym);
                self.heritage(node, text(name, src), line);
            }

            if kind == "class" {
                push_bind(self.out, Some(sym), "this", text(name, src), "param");
            }
        }

        if node.kind() == "import_statement" {
            if source_fits(node, src) {
                collect_import(node, src, &mut self.out.imports);
            } else {
                self.out.dropped += 1;
            }
            return;
        }

        if node.kind() == "export_statement" && node.child_by_field_name("source").is_some() {
            if source_fits(node, src) {
                collect_reexport(node, src, &mut self.out.imports);
            } else {
                self.out.dropped += 1;
            }
            return;
        }

        // a reference inside a JSX element sits where a line of source holds the text of the page (a child line has
        // no tag on it, a text node spans lines), so it is marked as written in markup. The flag is saved and put
        // back around the children: one write per JSX node, no walk up the parents. A function body in an
        // attribute (`onClick={() => { doIt(); }}`) is script on lines of its own but is marked as well: it is
        // told apart from the markup around it by nothing a single line shows.
        let was_markup = self.out.in_markup;

        if matches!(node.kind(), "jsx_element" | "jsx_self_closing_element" | "jsx_fragment") {
            // the rows of an outermost JSX node are kept: any other reference on one of them shares its line with
            // markup, whatever it is (see [`mark_shared_lines`])
            if !was_markup {
                self.spans.push((node.start_position().row, node.end_position().row));
            }

            self.out.in_markup = true;
        }

        self.reference(node, parent, current, line);
        self.binding(node, parent, enclosing, current, ctx);

        let ctx_next = owner.as_deref().or(ctx);
        let mut cursor = node.walk();

        self.ancestors.push(node);

        for child in node.children(&mut cursor) {
            self.node(child, Some(node), current, ctx_next, depth + 1);
        }

        self.ancestors.pop();

        self.out.in_markup = was_markup;
        self.class_sym = saved_class;
    }

    /// The types a class or interface extends or implements (`class A extends B implements I, J`,
    /// `interface A extends B, C`).
    fn heritage(&mut self, node: Node, derived: &str, line: usize) {
        let mut cursor = node.walk();
        let clauses: Vec<Node> = node
            .children(&mut cursor)
            .filter(|child| matches!(child.kind(), "class_heritage" | "extends_type_clause"))
            .collect();

        for clause in clauses {
            let mut inner = clause.walk();

            for part in clause.named_children(&mut inner) {
                match part.kind() {
                    "extends_clause" | "implements_clause" => {
                        let mut types = part.walk();

                        for ty in part.named_children(&mut types) {
                            if let Some(base) = base_name(ty, self.src) {
                                push_base(self.out, derived, &base, line);
                            }
                        }
                    }
                    // `extends X` of a JavaScript class, and of an interface
                    _ => {
                        if let Some(base) = base_name(part, self.src) {
                            push_base(self.out, derived, &base, line);
                        }
                    }
                }
            }
        }
    }

    /// Records the return type of the definition `sym` made from `node`.
    fn set_return(&mut self, sym: usize, node: Node) {
        let source = match node.kind() {
            "variable_declarator" => node.child_by_field_name("value"),
            _ => Some(node),
        };
        let ret = source.and_then(|function| returns(function, self.src));

        if let Some(def) = self.out.syms.get_mut(sym) {
            def.ret = ret;
        }
    }

    /// The written types a node declares: variables, parameters, fields, members and enum members.
    fn binding(
        &mut self,
        node: Node,
        parent: Option<Node>,
        enclosing: Option<usize>,
        current: Option<usize>,
        ctx: Option<&str>,
    ) {
        let src = self.src;

        match node.kind() {
            "variable_declarator" => {
                let Some(name) = node.child_by_field_name("name").filter(|n| n.kind() == "identifier") else {
                    return;
                };
                let ty = annotated(node, src).or_else(|| {
                    node.child_by_field_name("value")
                        .and_then(|value| initialiser(value, src))
                });

                if let Some(ty) = ty {
                    push_bind(self.out, enclosing, text(name, src), &ty, "var");
                }
            }
            "for_in_statement" => {
                let Some(name) = node.child_by_field_name("left").filter(|n| n.kind() == "identifier") else {
                    return;
                };

                if let Some(iterated) = node.child_by_field_name("right").and_then(|r| expr(r, src, 0)) {
                    push_bind(self.out, current, text(name, src), &format!("=@{iterated}"), "var");
                }
            }
            "required_parameter" | "optional_parameter" => {
                let Some(name) = node.child_by_field_name("pattern").filter(|n| n.kind() == "identifier") else {
                    return;
                };
                let Some(ty) = annotated(node, src) else {
                    return;
                };

                push_bind(self.out, current, text(name, src), &ty, "param");

                // `constructor(private api: Api)` also declares the field
                let mut cursor = node.walk();
                let declares_field = node
                    .children(&mut cursor)
                    .any(|child| matches!(child.kind(), "accessibility_modifier" | "readonly"));

                if declares_field && let (Some(class), Some(owner)) = (self.class_sym, ctx) {
                    push_bind(
                        self.out,
                        Some(class),
                        &format!("{owner}.{}", text(name, src)),
                        &ty,
                        "field",
                    );
                }
            }
            "public_field_definition" => {
                let (Some(class), Some(owner), Some(name)) = (self.class_sym, ctx, node.child_by_field_name("name"))
                else {
                    return;
                };
                let ty = annotated(node, src).or_else(|| {
                    node.child_by_field_name("value")
                        .and_then(|value| initialiser(value, src))
                });

                if let Some(ty) = ty {
                    push_bind(
                        self.out,
                        Some(class),
                        &format!("{owner}.{}", text(name, src)),
                        &ty,
                        "field",
                    );
                }
            }
            "property_signature" => {
                let (Some(class), Some(owner), Some(name)) = (self.class_sym, ctx, node.child_by_field_name("name"))
                else {
                    return;
                };
                let annotation = node.child_by_field_name("type").and_then(|t| t.named_child(0));

                if let Some(function) = annotation.filter(|t| t.kind() == "function_type") {
                    self.member(node, name, owner, function);
                } else if let Some(ty) = annotated(node, src) {
                    push_bind(
                        self.out,
                        Some(class),
                        &format!("{owner}.{}", text(name, src)),
                        &ty,
                        "field",
                    );
                }
            }
            "pair" => {
                let (Some(key), Some(value)) = (node.child_by_field_name("key"), node.child_by_field_name("value"))
                else {
                    return;
                };

                if key.kind() != "property_identifier" {
                    return;
                }

                if matches!(value.kind(), "arrow_function" | "function_expression" | "function") {
                    if let Some(object) = parent {
                        let owner = object_type(object, src);

                        self.member(node, key, owner.as_deref().unwrap_or(""), value);
                    }
                } else {
                    let mut values = Vec::new();

                    function_values(value, src, 0, &mut values);
                    self.properties(node, parent, text(key, src), &values, current);
                }
            }
            "shorthand_property_identifier" => {
                let name = text(node, src);

                self.properties(node, parent, name, &[name.to_string()], current);
            }
            "enum_assignment" => {
                if let Some(name) = node.child_by_field_name("name") {
                    self.enum_member(node, name, ctx);
                }
            }
            "property_identifier" if parent.is_some_and(|p| p.kind() == "enum_body") => {
                self.enum_member(node, node, ctx);
            }
            _ => {}
        }
    }

    /// Notes that property `key` of the object literal around `node` may hold each of `values` (a `prop`
    /// reference: the value as written in `qual`, the function the object is passed to in `recv`). Whether a
    /// value is a function is told when the graph is loaded.
    fn properties(&mut self, node: Node, parent: Option<Node>, key: &str, values: &[String], from: Option<usize>) {
        // the stack ends with the parent of `node`, the object literal, and the one before it is the object's parent
        let above = self.ancestors.iter().rev().nth(1).copied();
        let via = parent.and_then(|_| passed_to(above, self.src));
        let line = node.start_position().row + self.line_off + 1;

        for value in values {
            push_ref_recv(self.out, from, key, "prop", Some(value.clone()), via.clone(), line);
        }
    }

    /// A member of an interface or an object literal that holds a function: a method named `name` of `owner`.
    /// It never becomes the scope of the references inside it, and a string never names it.
    fn member(&mut self, node: Node, name: Node, owner: &str, function: Node) {
        let place = Placement {
            src: self.src,
            line_off: self.line_off,
            test: false,
        };
        let owner = (!owner.is_empty()).then_some(owner);
        let sym = push_def(self.out, node, "member", name, owner, &place);

        if let Some(def) = self.out.syms.get_mut(sym) {
            def.ret = returns(function, self.src);
        }
    }

    fn enum_member(&mut self, node: Node, name: Node, owner: Option<&str>) {
        let place = Placement {
            src: self.src,
            line_off: self.line_off,
            test: false,
        };

        push_def(self.out, node, "enum_member", name, owner, &place);
    }

    /// The class tokens a node writes, if it is a place where classes are written: the `class`, `className` or
    /// `*ClassName` of an element or an object, a call of a helper that joins class names, a member of a
    /// stylesheet module, `x.className = ...`, the arguments of `x.classList.add(...)` and of the like, and the
    /// selector or the class names `x.querySelector('.a')` and `x.getElementsByClassName('a')` look for. A token
    /// the places overlap on (`className={styles.card}`) is kept once.
    fn class_refs(&mut self, node: Node<'a>, from: Option<usize>) {
        let src = self.src;
        let mut places: Vec<Node<'a>> = Vec::new();
        let mut selector = false;

        match node.kind() {
            "jsx_attribute" => {
                let named = node
                    .named_child(0)
                    .filter(|name| name.kind() == "property_identifier")
                    .is_some_and(|name| class_name(text(name, src)));

                if named {
                    places.extend(node.named_child(1));
                }
            }
            "pair" => {
                let named = node.child_by_field_name("key").is_some_and(|key| {
                    matches!(key.kind(), "property_identifier" | "string")
                        && class_name(text(key, src).trim_matches(['\'', '"']))
                });

                if named {
                    places.extend(node.child_by_field_name("value"));
                }
            }
            "call_expression" if helper_of(node, src, &self.imported.binders).is_some() => places.push(node),
            "call_expression" => selector = self.method_places(node, &mut places),
            "assignment_expression" | "augmented_assignment_expression" => {
                let assigned = node
                    .child_by_field_name("left")
                    .filter(|left| left.kind() == "member_expression")
                    .and_then(|left| left.child_by_field_name("property"))
                    .is_some_and(|property| text(property, src) == "className");

                if assigned {
                    places.extend(node.child_by_field_name("right"));
                }
            }
            "member_expression" | "subscript_expression" if !self.imported.sheets.is_empty() => places.push(node),
            _ => {}
        }

        if places.is_empty() {
            return;
        }

        let line_off = self.line_off;
        let out = &mut *self.out;
        let mut vars = Vars::new(&mut self.consts, &self.ancestors);

        for place in places {
            let emit = &mut |token: &str, qual: Option<&str>, row: usize| {
                out.add_class(from, token, qual, row + line_off + 1);
            };

            if selector {
                selector_expression(place, src, Some(&mut vars), emit);
            } else {
                class_expression_with(
                    place,
                    src,
                    &self.imported.sheets,
                    &self.imported.binders,
                    Some(&mut vars),
                    emit,
                );
            }
        }

        // a constant that was not read for want of budget: the file may have lost a class use
        if vars.cut {
            out.classes_cut = true;
        }
    }

    /// The arguments of a method call that name classes, pushed on `places`: `x.classList.add|remove(...)` (all
    /// of them), `x.classList.toggle|contains(first)`, `x.classList.replace(old, new)`, `x.getElementsByClassName(a)`
    /// and `x.querySelector|querySelectorAll|closest|matches(selector)`. True when the places are selectors. The
    /// call must be a method call (`x.name(...)`): a function of the same name that is no method says nothing.
    fn method_places(&self, call: Node<'a>, places: &mut Vec<Node<'a>>) -> bool {
        let src = self.src;
        let Some(function) = call
            .child_by_field_name("function")
            .filter(|function| function.kind() == "member_expression")
        else {
            return false;
        };
        let Some(method) = function.child_by_field_name("property").map(|name| text(name, src)) else {
            return false;
        };
        let Some(arguments) = call.child_by_field_name("arguments") else {
            return false;
        };
        let (count, selector) = match method {
            "add" | "remove" | "toggle" | "contains" | "replace" => {
                let on_list = function
                    .child_by_field_name("object")
                    .filter(|object| object.kind() == "member_expression")
                    .and_then(|object| object.child_by_field_name("property"))
                    .is_some_and(|property| text(property, src) == "classList");

                if !on_list {
                    return false;
                }

                match method {
                    "add" | "remove" => (usize::MAX, false),
                    "replace" => (2, false),
                    _ => (1, false),
                }
            }
            "getElementsByClassName" => (1, false),
            "querySelector" | "querySelectorAll" | "closest" | "matches" => (1, true),
            _ => return false,
        };
        let mut cursor = arguments.walk();

        places.extend(
            arguments
                .named_children(&mut cursor)
                .filter(|argument| argument.kind() != "comment")
                .take(count),
        );

        selector
    }

    /// The reference a node itself makes, if any.
    fn reference(&mut self, node: Node<'a>, parent: Option<Node>, from: Option<usize>, line: usize) {
        let src = self.src;

        self.class_refs(node, from);

        match node.kind() {
            "type_identifier" if !is_name_of(node, parent) => {
                let qual = parent
                    .filter(|parent| parent.kind() == "nested_type_identifier")
                    .and_then(|parent| parent.child_by_field_name("module"))
                    .map(|module| text(module, src).to_string());

                push_ref(self.out, from, text(node, src), "type", qual, line);
            }
            "call_expression" => {
                let Some(function) = node.child_by_field_name("function") else {
                    return;
                };

                if let Some((name, qual, member)) = callee(function, src) {
                    if member {
                        let recv = function
                            .child_by_field_name("object")
                            .and_then(|object| expr(object, src, 0));

                        push_mcall(self.out, from, &name, qual, recv, line);
                    } else {
                        push_ref(self.out, from, &name, "call", qual, line);
                    }
                }
            }
            // React/JSX: <Foo /> and <Foo.Bar> are component uses (lowercase tags are DOM elements)
            "jsx_opening_element" | "jsx_self_closing_element" if self.jsx => {
                if let Some(name) = node.child_by_field_name("name") {
                    match name.kind() {
                        "identifier" if text(name, src).starts_with(|c: char| c.is_uppercase()) => {
                            push_ref(self.out, from, text(name, src), "component", None, line);
                        }
                        "member_expression" => {
                            if let Some((member, qual, _)) = callee(name, src) {
                                push_ref(self.out, from, &member, "component", qual, line);
                            }
                        }
                        _ => {}
                    }
                }
            }
            // names passed as strings: IPC commands, event names, routes, registries
            // a function passed as a value (`app.get(path, handler)`, `rows.map(format)`), or a constant
            "identifier" => {
                let Some(parent) = parent else {
                    return;
                };
                let name = text(node, src);

                // also an element of an array: a table of steps or handlers (`[["Facts", factsStep]]`)
                if matches!(parent.kind(), "arguments" | "jsx_expression" | "array") {
                    push_ref(self.out, from, name, "fnref", None, line);
                }

                // a name this file imports is a value of another file whatever its case (`siteContent.en`)
                let imported = self.imported.values.contains(name) && imported_position(node, parent);

                if imported || (constant_like(name) && value_position(node, parent)) {
                    push_use_under(self.out, from, name, None, node, Some(parent), src, line);
                }
            }
            // `{ siteContent }`: the shorthand of an imported name passes its value on
            "shorthand_property_identifier" if self.imported.values.contains(text(node, src)) => {
                push_use_under(self.out, from, text(node, src), None, node, parent, src, line);
            }
            "member_expression" => {
                // `Color.Red`, `limits.MAX_ROWS`: a constant named through its owner
                let (Some(object), Some(property)) =
                    (node.child_by_field_name("object"), node.child_by_field_name("property"))
                else {
                    return;
                };
                let name = text(property, src);
                // `ns.member` of `import * as ns` names an export of that module, whatever its case
                let named = constant_like(name) || self.imported.namespaces.contains(text(object, src));

                if object.kind() == "identifier"
                    && property.kind() == "property_identifier"
                    && named
                    && !parent.is_some_and(|p| {
                        p.kind() == "call_expression"
                            && p.child_by_field_name("function").is_some_and(|f| f.id() == node.id())
                    })
                {
                    push_use_under(
                        self.out,
                        from,
                        name,
                        Some(text(object, src).to_string()),
                        node,
                        parent,
                        src,
                        line,
                    );
                }
            }
            "string" => {
                let value = text(node, src).trim_matches(['\'', '"']);

                if ident_like(value) {
                    push_ref(self.out, from, value, "str", None, line);
                } else if !parent.is_some_and(|parent| parent.kind().starts_with("jsx_")) {
                    // a class name or other markup attribute is no command
                    push_word_under(self.out, from, value, node, parent, src);
                }
            }
            "new_expression" => {
                if let Some((name, qual, _)) = node.child_by_field_name("constructor").and_then(|c| callee(c, src)) {
                    push_ref(self.out, from, &name, "new", qual, line);
                }
            }
            _ => {}
        }
    }
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
    fn components_calls_imports_and_strings() {
        let out = run(
            "src/App.tsx",
            "import Foo, { bar as baz } from './x';\nimport * as ns from '$lib/y';\n\
             export function App() { baz(); ns.run(); return <Foo.Bar a={invoke('git_file_lines')} /> }\n",
        );

        let imports: Vec<(&str, &str, &str)> = out
            .imports
            .iter()
            .map(|i| (i.local.as_str(), i.orig.as_str(), i.source.as_str()))
            .collect();
        assert_eq!(
            imports,
            vec![("Foo", "Foo", "./x"), ("baz", "bar", "./x"), ("ns", "*", "$lib/y")]
        );

        let refs: Vec<(&str, &str, Option<&str>)> = out
            .refs
            .iter()
            .map(|r| (r.name.as_str(), r.kind, r.qual.as_deref()))
            .collect();
        assert!(refs.contains(&("baz", "call", None)));
        assert!(refs.contains(&("run", "mcall", Some("ns"))));
        assert!(refs.contains(&("Bar", "component", Some("Foo"))));
        assert!(refs.contains(&("git_file_lines", "str", None)));
        assert!(out.syms.iter().any(|s| s.name == "App" && s.kind == "fn"));
    }

    #[test]
    fn an_imported_name_is_a_use_in_any_case_and_a_called_one_is_only_a_call() {
        let out = run(
            "a.ts",
            "import { siteContent, run } from './x';\nimport * as ns from './y';\n\
             function f(local) { run(); local.a; return [siteContent.en, ns.value, ns]; }\n",
        );

        let uses: Vec<(&str, Option<&str>)> = out
            .refs
            .iter()
            .filter(|r| r.kind == "use")
            .map(|r| (r.name.as_str(), r.qual.as_deref()))
            .collect();

        assert_eq!(uses, vec![("siteContent", None), ("value", Some("ns"))]);
        assert!(out.refs.iter().any(|r| r.kind == "call" && r.name == "run"));
    }

    #[test]
    fn plain_typescript_has_no_jsx_components() {
        let out = run("a.ts", "const x = a < B > (c);\nclass K { m() {} }\nnew K();\n");

        assert!(out.refs.iter().all(|r| r.kind != "component"));
        assert!(
            out.syms
                .iter()
                .any(|s| s.name == "m" && s.parent.as_deref() == Some("K"))
        );
        assert!(out.refs.iter().any(|r| r.kind == "new" && r.name == "K"));
    }

    #[test]
    fn functions_put_in_object_properties_are_props_with_the_function_they_are_passed_to() {
        let out = run(
            "a.js",
            "import { runIt, other } from './x';\nimport * as ns from './y';\nfunction local() {}\n\
             export const runner = create({ run: runIt, fallback: svc.cb ?? local, bound: other.bind(null), \
             shared: ns.member, name: 'x', count: 3, this_: this.method });\n\
             const plain = { local };\n",
        );
        let props: Vec<(&str, &str, Option<&str>)> = out
            .refs
            .iter()
            .filter(|r| r.kind == "prop")
            .map(|r| (r.name.as_str(), r.qual.as_deref().unwrap_or(""), r.recv.as_deref()))
            .collect();

        assert_eq!(
            props,
            vec![
                ("run", "runIt", Some("create")),
                ("fallback", "local", Some("create")),
                ("bound", "other", Some("create")),
                ("shared", "ns.member", Some("create")),
                ("local", "local", None),
            ]
        );
    }

    /// The class tokens of a file as sorted (token, qualifier).
    fn classes(out: &Extract) -> Vec<(String, Option<String>)> {
        let mut found: Vec<(String, Option<String>)> = out
            .refs
            .iter()
            .filter(|r| r.kind == "cls")
            .map(|r| (r.name.clone(), r.qual.clone()))
            .collect();

        found.sort();

        found
    }

    fn tokens(out: &Extract) -> Vec<String> {
        classes(out).into_iter().map(|(name, _)| name).collect()
    }

    #[test]
    fn a_class_attribute_and_the_class_helpers_name_their_classes() {
        let out = run(
            "src/A.tsx",
            "import clsx from 'clsx';\nexport function A({ cond, x }) {\n  return <div className=\"a b\" id=\"nope\">\
             <p className={cn('c', cond && 'd', { e: x, 'f-g': x }, ['h'])} class={clsx(cond ? 'i' : 'j')} />\
             <i className={`k ${x} l m-${x}`} /></div>;\n}\n\
             const t = classnames('n'); const u = classNames('o'); const v = twMerge('p q');\n",
        );

        assert_eq!(
            tokens(&out),
            [
                "a", "b", "c", "d", "e", "f-g", "h", "i", "j", "k", "l", "n", "o", "p", "q"
            ]
        );
        assert!(out.refs.iter().filter(|r| r.kind == "cls").all(|r| !r.style));
    }

    #[test]
    fn the_line_of_a_class_is_the_line_it_is_written_on() {
        let out = run(
            "src/A.tsx",
            "const a = <p\n className=\"x\n  y\" />;\nconst b = clsx(\n 'z');\n",
        );
        let lines: Vec<(&str, usize)> = out
            .refs
            .iter()
            .filter(|r| r.kind == "cls")
            .map(|r| (r.name.as_str(), r.line))
            .collect();

        assert_eq!(lines, vec![("x", 2), ("y", 3), ("z", 5)]);
    }

    #[test]
    fn a_condition_or_a_string_outside_a_class_position_is_no_class() {
        let out = run(
            "src/A.tsx",
            "export function A({ cond }) {\n  const s = 'card'; fn('card'); const o = { title: 'card', kind: 'box' };\n\
             return <p title=\"card\" className={cond === 'x' ? 'a' : 'b'} />;\n}\n\
             const w = clsx(mode === 'dark' && 'z', k ?? 'y');\n",
        );

        assert_eq!(tokens(&out), ["a", "b", "y", "z"]);
    }

    #[test]
    fn an_object_key_named_class_holds_classes_and_a_plain_key_does_not() {
        let out = run(
            "src/a.ts",
            "const props = { class: 'one two', className: ['three'], 'class': 'four', id: 'five', label: 'six' };\n",
        );

        assert_eq!(tokens(&out), ["four", "one", "three", "two"]);
    }

    #[test]
    fn cva_reads_its_base_variant_values_and_compound_classes_but_no_names() {
        let out = run(
            "src/button.ts",
            "export const button = cva('base-a base-b', {\n  variants: {\n    intent: { primary: 'p-1', secondary: ['s-1', 's-2'] },\n    size: { sm: 'text-sm' },\n  },\n\
             compoundVariants: [{ intent: 'primary', size: 'sm', class: 'combo' }],\n  defaultVariants: { intent: 'primary', size: 'sm' },\n});\n",
        );

        assert_eq!(
            tokens(&out),
            ["base-a", "base-b", "combo", "p-1", "s-1", "s-2", "text-sm"]
        );
    }

    #[test]
    fn a_word_that_is_no_token_makes_none() {
        let out = run(
            "src/A.tsx",
            "const a = clsx('md:flex w-[10px] !x w-1/2 hover:bg-red ok', `btn-${x} ${y} a${z}`, 'sk_live_abcdefghijklmnop');\n",
        );

        assert_eq!(tokens(&out), ["ok"]);
    }

    #[test]
    fn a_class_is_kept_whatever_the_definition_around_it_is_named() {
        let out = run(
            "src/A.tsx",
            "export const AuthLayout = () => <div className=\"auth-box\" />;\n\
             export function Keyboard() { return clsx('key-cap'); }\n\
             export function Monkey() { return clsx('banana'); }\n\
             export function Author() { return clsx('by-line'); }\n\
             export function TokenInput() { return clsx('token-box'); }\n\
             function f() { const password = clsx('pw-box'); return password; }\n",
        );

        assert_eq!(
            tokens(&out),
            ["auth-box", "banana", "by-line", "key-cap", "pw-box", "token-box"]
        );
        // a token that holds a credential is still not kept
        let key = ["gh", "p_", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8"].concat();

        assert!(tokens(&run("src/A.tsx", &format!("const a = clsx('{key}', 'ok');\n"))) == ["ok"]);
    }

    #[test]
    fn a_name_is_followed_to_its_constant_unless_it_means_a_secret() {
        let out = run(
            "src/A.tsx",
            "const authCard = 'auth-card'; const keyClass = 'key-class'; const password = 'pw-shown'; const apiKey = 'api-shown'; const authToken = 'tk-shown';\n\
             export const A = () => <p className={clsx(authCard, keyClass, password, apiKey, authToken)} />;\n",
        );

        assert_eq!(tokens(&out), ["auth-card", "key-class"]);
    }

    #[test]
    fn a_constant_in_a_method_is_read_though_the_method_has_its_name() {
        let out = run(
            "src/A.tsx",
            "class A { foo() { const foo = 'inner-class'; return <p className={foo} />; } }\n",
        );

        assert_eq!(tokens(&out), ["inner-class"]);
    }

    #[test]
    fn names_that_find_nothing_are_paid_for_and_a_file_of_them_says_it_lost_uses() {
        let names: Vec<String> = (0..9_000).map(|n| format!("n{n}")).collect();
        let out = run(
            "src/A.tsx",
            &format!(
                "export const A = () => <p className={{clsx({})}} />;\n",
                names.join(", ")
            ),
        );

        assert!(out.classes_cut);
        assert!(!run("src/A.tsx", "export const A = () => <p className={clsx(a, b)} />;\n").classes_cut);
    }

    #[test]
    fn a_tree_past_the_depth_marks_the_file_as_cut() {
        let deep = format!("const a = {}1{};\nclsx('x-y');\n", "[".repeat(450), "]".repeat(450));
        let out = run("src/A.ts", &deep);

        assert!(out.classes_cut);
        assert!(out.dropped > 0);
        assert!(!run("src/A.ts", "const a = [1];\n").classes_cut);
    }

    #[test]
    fn a_reference_on_a_line_that_holds_markup_is_marked_whatever_it_is() {
        let out = run(
            "a.tsx",
            "export function A() {\n  const y = f(); const x = <div>text</div>;\n  return target(<p>text</p>);\n  const z = c ? other() : <b>text</b>;\n  const w = <div\n    a={b}\n  >hi</div>; after();\n  plain();\n}\n",
        );
        let marked = |name: &str| out.refs.iter().find(|r| r.name == name).map(|r| r.markup);

        assert_eq!(marked("f"), Some(true));
        assert_eq!(marked("target"), Some(true));
        assert_eq!(marked("other"), Some(true));
        assert_eq!(marked("after"), Some(true));
        assert_eq!(marked("plain"), Some(false));
    }

    #[test]
    fn a_member_of_an_imported_stylesheet_is_a_class_of_that_module_only() {
        let out = run(
            "src/A.tsx",
            "import styles from './A.module.css?inline';\nimport other from './other';\nimport * as ns from './b.scss';\n\
             export const A = () => <p className={styles.card} id={styles['row-x']}>{other.skip}{ns.deep}</p>;\n\
             const c = styles.free; const d = other.nope;\n",
        );

        assert_eq!(
            classes(&out),
            vec![
                ("card".to_string(), Some("styles".to_string())),
                ("deep".to_string(), Some("ns".to_string())),
                ("free".to_string(), Some("styles".to_string())),
                ("row-x".to_string(), Some("styles".to_string())),
            ]
        );
    }

    #[test]
    fn a_sheet_import_without_a_name_is_recorded_and_other_import_tables_are_unchanged() {
        let out = run(
            "src/A.tsx",
            "import './card.css';\nimport 'polyfill';\nimport './x.scss?raw';\nimport 'https://cdn.example/a.css';\nimport a from './a';\n",
        );
        let imports: Vec<(&str, &str, &str)> = out
            .imports
            .iter()
            .map(|i| (i.local.as_str(), i.orig.as_str(), i.source.as_str()))
            .collect();

        assert_eq!(
            imports,
            vec![
                ("sheet:./card.css", "*", "./card.css"),
                ("sheet:./x.scss", "*", "./x.scss"),
                ("a", "a", "./a"),
            ]
        );
    }

    #[test]
    fn a_repeated_token_is_kept_once_and_a_thousand_and_first_marks_the_file() {
        let out = run(
            "src/A.tsx",
            "function f() { clsx('x', 'x'); return clsx('x'); }\nfunction g() { return clsx('x'); }\n",
        );

        // once for each definition that writes it
        assert_eq!(tokens(&out), ["x", "x"]);
        assert!(!out.classes_cut);

        let many: Vec<String> = (0..1_001).map(|n| format!("c{n}")).collect();
        let out = run("src/B.tsx", &format!("const a = clsx('{}');\n", many.join(" ")));

        assert_eq!(tokens(&out).len(), 1_000);
        assert!(out.classes_cut);
    }

    #[test]
    fn a_helper_bound_to_a_stylesheet_module_reaches_that_module_only() {
        let out = run(
            "src/A.tsx",
            "import classNames from 'classnames/bind';\nimport styles from './a.module.css';\nimport other from './b.module.css';\n\
             const cx = classNames.bind(styles);\nconst global = classnames.bind(somewhere);\nexport const ox = classNames.bind(other);\n\
             export function A({ on }) {\n  return <div className={cx('card', { active: on }, on && 'lit')} id={global('plain')}>\n\
             <i className={ox('o')} /><b className={cx('a', clsx('g'))} /></div>;\n}\n",
        );

        assert_eq!(
            classes(&out),
            [
                ("a".to_string(), Some("styles".to_string())),
                ("active".to_string(), Some("styles".to_string())),
                ("card".to_string(), Some("styles".to_string())),
                ("g".to_string(), None),
                ("lit".to_string(), Some("styles".to_string())),
                ("o".to_string(), Some("other".to_string())),
                ("plain".to_string(), None),
            ]
        );
    }

    #[test]
    fn a_call_of_a_name_that_was_not_bound_to_a_class_helper_is_no_class_position() {
        let out = run(
            "src/A.tsx",
            "import styles from './a.module.css';\nconst cx = makeIt(styles);\nconst y = cx('card');\nfunction f(cx) { return cx('x'); }\n",
        );

        assert!(classes(&out).is_empty());
    }

    /// The class refs of a file as sorted (token, line, owner name).
    fn placed(out: &Extract) -> Vec<(String, usize, String)> {
        let mut found: Vec<(String, usize, String)> = out
            .refs
            .iter()
            .filter(|r| r.kind == "cls")
            .map(|r| {
                let owner = r
                    .from
                    .and_then(|from| out.syms.get(from))
                    .map_or("", |sym| sym.name.as_str());

                (r.name.clone(), r.line, owner.to_string())
            })
            .collect();

        found.sort();

        found
    }

    #[test]
    fn class_list_calls_name_their_classes_on_any_receiver() {
        let out = run(
            "src/a.ts",
            "el.classList.add('a', 'b c', x);\nthis.node.classList.remove('d');\ndoc?.body.classList.toggle('e', 'not-this');\n\
             if (el.classList.contains('f', 'nor-this')) {}\nel.classList.replace('g', 'h', 'nor-this');\n\
             classList.add('free');\nel.add('plain');\nlist.classList.item('item');\n",
        );

        assert_eq!(tokens(&out), ["a", "b", "c", "d", "e", "f", "g", "h"]);
    }

    #[test]
    fn a_class_name_assignment_reads_its_right_side() {
        let out = run(
            "src/a.ts",
            "el.className = 'a b';\nel.className += ' c';\nel.className = ok ? 'd' : 'e';\nel.id = 'no';\nel.className = cn('f');\nel.classNames = 'g';\n",
        );

        assert_eq!(tokens(&out), ["a", "b", "c", "d", "e", "f"]);
    }

    #[test]
    fn a_selector_a_query_method_reads_gives_its_class_tokens_and_not_those_it_excludes() {
        let out = run(
            "src/a.ts",
            "document.querySelector('.a > .b:not(.c)');\nel.querySelectorAll(`.d [data-x=\".no\"]`);\nel.closest('.e, #f');\n\
             el.matches('.g');\nel.getElementsByClassName('h i');\nfoo('.nope');\nel.querySelector(`.p-${x}`);\n\
             matches('.free');\nel.querySelector(ok ? '.j' : '.k');\nel.on('.l');\n",
        );

        assert_eq!(tokens(&out), ["a", "b", "d", "e", "g", "h", "i", "j", "k"]);
    }

    #[test]
    fn a_line_of_a_selector_is_the_line_its_class_is_on() {
        let out = run("src/a.ts", "x;\nel.querySelector(`\n  .a\n  .b`);\n");

        assert_eq!(
            placed(&out),
            vec![("a".to_string(), 3, String::new()), ("b".to_string(), 4, String::new())]
        );
    }

    #[test]
    fn an_attribute_or_key_ending_in_class_name_holds_classes_and_the_other_style_props_do_not() {
        let out = run(
            "src/A.tsx",
            "export const A = () => <W wrapperClassName=\"a\" iconClassName={cn('b')} classes=\"no\" sx=\"no2\" style=\"no3\" ClassName=\"no4\" />;\n\
             const o = { labelClassName: 'c', 'inputClassName': 'd', classes: 'no5', sx: 'no6', style: 'no7' };\n",
        );

        assert_eq!(tokens(&out), ["a", "b", "c", "d"]);
    }

    #[test]
    fn a_constant_read_in_a_class_position_gives_its_classes_at_the_use() {
        let out = run(
            "src/A.tsx",
            "const base = 'card box';\nconst mode = on ? 'x' : 'y';\nconst list = ['l1', `l2`];\nconst help = cn('h1', { h2: a });\n\
             const never = 'unused';\nconst twice = base;\nconst deep = twice;\n\
             export function A() {\n  const local = 'loc';\n  return <div className={base}>\n    <p className={cn(mode, 'z', list)} class={deep} id={help} title={local} />\n    <b className={local} />\n  </div>;\n}\n",
        );

        // `help` is a helper call at its declaration: that read is the older one, at the declaration, and `id` is
        // no class position
        assert_eq!(
            placed(&out),
            vec![
                ("box".to_string(), 10, "A".to_string()),
                ("card".to_string(), 10, "A".to_string()),
                ("h1".to_string(), 4, "help".to_string()),
                ("h2".to_string(), 4, "help".to_string()),
                ("l1".to_string(), 11, "A".to_string()),
                ("l2".to_string(), 11, "A".to_string()),
                ("loc".to_string(), 12, "A".to_string()),
                ("x".to_string(), 11, "A".to_string()),
                ("y".to_string(), 11, "A".to_string()),
                ("z".to_string(), 11, "A".to_string()),
            ]
        );
        assert!(!tokens(&out).contains(&"unused".to_string()));
    }

    #[test]
    fn a_constant_is_followed_in_the_other_class_sources_too() {
        let out = run(
            "src/a.ts",
            "const c1 = 'a'; const sel = '.b'; const c2 = ['c'];\nel.classList.add(c1);\nel.className = c2;\nel.querySelector(sel);\nel.closest(`${sel}`);\n\
             const p = <W iconClassName={c1} />;\n",
        );

        assert_eq!(tokens(&out), ["a", "b", "c"]);
    }

    #[test]
    fn a_constant_that_is_not_surely_one_is_not_followed() {
        let out = run(
            "src/A.tsx",
            "import { imp } from './x';\nlet lv = 'a'; lv = 'a2'; var vv = 'b'; const { dd } = obj; const o = { k: 'c' };\nconst shadowed = 'd';\n\
             const t = 'ok-top';\n\
             export function A(param = 'e', { destructured }, shadowed) {\n  const { t: inner } = obj;\n  return <p className={cn(lv, vv, dd, imp, param, destructured, o.k, shadowed, unknown)} />;\n}\n\
             function B() { let shadowed = 'f'; return <p className={shadowed} />; }\n\
             function C() { const shadowed = 'g'; return <p className={shadowed} />; }\n\
             function D() { return <p className={shadowed} />; }\n\
             function E() { { const blocked = 'h'; } return <p className={blocked} />; }\n\
             function F(t) { return <p className={t} />; }\n\
             function G() { for (const t of []) { return <p className={t} />; } }\n\
             function H() { try {} catch (t) { return <p className={t} />; } }\n",
        );

        assert_eq!(tokens(&out), ["d", "g"]);
    }

    #[test]
    fn a_constant_of_one_function_is_no_name_of_another_and_a_later_top_level_one_still_reads() {
        let out = run(
            "src/A.tsx",
            "function A() { const mine = 'a'; return <p className={mine} />; }\nfunction B() { return <p className={mine} />; }\n\
             function C() { return <p className={late} />; }\nconst late = 'c';\n",
        );

        assert_eq!(
            placed(&out),
            vec![
                ("a".to_string(), 1, "A".to_string()),
                ("c".to_string(), 3, "C".to_string())
            ]
        );
    }

    #[test]
    fn a_constant_named_for_a_credential_is_not_followed() {
        let out = run(
            "src/A.tsx",
            "const dbPassword = 'hidden'; const ok = 'shown';\nconst a = <p className={dbPassword} /> ; const b = <p className={ok} />;\n",
        );

        assert_eq!(tokens(&out), ["shown"]);
    }

    #[test]
    fn chains_of_constants_stop_at_the_limit_and_a_cycle_ends() {
        let out = run(
            "src/a.ts",
            "const a = b; const b = a;\nel.className = a;\nconst c1 = 'ok1'; const c2 = c1; const c3 = c2; const c4 = c3; const c5 = c4; const c6 = c5;\n\
             el.className = c4;\nel.className = c6;\n",
        );

        assert_eq!(tokens(&out), ["ok1"]);
        assert!(out.classes_cut);
    }

    #[test]
    fn hostile_constants_are_read_within_their_budgets() {
        let count = 6_000;
        let chain: String = (0..count).map(|i| format!("const c{} = c{};\n", i + 1, i)).collect();
        let big = format!("const big = [{}];\n", "'tok', ".repeat(40_000));
        let uses = "el.className = big;\n".repeat(5_000);
        let wide = format!(
            "const w0 = ['x'];\n{}el.className = w6;\n",
            (1..=6)
                .map(|i| format!("const w{i} = [{}];\n", format!("w{}, ", i - 1).repeat(200)))
                .collect::<String>()
        );
        let deep = format!(
            "const d = {}'z'{};\nel.className = d;\n",
            "[".repeat(20_000),
            "]".repeat(20_000)
        );

        for source in [
            format!("const c0 = 'a';\n{chain}el.className = c{count};\n"),
            format!("{big}{uses}"),
            wide,
            deep,
        ] {
            let started = std::time::Instant::now();
            let out = std::thread::Builder::new()
                .stack_size(2 * 1024 * 1024)
                .spawn(move || super::super::extract(&mut tree_sitter::Parser::new(), "a.ts", &source))
                .expect("thread")
                .join()
                .expect("no stack overflow");

            assert!(
                started.elapsed() < super::super::FILE_LIMIT / 10,
                "{:?}",
                started.elapsed()
            );
            assert!(out.refs.iter().filter(|r| r.kind == "cls").count() <= 1_000);
        }
    }

    /// The `base` references of a file as (base, derived).
    fn bases(out: &Extract) -> Vec<(&str, &str)> {
        out.refs
            .iter()
            .filter(|r| r.kind == "base")
            .map(|r| (r.name.as_str(), r.qual.as_deref().unwrap_or("")))
            .collect()
    }

    #[test]
    fn a_class_or_interface_records_what_it_extends_or_implements() {
        let out = run(
            "ts/a.ts",
            "class A extends Base<T> implements I, ns.J {}\ninterface K extends L, M<X> {}\nclass Plain {}\nclass D extends mixin(Base) {}\n",
        );

        assert_eq!(
            bases(&out),
            [("Base", "A"), ("I", "A"), ("J", "A"), ("L", "K"), ("M", "K")]
        );

        let js = run("ts/b.js", "class A extends B {}\nclass C extends ns.D {}\n");

        assert_eq!(bases(&js), [("B", "A"), ("D", "C")]);
    }

    #[test]
    fn a_reference_inside_jsx_is_marked_as_written_in_markup_and_one_outside_it_is_not() {
        let out = run(
            "a.tsx",
            "export function A() {\n  plain();\n  const x = <div />;\n  return <p onClick={() => { inner(); }}>{shown()}<Card /></p>;\n}\nafter();\n",
        );
        let marked = |name: &str| out.refs.iter().find(|r| r.name == name).map(|r| r.markup);

        assert_eq!(marked("plain"), Some(false));
        assert_eq!(marked("after"), Some(false));
        assert_eq!(marked("shown"), Some(true));
        assert_eq!(marked("inner"), Some(true));
        assert_eq!(marked("Card"), Some(true));
    }

    #[test]
    fn a_file_cut_off_deep_inside_a_call_is_read_in_linear_time() {
        // the parser keeps what it cannot read as one flat node of thousands of children; asking each child for
        // its parent made the walk quadratic (23 s for 200 KB). The shapes make a one-letter string, a word, a
        // name of a command, a constant and an object literal, each one asking for a parent of its own
        for unit in ["a?'b':", "a?'beta':", "a?'get_beta':", "FOO.BAR?'b':", "x=a?{k:v}:"] {
            let mut src = String::from("clsx(");

            while src.len() < 200_000 {
                src.push_str(unit);
            }

            let started = std::time::Instant::now();
            let out = run("a.ts", &src);

            assert!(
                started.elapsed() < super::super::FILE_LIMIT / 10,
                "{unit} {:?}",
                started.elapsed()
            );
            assert_eq!(out.dropped, 0, "{unit}");
        }
    }
}
