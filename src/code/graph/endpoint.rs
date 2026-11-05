//! What the end of a query names: a symbol, a file of the repository, or a string that names no symbol.
//!
//! `code_path`, `code_symbol` and `code_impact` take a name (`name`, `Type::name`, `module::name`,
//! `path/fragment::name`). An agent often knows a file, or a command or event name written as a string, and not
//! a symbol, so a text that names no symbol is read as the next of these:
//!
//! 1. a file of the repository (`ui/src/Panel.svelte`, also `./ui/src/Panel.svelte`, the absolute path, or a
//!    file name that only one file ends with): all its definitions, and for a start also the references it
//!    makes outside any definition;
//! 2. a string that is written in the code and names no definition (an event name): the definitions that write
//!    it;
//! 3. words (`/useful-skills doctor`, `the diff view asks for the full lines`): the definitions whose name or
//!    word literals hold them (see `words`).
//!
//! A name that is a symbol stays a symbol whatever else it names. The answer says which reading was used
//! whenever the end holds more than one definition.

use std::collections::HashMap;

use super::{Graph, shown};
use crate::code::extract;

/// Definitions of one file that an end holds, the first ones in the order of the file. A file with more is
/// cut.
const MAX_FILE_ENDS: usize = 400;

/// Files an end may hold when the text is a file name that several files end with.
const MAX_ENDS_FILES: usize = 4;

/// Definitions that write a string and are used as an end.
const MAX_MENTION_ENDS: usize = 64;

/// Definitions or places named in a note.
const NOTE_LISTED: usize = 4;

/// Definitions of a query that centre a map: the best ones of words, which are guesses.
const MAX_MAP_SEEDS: usize = 12;

/// What an end of a query was taken for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Reading {
    Symbol,
    File,
    Text,
    Words,
}

/// The candidates of an end made of words that name no one definition.
pub(super) struct Unsure {
    /// What was wrong, as `no definition holds all of a, b`.
    pub why: String,
    /// The best few definitions, one line each.
    pub lines: Vec<String>,
}

/// The definitions an end of a query stands for.
pub(super) struct Endpoint {
    pub reading: Reading,
    pub syms: Vec<usize>,
    /// References made outside any definition in the files of the end (the code at the top of a file): where a
    /// chain from the end may also begin.
    pub module_refs: Vec<usize>,
    /// What the text was taken for, when more than one definition stands for it.
    pub note: Option<String>,
    /// Words that name no one definition (see `words`): `code_path` does not guess a chain from them.
    pub unsure: Option<Unsure>,
    /// What words were taken for when one definition alone is the end: the note of that definition.
    pub sure_note: Option<String>,
    /// How many of the first definitions are the end when words are not unsure.
    pub sure: usize,
}

impl Endpoint {
    pub(super) fn none() -> Self {
        Self {
            reading: Reading::Symbol,
            syms: Vec::new(),
            module_refs: Vec::new(),
            note: None,
            unsure: None,
            sure_note: None,
            sure: 0,
        }
    }

    /// True when nothing is named.
    pub(super) fn is_empty(&self) -> bool {
        self.syms.is_empty() && self.module_refs.is_empty()
    }
}

/// What a `code_map` query names: the definitions, the files that hold them, and the files that use them.
pub(super) struct MapSeeds {
    pub files: Vec<usize>,
    /// The definitions the query names; for a file, all of its definitions.
    pub syms: Vec<usize>,
    /// The files (not among `files`, no tests) that use what the query names, the most references first.
    pub callers: Vec<usize>,
}

