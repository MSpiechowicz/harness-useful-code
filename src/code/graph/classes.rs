//! Class links: a class token written in code (a `cls` reference) reaches the stylesheet selectors that
//! style it.
//!
//! A selector styles a class when the class is one of its `.name` parts, so `card` reaches `.card`,
//! `.card .title`, `.card:hover` and `.a.card`, and not `.cardigan`. A class inside `:not(...)` is excluded by
//! the selector, not styled by it, so it counts for nothing; `:is`, `:where` and `:has` count.
//!
//! A token has many selectors in a large stylesheet and many places write it, so the links are built apart from
//! the code references, with caps of their own, and never decide whether a repository is too large. Code
//! edges (callers, the edge budget, the ranking) do not see them. Which selectors a use reaches is the first
//! tier that finds any:
//!
//! 1. the same file (a component and its `<style>` block, a page and its `<style>`);
//! 2. the stylesheets the file imports, and those they import (a few levels deep);
//! 3. the same package;
//! 4. the whole repository.
//!
//! The first two are certain. The last two match the name alone and read `?`, and never take a selector of a
//! `<style>` block (that belongs to the file that holds it). A CSS module is reached by none of the four with a
//! written name: it renames its classes, so only `styles.card` reads the module `styles` is imported from, and
//! `$style.card` only the file's own blocks.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use super::{File, Graph, Imp, NAME_ONLY_WEIGHT, POLL_EVERY, Ref, Sym, shown};
use crate::code::pace::Pace;
use crate::code::packages::normalize;
use crate::code::walk::CANCELLED;

/// Selectors one token is kept for. A token on more of them is counted, not listed, so a hostile stylesheet of
/// a hundred thousand `.a-N .x` rules costs one number for `x`.
pub(super) const MAX_SELECTORS_PER_TOKEN: usize = 1_024;

/// The most selectors one use of a token links to: the exact `.token` first, then the shortest selectors.
const MAX_CLASS_TARGETS: usize = 64;

/// The class parts of one selector that are looked up.
const MAX_TOKENS_PER_SELECTOR: usize = 16;

/// The class uses a graph keeps after the ones no selector styles are dropped.
pub(super) const MAX_CLASS_REFS: usize = 300_000;

/// The links all class uses of a graph may make together, and the selectors and imports the lookups may visit.
const MAX_CLASS_EDGES: usize = 1_000_000;
const CLASS_SCAN_BUDGET: usize = 50_000_000;

/// What the lookup of the stylesheets a use names (`styles.card`) takes from [`CLASS_SCAN_BUDGET`]: it reads
/// paths and module maps, which is more than one step of a scan.
const SHEET_LOOKUP_COST: usize = 128;

/// The part of the time left when the class pass starts that it may spend (one in this many). The graph
/// load that follows it and the ranking of the trial need the rest, so a pass that is slow gives up its
/// own uses and never the call.
const CLASS_TIME_SHARE: u32 = 4;

/// How far, and through how many files, the stylesheets a file imports are followed.
const MAX_SHEET_DEPTH: usize = 4;
const MAX_SHEET_FILES: usize = 64;

/// The extensions of a stylesheet an import may name.
const SHEET_EXTENSIONS: [&str; 4] = ["css", "scss", "sass", "less"];

/// The selectors that hold one class token.
#[derive(Default)]
pub(super) struct Selectors {
    /// The selectors (symbol indexes) kept, in symbol order. The exact `.token` selectors are kept past the
    /// cap, so a busy token still reaches its own rule.
    syms: Vec<usize>,
    /// How many selectors are exactly `.token`.
    exact: usize,
    /// How many selectors hold the token, kept or not.
    total: usize,
}

impl Selectors {
    /// True when the token is on more selectors than one use links to.
    pub(super) fn is_over(&self) -> bool {
        self.total > MAX_CLASS_TARGETS
    }

    pub(super) fn total(&self) -> usize {
        self.total
    }