impl Graph {
    /// The files and definitions a `code_map` query (a symbol, a file, a string or words) stands for, tests left
    /// out, and only those under `prefix` when there is one.
    pub(super) fn map_seeds(&self, query: &str, prefix: Option<&str>) -> MapSeeds {
        // what is outside the prefix is left out before a reading cuts its list: the one definition under the focus
        // may be the least used
        let end = self.endpoint_in(query, usize::MAX, prefix);
        let live = |file: usize| self.files.get(file).is_some_and(|f| !f.test);
        let under = |file: usize| {
            live(file) && prefix.is_none_or(|prefix| self.files.get(file).is_some_and(|f| f.path.starts_with(prefix)))
        };
        let defined_in = |files: &[usize]| -> Vec<usize> {
            files
                .iter()
                .filter_map(|file| self.file_syms.get(*file))
                .flatten()
                .copied()
                .filter(|sym| self.syms.get(*sym).is_some_and(|s| !s.test))
                .collect()
        };
        // `station` as a string a page writes is the class of `.station`: the sheet defines it, the page uses it
        let selectors: Vec<usize> = if end.reading == Reading::Text || end.syms.is_empty() {
            self.by_name
                .get(&format!(".{query}"))
                .map(Vec::as_slice)
                .unwrap_or(&[])
                .iter()
                .copied()
                .filter(|sym| self.is_selector(*sym) && self.syms.get(*sym).is_some_and(|s| !s.test && under(s.file)))
                .take(MAX_MAP_SEEDS)
                .collect()
        } else {
            Vec::new()
        };
        let (files, syms): (Vec<usize>, Vec<usize>) = if !selectors.is_empty() {
            let mut files: Vec<usize> = Vec::new();

            for file in selectors.iter().filter_map(|sym| self.syms.get(*sym)).map(|s| s.file) {
                if !files.contains(&file) {
                    files.push(file);
                }
            }

            (files, selectors)
        } else if end.reading == Reading::File {
            let files: Vec<usize> = self
                .files_named(query, prefix)
                .into_iter()
                .filter(|file| under(*file))
                .collect();
            let syms = defined_in(&files);

            (files, syms)
        } else {
            let mut named: Vec<usize> = end
                .syms
                .iter()
                .copied()
                .filter(|sym| self.syms.get(*sym).is_some_and(|s| !s.test && under(s.file)))
                .collect();
            let mut files: Vec<usize> = Vec::new();

            named.truncate(MAX_MAP_SEEDS);

            for file in named.iter().filter_map(|sym| self.syms.get(*sym)).map(|s| s.file) {
                if !files.contains(&file) {
                    files.push(file);
                }
            }

            (files, named)
        };
        // what no definition holds may still be what a folder or a kind of file is called: for a map, unlike an end
        // of `code_path`, a whole file is a fair answer
        let (files, syms) = if files.is_empty() {
            let files = self.files_of_words(query, prefix);
            let syms = defined_in(&files);

            (files, syms)
        } else {
            (files, syms)
        };

        // who uses what is named: the ranking runs from users to what they use, so these get no rank from it
        let mut uses: HashMap<usize, usize> = HashMap::new();

        for sym in &syms {
            for ri in self.incoming.get(*sym).map(Vec::as_slice).unwrap_or(&[]) {
                if let Some(file) = self
                    .refs
                    .get(*ri)
                    .map(|r| r.file)
                    .filter(|f| live(*f) && !files.contains(f))
                {
                    *uses.entry(file).or_default() += 1;
                }
            }
        }

        let mut callers: Vec<(usize, usize)> = uses.into_iter().collect();

        callers.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        MapSeeds {
            files,
            syms,
            callers: callers.into_iter().map(|(file, _)| file).collect(),
        }
    }

    /// The end `query` names. A symbol keeps at most `symbols` of its definitions (the most used first).
    pub(super) fn endpoint(&self, query: &str, symbols: usize) -> Endpoint {
        self.endpoint_in(query, symbols, None)
    }

    /// [`Graph::endpoint`] of what is under the path prefix `scope` alone: the candidates outside it are left out
    /// before any reading cuts its list, so the one definition that is under it is not lost to better ones elsewhere.
    pub(super) fn endpoint_in(&self, query: &str, symbols: usize, scope: Option<&str>) -> Endpoint {
        let mut found = self.find(query);

        found.retain(|sym| self.syms.get(*sym).is_some_and(|s| self.in_scope(s.file, scope)));

        if !found.is_empty() {
            let total = found.len();

            found.truncate(symbols);

            let note = (found.len() > 1).then(|| {
                let places: Vec<String> = found.iter().take(NOTE_LISTED).map(|sym| self.place(*sym)).collect();

                format!(
                    "`{}` matches {total} definitions, used: {}{}",
                    shown(query),
                    places.join(", "),
                    more(found.len(), NOTE_LISTED)
                )
            });

            return Endpoint {
                reading: Reading::Symbol,
                syms: found,
                module_refs: Vec::new(),
                note,
                unsure: None,
                sure_note: None,
                sure: 0,
            };
        }

        let files = self.files_named(query, scope);

        if !files.is_empty() {
            return self.file_endpoint(query, &files);
        }

        let text = self.text_endpoint(query, scope);

        if !text.is_empty() {
            return text;
        }

        self.words_endpoint(query, scope)
    }