    fn add(&mut self, sym: usize, exact: bool) {
        self.total += 1;

        if exact {
            self.exact += 1;
        }

        if self.syms.len() < MAX_SELECTORS_PER_TOKEN || (exact && self.exact <= MAX_CLASS_TARGETS) {
            self.syms.push(sym);
        }
    }
}

/// Per class token: the selectors that hold it. Only tokens some file writes are kept.
pub(super) type ClassIndex = HashMap<String, Selectors>;

/// What the class links left out, for the answers that count users.
#[derive(Default)]
pub(super) struct Report {
    /// Per token on more selectors than are linked: how many hold it.
    pub over: HashMap<String, usize>,
    /// The files whose class tokens were cut when they were stored.
    pub cut_files: usize,
    /// The `.html` pages the walk left out: each may have written class names.
    pub cut_pages: usize,
    /// The class uses known to be unread: dropped past the cap at load, or left without links by the budgets.
    pub cut_uses: usize,
    /// The links made.
    pub edges: usize,
    /// The tokens some use was linked for. A selector with no users whose token is here lost the use to a
    /// nearer stylesheet, which is not the same as nobody writing the class.
    pub written: HashSet<String>,
}

/// True when `name` holds `#id` outside an attribute (`a[href="#top"]`), and not the `#{` of an interpolated name.
fn names_an_id(name: &str) -> bool {
    let mut attribute = 0_usize;
    let mut chars = name.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '[' => attribute += 1,
            ']' => attribute = attribute.saturating_sub(1),
            '#' if attribute == 0 && chars.peek() != Some(&'{') => return true,
            _ => {}
        }
    }

    false
}

/// The class tokens of a selector name: the `.name` parts, at most [`MAX_TOKENS_PER_SELECTOR`].
///
/// Left out are a part inside a string or an attribute (`[a=".x"]`), inside `:not(...)` (the selector excludes
/// that class), followed by `#{` (an interpolated name that is only a prefix) or by `\` (an escaped name), and
/// one that starts with a digit.
pub(in crate::code) fn selector_tokens(name: &str) -> Vec<&str> {
    let bytes = name.as_bytes();
    let mut tokens: Vec<&str> = Vec::new();
    // per open parenthesis: true when it is inside `:not(`
    let mut excluded: Vec<bool> = Vec::new();
    let mut at = 0;

    while let Some(&byte) = bytes.get(at) {
        if tokens.len() >= MAX_TOKENS_PER_SELECTOR {
            break;
        }

        match byte {
            b'"' | b'\'' => at = after_string(bytes, at),
            b'[' => at = after_bracket(bytes, at),
            b'\\' => at += 2,
            b'(' => {
                let negated = at >= 4
                    && bytes
                        .get(at - 4..at)
                        .is_some_and(|word| word.eq_ignore_ascii_case(b":not"));
                let inherited = excluded.last().copied().unwrap_or(false);

                excluded.push(inherited || negated);
                at += 1;
            }
            b')' => {
                excluded.pop();
                at += 1;
            }
            b'.' => {
                let start = at + 1;
                let mut end = start;

                while bytes.get(end).is_some_and(|b| is_ident_byte(*b)) {
                    end += 1;
                }

                at = end.max(at + 1);

                let interpolated = bytes.get(end) == Some(&b'#') && bytes.get(end + 1) == Some(&b'{');
                let escaped = bytes.get(end) == Some(&b'\\');
                let numeric = bytes.get(start).is_some_and(u8::is_ascii_digit);

                if let Some(token) = name.get(start..end).filter(|token| !token.is_empty())
                    && !interpolated
                    && !escaped
                    && !numeric
                    && !excluded.last().copied().unwrap_or(false)
                    && !tokens.contains(&token)
                {
                    tokens.push(token);
                }
            }
            _ => at += 1,
        }
    }

    tokens
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') || byte >= 0x80
}

/// The index after the string that starts at `at` (the end of the text when it is not closed).
fn after_string(bytes: &[u8], at: usize) -> usize {
    let Some(&quote) = bytes.get(at) else {
        return at + 1;
    };
    let mut end = at + 1;

    while let Some(&byte) = bytes.get(end) {
        match byte {
            b'\\' => end += 2,
            _ if byte == quote => return end + 1,
            _ => end += 1,
        }
    }

    bytes.len()
}

/// The index after the attribute selector that starts at `at`.
fn after_bracket(bytes: &[u8], at: usize) -> usize {
    let mut end = at + 1;

    while let Some(&byte) = bytes.get(end) {
        match byte {
            b'"' | b'\'' => end = after_string(bytes, end),
            b']' => return end + 1,
            _ => end += 1,
        }
    }

    bytes.len()
}

/// True for a stylesheet extension of an import (`./a.module.css?inline`).
fn is_sheet_source(source: &str) -> bool {
    let bare = source.split(['?', '#']).next().unwrap_or(source);

    bare.rsplit_once('.').is_some_and(|(_, extension)| {
        SHEET_EXTENSIONS
            .iter()
            .any(|known| extension.eq_ignore_ascii_case(known))
    })
}

/// True for a CSS module (`Panel.module.css`): its names belong to the file that imports it. A component that
/// is called `Own.module.vue` is no module: its `<style>` is its own.
fn is_module_sheet(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .is_some_and(|name| name.contains(".module.") && is_sheet_source(name))
}

/// The selectors of the stylesheets of a repository by the class tokens in `used`.
pub(super) fn index(syms: &[Sym], files: &[File], used: &HashSet<String>) -> ClassIndex {
    let mut index = ClassIndex::new();

    if used.is_empty() {
        return index;
    }

    for (i, sym) in syms.iter().enumerate() {
        if sym.kind != "class" || !(sym.style || files.get(sym.file).is_some_and(|f| f.lang == "scss")) {
            continue;
        }

        for token in selector_tokens(&sym.name) {
            if !used.contains(token) {
                continue;
            }

            let exact = sym.name.strip_prefix('.') == Some(token);

            index.entry(token.to_string()).or_default().add(i, exact);
        }
    }

    index
}

/// What one class pass keeps between the uses it links.
struct Pass<'a> {
    /// What the lookups may still visit.
    left: usize,
    /// Per file: its imports, by local name.
    imports: Option<HashMap<usize, Vec<(&'a str, &'a Imp)>>>,
    /// Per file: the stylesheets it loads directly. A hub that thousands of files import is resolved once, not
    /// once per file that uses a class.
    direct: HashMap<usize, Rc<[usize]>>,
    /// Per file: the stylesheets it reaches.
    closures: HashMap<usize, Rc<[usize]>>,
    /// The cancellation and the time budget of the load.
    pace: &'a Pace<'a>,
    /// What was paid since the pace was last asked.
    since_check: usize,
    /// Why the pass stopped (cancelled, or out of time); every later payment fails at once.
    stopped: Option<String>,
}

impl Pass<'_> {
    /// Takes `n` from the budget; `None` when it is spent, or when the pass was cancelled or ran out of time.
    /// The pace is asked once for every [`POLL_EVERY`] units, whatever they pay for, so no lookup runs on
    /// unseen.
    fn pay(&mut self, n: usize) -> Option<()> {
        if self.stopped.is_some() {
            return None;
        }

        self.left = self.left.checked_sub(n)?;
        self.since_check += n;

        if self.since_check >= POLL_EVERY {
            self.since_check = 0;

            if let Err(error) = self.pace.check() {
                self.stopped = Some(error);

                return None;
            }
        }

        Some(())
    }
}