    /// True when `file` is under the path prefix `scope`, or there is none.
    pub(super) fn in_scope(&self, file: usize, scope: Option<&str>) -> bool {
        scope.is_none_or(|prefix| self.files.get(file).is_some_and(|f| f.path.starts_with(prefix)))
    }

    /// What to say of a query that names a file the index holds no text of, rather than guess from its words.
    pub(super) fn left_out_note(&self, query: &str) -> Option<String> {
        let readings = self.readings(query);
        let written = readings.last()?.trim_start_matches('/');

        if written.is_empty() || extract::spec_for(written).is_none() || !self.files_named(query, None).is_empty() {
            return None;
        }

        let path = self.left_out.iter().find(|path| {
            readings.iter().any(|reading| reading == *path)
                || path.strip_suffix(written).is_some_and(|before| before.ends_with('/'))
        })?;

        // the attributes are the repository's own claim: an agent must not take them for a check of the file
        if self.attr_marked.binary_search(path).is_ok() {
            return Some(format!(
                "`{}` is marked generated by `.gitattributes` and is not in the index; that is the repository's claim, \
                 not verified. Read the file itself.",
                shown(path)
            ));
        }

        Some(format!(
            "`{}` is in the repository but not in the index: a tool wrote it (generated or minified code), or it is too \
             large or unreadable. Read the file itself; what it was built from is indexed under its own path.",
            shown(path)
        ))
    }

    /// A line for the end of an answer when `.gitattributes` keeps files out of the index (it can hide code):
    /// how many, and no path: whoever writes the repository chooses the paths, and the line would repeat that
    /// string on every answer. The query that names a file gets its own note (see [`Graph::left_out_note`]).
    /// Empty when it keeps none.
    pub(super) fn attributes_note(&self) -> String {
        if self.attr_marked.is_empty() {
            return String::new();
        }

        format!(
            "{} file(s) are left out because .gitattributes marks them generated; see .gitattributes\n",
            self.attr_marked.len()
        )
    }

    /// A line for the end of a `code_symbol` answer that finds nothing, or that is about a stylesheet that lost
    /// selectors to the caps of the index: the selector asked for may be one of them. Empty when no stylesheet
    /// did, or when the answer is neither. `answer` is what the question found.
    pub(super) fn selectors_note(&self, query: &str, answer: &str) -> String {
        if self.selector_cut.is_empty() {
            return String::new();
        }

        let missed = answer.starts_with("No symbol, file or command matches");
        let about = !missed
            && self.files_named(query.trim(), None).iter().any(|file| {
                self.files
                    .get(*file)
                    .is_some_and(|f| self.selector_cut.binary_search(&f.path).is_ok())
            });

        if !missed && !about {
            return String::new();
        }

        format!(
            "{} stylesheet(s) had selectors left out past the index caps or for holding a credential\n",
            self.selector_cut.len()
        )
    }

    /// The files a text may mean: the one with that path, else the (few) files whose path ends with it.
    fn files_named(&self, query: &str, scope: Option<&str>) -> Vec<usize> {
        let readings = self.readings(query);

        for reading in &readings {
            if let Some(&file) = self.path_idx.get(reading).filter(|file| self.in_scope(**file, scope)) {
                return vec![file];
            }
        }

        // a file name, or the end of a path: only for a text that looks like a source file
        let Some(written) = readings.last().map(|reading| reading.trim_start_matches('/')) else {
            return Vec::new();
        };

        if written.is_empty() || extract::spec_for(written).is_none() {
            return Vec::new();
        }

        let mut files: Vec<usize> = (0..self.files.len())
            .filter(|file| {
                self.in_scope(*file, scope)
                    && self.files.get(*file).is_some_and(|f| {
                        f.path == written || f.path.strip_suffix(written).is_some_and(|before| before.ends_with('/'))
                    })
            })
            .collect();

        files.sort_by_key(|file| self.files.get(*file).map(|f| (f.test, f.path.len(), f.path.as_str())));
        files.truncate(MAX_ENDS_FILES);

        files
    }