impl Graph {
    /// Links the class uses to the selectors that style them, into `res`. Returns the links made, the uses
    /// left unlinked because a budget ran out, and the tokens that got a link.
    ///
    /// Class links never decide that an index fails: when the time of the load runs out in here, the uses not
    /// yet linked are counted as unread and the graph stays usable. Only a cancel ends the pass with an error.
    pub(super) fn link_classes(
        &self,
        res: &mut [Vec<(usize, f32)>],
        index: &ClassIndex,
        pace: &Pace,
    ) -> Result<(usize, usize, HashSet<String>), String> {
        // the pass has a deadline of its own: what it does not finish is counted as unread, and the time it
        // might have used is left for the ranking that follows the load
        let own = pace.share(CLASS_TIME_SHARE);
        let pace = &own;
        let mut pass = Pass {
            left: CLASS_SCAN_BUDGET,
            imports: None,
            direct: HashMap::new(),
            closures: HashMap::new(),
            pace,
            since_check: 0,
            stopped: None,
        };
        let mut edges = 0_usize;
        let mut unlinked = 0_usize;
        let mut written: HashSet<String> = HashSet::new();

        for (seen, (ri, r)) in self
            .refs
            .iter()
            .enumerate()
            .filter(|(_, r)| r.kind == "cls")
            .enumerate()
        {
            if seen.is_multiple_of(POLL_EVERY) && pass.stopped.is_none() {
                pass.stopped = pace.check().err();
            }

            if pass.stopped.as_deref() == Some(CANCELLED) {
                return Err(CANCELLED.to_owned());
            }

            let Some(list) = index.get(&r.name) else {
                continue;
            };

            if edges >= MAX_CLASS_EDGES {
                unlinked += 1;
                continue;
            }

            let Some(mut targets) = self.class_targets(r, list, &mut pass) else {
                unlinked += 1;

                if pass.stopped.as_deref() == Some(CANCELLED) {
                    return Err(CANCELLED.to_owned());
                }

                continue;
            };

            if targets.len() > MAX_CLASS_EDGES - edges {
                targets.truncate(MAX_CLASS_EDGES - edges);
                unlinked += 1;
            }

            edges += targets.len();

            if !targets.is_empty() && !written.contains(&r.name) {
                written.insert(r.name.clone());
            }

            if let Some(slot) = res.get_mut(ri) {
                *slot = targets;
            }
        }

        Ok((edges, unlinked, written))
    }

    /// The selectors the use `r` of a token reaches, with their weights; `None` when the budget ran out.
    fn class_targets<'a>(&'a self, r: &Ref, list: &Selectors, pass: &mut Pass<'a>) -> Option<Vec<(usize, f32)>> {
        let from_test = self.files.get(r.file)?.test;
        // production code never reaches a selector that only tests style
        let usable = |s: usize| self.syms.get(s).is_some_and(|sym| from_test || !sym.test);