    fn file_endpoint(&self, query: &str, files: &[usize]) -> Endpoint {
        let mut syms: Vec<usize> = Vec::new();
        let mut total = 0_usize;

        for &file in files {
            let list = self.file_syms.get(file).map(Vec::as_slice).unwrap_or(&[]);

            total += list.len();
            syms.extend(list.iter().copied());
        }

        // the tests of a file are its definitions too, but production ones lead
        syms.sort_by_key(|sym| (self.syms.get(*sym).is_some_and(|s| s.test), *sym));
        syms.truncate(MAX_FILE_ENDS);

        let module_refs: Vec<usize> = self
            .refs
            .iter()
            .enumerate()
            .filter(|(_, r)| r.from.is_none() && files.contains(&r.file) && r.kind != "type")
            .map(|(ri, _)| ri)
            .collect();
        let paths: Vec<String> = files
            .iter()
            .take(NOTE_LISTED)
            .filter_map(|file| self.files.get(*file))
            .map(|file| shown(&file.path).into_owned())
            .collect();
        let cut = if total > syms.len() {
            format!(", the first {} used", syms.len())
        } else {
            String::new()
        };
        let note = if files.len() > 1 {
            format!(
                "`{}` matches {} files, used: {}{}: {}{cut}",
                shown(query),
                files.len(),
                paths.join(", "),
                more(files.len(), NOTE_LISTED),
                definitions(total)
            )
        } else {
            format!("file {}: {}{cut}", paths.join(", "), definitions(total))
        };

        Endpoint {
            reading: Reading::File,
            syms,
            module_refs,
            note: Some(note),
            unsure: None,
            sure_note: None,
            sure: 0,
        }
    }

    /// A string written in the code that names no definition: the definitions that write it.
    fn text_endpoint(&self, query: &str, scope: Option<&str>) -> Endpoint {
        let Some(places) = self.loose.get(query) else {
            return Endpoint::none();
        };
        let mut syms: Vec<usize> = places
            .iter()
            .filter_map(|place| place.from)
            .filter(|sym| self.syms.get(*sym).is_some_and(|s| self.in_scope(s.file, scope)))
            .collect();

        syms.sort_by_key(|sym| (self.syms.get(*sym).is_some_and(|s| s.test), *sym));
        syms.dedup();
        syms.truncate(MAX_MENTION_ENDS);

        let listed: Vec<String> = places
            .iter()
            .take(NOTE_LISTED)
            .filter_map(|place| {
                let file = self.files.get(place.file)?;

                Some(format!("{}:{}", shown(&file.path), place.line))
            })
            .collect();
        let note = format!(
            "`{}` is a string, not a symbol: written in {} ({}{}), {} used",
            shown(query),
            if places.len() == 1 {
                "1 place".to_owned()
            } else {
                format!("{} places", places.len())
            },
            listed.join(", "),
            more(places.len(), NOTE_LISTED),
            definitions(syms.len())
        );

        Endpoint {
            reading: Reading::Text,
            syms,
            module_refs: Vec::new(),
            note: Some(note),
            unsure: None,
            sure_note: None,
            sure: 0,
        }
    }

    /// `label path:line` of a definition.
    pub(super) fn place(&self, sym: usize) -> String {
        format!("{} {}", self.label(sym), self.loc(sym))
    }
}

/// `1 definition` or `N definitions`.
fn definitions(count: usize) -> String {
    if count == 1 {
        "1 definition".to_owned()
    } else {
        format!("{count} definitions")
    }
}

/// `, … N more` when `total` is past the `listed` names of a note.
fn more(total: usize, listed: usize) -> String {
    if total > listed {
        format!(", … {} more", total - listed)
    } else {
        String::new()
    }
}