        match r.qual.as_deref() {
            // `$style.card` of a Vue block: the blocks of the file itself
            Some("$style") => {
                let found = self.pick(list, pass, |s| usable(s) && self.sym_file(s) == Some(r.file))?;

                Some(self.ranked(&r.name, found, 1.0))
            }
            // `styles.card` of `import styles from './x.module.css'`: that module and nothing else
            Some(qual) => {
                let Some(import) = self.imports.get(&(r.file, qual.to_string())) else {
                    return Some(Vec::new());
                };

                // the lookup runs for every use, so it is paid for like a scan of the list
                pass.pay(SHEET_LOOKUP_COST)?;

                let modules = self.sheet_files(r.file, &import.source);
                let found = self.pick(list, pass, |s| {
                    usable(s) && self.sym_file(s).is_some_and(|file| modules.contains(&file))
                })?;

                Some(self.ranked(&r.name, found, 1.0))
            }
            None => self.class_tiers(r, list, pass, usable),
        }
    }

    /// The first of the four tiers (see the module docs) that holds a selector for the use `r`.
    fn class_tiers<'a>(
        &'a self,
        r: &Ref,
        list: &Selectors,
        pass: &mut Pass<'a>,
        usable: impl Fn(usize) -> bool,
    ) -> Option<Vec<(usize, f32)>> {
        // a literal class (`card`) is a global name: it never reaches a CSS module, which renames its classes and
        // is read only through the name it is imported as (`styles.card`), however near it is
        let global = |s: usize| usable(s) && self.file_of(s).is_some_and(|file| !is_module_sheet(&file.path));
        let own = self.pick(list, pass, |s| global(s) && self.sym_file(s) == Some(r.file))?;

        if !own.is_empty() {
            return Some(self.ranked(&r.name, own, 1.0));
        }

        let reached = self.sheet_closure(r.file, pass)?;

        if !reached.is_empty() {
            let near = self.pick(list, pass, |s| {
                global(s) && self.sym_file(s).is_some_and(|file| reached.contains(&file))
            })?;

            if !near.is_empty() {
                return Some(self.ranked(&r.name, near, 1.0));
            }
        }

        // by name alone: a selector of a `<style>` block or of a CSS module is not reached this way
        let anywhere = |s: usize| {
            usable(s)
                && self.syms.get(s).is_some_and(|sym| !sym.style)
                && self.file_of(s).is_some_and(|file| !is_module_sheet(&file.path))
        };
        let pkg = self.files.get(r.file).map(|file| file.pkg.as_str())?;
        let package = self.pick(list, pass, |s| {
            anywhere(s) && self.file_of(s).is_some_and(|file| file.pkg == pkg)
        })?;

        if !package.is_empty() {
            return Some(self.ranked(&r.name, package, NAME_ONLY_WEIGHT));
        }

        let all = self.pick(list, pass, anywhere)?;

        Some(self.ranked(&r.name, all, NAME_ONLY_WEIGHT))
    }

    /// The selectors of `list` that pass `keep`; `None` when the scan cannot be paid.
    fn pick(&self, list: &Selectors, pass: &mut Pass, keep: impl Fn(usize) -> bool) -> Option<Vec<usize>> {
        pass.pay(list.syms.len())?;

        Some(list.syms.iter().copied().filter(|s| keep(*s)).collect())
    }

    /// At most [`MAX_CLASS_TARGETS`] of `found`: the exact `.token` first, then the shortest selectors, then the
    /// first defined.
    fn ranked(&self, token: &str, mut found: Vec<usize>, weight: f32) -> Vec<(usize, f32)> {
        let exact = format!(".{token}");
        let key = |s: &usize| {
            let name = self.syms.get(*s).map_or("", |sym| sym.name.as_str());

            (name != exact, name.len(), *s)
        };

        found.sort_by_key(key);
        found.truncate(MAX_CLASS_TARGETS);

        found.into_iter().map(|s| (s, weight)).collect()
    }

    fn sym_file(&self, sym: usize) -> Option<usize> {
        self.syms.get(sym).map(|s| s.file)
    }

    /// The stylesheets file `from` reaches through what it imports, `@use`s or `@import`s, and what those
    /// load in turn: at most [`MAX_SHEET_DEPTH`] levels and [`MAX_SHEET_FILES`] files, in the order found.
    /// `None` when the budget ran out.
    fn sheet_closure<'a>(&'a self, from: usize, pass: &mut Pass<'a>) -> Option<Rc<[usize]>> {
        if let Some(known) = pass.closures.get(&from) {
            return Some(Rc::clone(known));
        }

        let mut reached: Vec<usize> = Vec::new();
        let mut seen: HashSet<usize> = HashSet::from([from]);
        let mut level = vec![from];

        for _ in 0..MAX_SHEET_DEPTH {
            let mut next = Vec::new();

            for file in level {
                let direct = self.sheets_of(file, pass)?;

                for sheet in direct.iter().copied() {
                    // past the cap nothing more is kept, so a hub of thousands of imports is not walked on
                    if reached.len() >= MAX_SHEET_FILES {
                        break;
                    }

                    // the walk over a list that was looked up once is paid for every time it is walked
                    pass.pay(1)?;

                    if seen.insert(sheet) {
                        reached.push(sheet);
                        next.push(sheet);
                    }
                }
            }

            if next.is_empty() {
                break;
            }

            level = next;
        }

        let closure: Rc<[usize]> = reached.into();

        pass.closures.insert(from, Rc::clone(&closure));

        Some(closure)
    }

    /// The stylesheets file `file` loads directly. A stylesheet loads everything it imports; any other file only
    /// the sheets it names (`import './a.css'`, `import s from './a.module.css'`, a `<style>` block's `@use`).
    fn sheets_of<'a>(&'a self, file: usize, pass: &mut Pass<'a>) -> Option<Rc<[usize]>> {
        if let Some(known) = pass.direct.get(&file) {
            return Some(Rc::clone(known));
        }

        if pass.imports.is_none() {
            let mut by_file: HashMap<usize, Vec<(&str, &Imp)>> = HashMap::new();

            for ((owner, local), import) in &self.imports {
                by_file.entry(*owner).or_default().push((local.as_str(), import));
            }

            // the order of a hash map must not decide which files fit under the cap
            by_file
                .values_mut()
                .for_each(|list| list.sort_by_key(|(local, _)| *local));
            pass.imports = Some(by_file);
        }

        let entries = pass.imports.as_ref().and_then(|by_file| by_file.get(&file)).cloned();
        let Some(entries) = entries else {
            return Some(Rc::from([]));
        };
        let sheet = self.files.get(file)?.lang == "scss";

        pass.pay(entries.len() + 1)?;

        let mut found = Vec::new();

        for (local, import) in entries {
            let loads =
                sheet || local.starts_with("sheet:") || local.starts_with("style:") || is_sheet_source(&import.source);

            if loads {
                // every import is looked up in the paths and the module maps, which is paid for like a use
                pass.pay(SHEET_LOOKUP_COST)?;
                found.extend(self.sheet_files(file, &import.source));
            }
        }

        // an import that names a folder resolves to several files, and many imports name the same ones
        let mut seen = HashSet::new();

        found.retain(|sheet| seen.insert(*sheet));

        let direct: Rc<[usize]> = found.into();

        pass.direct.insert(file, Rc::clone(&direct));

        Some(direct)
    }

    /// The stylesheet files the specifier `source`, written in `from`, names: an exact path first, then an alias
    /// or a module path of any of the formats.
    fn sheet_files(&self, from: usize, source: &str) -> Vec<usize> {
        let dir = self
            .files
            .get(from)
            .map_or("", |file| file.path.rsplit_once('/').map_or("", |(dir, _)| dir));
        let bare = source.split(['?', '#']).next().unwrap_or(source);
        let bare = bare.strip_prefix('~').unwrap_or(bare);
        let exact = [normalize(&format!("{dir}/{bare}")), normalize(bare)]
            .iter()
            .find_map(|path| self.path_idx.get(path).copied());
        let mut found: Vec<usize> = match exact {
            Some(file) => vec![file],
            None => Vec::new(),
        };

        if found.is_empty() {
            found = self.ts_module(from, bare).unwrap_or_default();
        }

        if found.is_empty() {
            found = self.scss_module(from, bare).unwrap_or_default();
        }

        found.retain(|file| self.files.get(*file).is_some_and(|f| f.lang == "scss"));

        found
    }

    /// True for the definition of a selector in a stylesheet (or the `<style>` block of a component), the only
    /// definitions class uses can reach.
    pub(super) fn is_selector(&self, sym: usize) -> bool {
        self.syms
            .get(sym)
            .is_some_and(|s| s.kind == "class" && (s.style || self.lang_of_sym(sym) == "scss"))
    }

    /// True for a selector that names no class, id or placeholder (`*`, `html`, `:root`): it says the least about
    /// what a stylesheet holds.
    pub(super) fn is_generic_selector(&self, sym: usize) -> bool {
        self.is_selector(sym)
            && self.syms.get(sym).is_some_and(|s| {
                selector_tokens(&s.name).is_empty() && !s.name.starts_with('%') && !names_an_id(&s.name)
            })
    }

    /// How many places outside the tests write the class of the selector `sym`. Zero for any other definition.
    pub(super) fn class_uses(&self, sym: usize) -> usize {
        self.incoming.get(sym).map_or(0, |users| {
            users
                .iter()
                .filter(|ri| {
                    self.is_class(**ri)
                        && self
                            .refs
                            .get(**ri)
                            .and_then(|r| self.files.get(r.file))
                            .is_some_and(|file| !file.test)
                })
                .count()
        })
    }

    /// How a definition ranks among others by who uses it, for the sorts that put the most used first. The
    /// code that uses it counts first and the class uses (`cls` links) do not count with it, so a selector many
    /// pages write never leads a function. Of equal code, a selector comes after the other definitions, and
    /// selectors are told apart by their class uses (in a stylesheet the most written one leads).
    pub(super) fn use_rank(&self, sym: usize) -> (std::cmp::Reverse<usize>, bool, std::cmp::Reverse<usize>) {
        let users = self.incoming.get(sym).map_or(&[][..], Vec::as_slice);
        let classes = users.iter().filter(|ri| self.is_class(**ri)).count();

        (
            std::cmp::Reverse(users.len() - classes),
            self.is_selector(sym),
            std::cmp::Reverse(classes),
        )
    }

    /// The lines under `used by` for a selector that say how far to trust the count. `users` is how many
    /// places use it.
    pub(super) fn class_notes(&self, sym: usize, users: usize) -> Vec<String> {
        if !self.is_selector(sym) {
            return Vec::new();
        }

        let Some(def) = self.syms.get(sym) else {
            return Vec::new();
        };
        let tokens = selector_tokens(&def.name);

        if tokens.is_empty() {
            return Vec::new();
        }

        let mut notes: Vec<String> = tokens
            .iter()
            .filter_map(|token| self.classes.over.get(*token).map(|count| (token, count)))
            .take(3)
            .map(|(token, count)| {
                format!(
                    "class `{}` is on {count} selectors; uses are linked to the first {MAX_CLASS_TARGETS}, so this count may be low",
                    shown(token)
                )
            })
            .collect();
        let cut = self.classes.cut_files > 0 || self.classes.cut_uses > 0 || self.classes.cut_pages > 0;

        if cut {
            let mut parts = Vec::new();

            if self.classes.cut_files > 0 {
                parts.push(format!("{} files", self.classes.cut_files));
            }

            if self.classes.cut_uses > 0 {
                parts.push(format!("{} uses", self.classes.cut_uses));
            }

            if self.classes.cut_pages > 0 {
                parts.push(format!("{} pages", self.classes.cut_pages));
            }

            notes.push(format!(
                "class uses past the index caps were not read ({}), so this count may be low",
                parts.join(", ")
            ));
        } else if users == 0 && notes.is_empty() {
            // the nearest stylesheet that holds the token takes all its uses, so a selector in a farther one
            // reads zero although the class is written
            match tokens.iter().find(|token| self.classes.written.contains(**token)) {
                Some(token) => notes.push(format!(
                    "class `{}` is written elsewhere, and those uses are linked to another stylesheet",
                    shown(token)
                )),
                None => notes.push("(classes built at run time are not seen)".to_string()),
            }
        }

        // a selector with several classes is linked from the users of each of them, which reads as more than one
        // thing wrote the whole selector
        if tokens.len() >= 2 && users > 0 {
            notes.push("(counts uses of each of its classes)".to_string());
        }

        notes
    }
}

#[cfg(test)]
mod tests {
    use super::selector_tokens;

    #[test]
    fn a_selector_holds_the_classes_it_styles_and_not_the_ones_it_excludes_or_writes_in_a_string() {
        assert_eq!(selector_tokens(".card .title:hover"), ["card", "title"]);
        assert_eq!(selector_tokens(".a.b.a"), ["a", "b"]);
        assert_eq!(selector_tokens(":is(.a, .b) :where(.c) :has(.d)"), ["a", "b", "c", "d"]);
        assert_eq!(selector_tokens(".x:not(.a, :is(.b)) .y"), ["x", "y"]);
        assert_eq!(selector_tokens("a[href=\".x\"] .y[title='.z']"), ["y"]);
        assert_eq!(selector_tokens(".btn-#{$k} .b .md\\:flex .1x"), ["b"]);
        assert_eq!(selector_tokens("%base :root a::before"), Vec::<&str>::new());
    }

    #[test]
    fn a_selector_is_read_for_at_most_sixteen_classes() {
        let name: String = (0..40).map(|n| format!(".c{n}")).collect();

        assert_eq!(selector_tokens(&name).len(), 16);
    }
}
