//! SCSS and CSS resolution: `ns.$token`, `ns.fn()` and `@include ns.mixin` go to the module `@use "x" as ns`
//! loads; every other name resolves by name, nearest package first.
//!
//! A module path is looked up from the importing file's folder (a relative path) and from the repository
//! root, as `x.scss`, the partial `_x.scss`, `x/_index.scss` and `x/index.scss`. A module no file holds
//! (`sass:math`, a package of `node_modules`) is external: what is read through it is dropped. A module
//! that holds no definition of the name (it only `@forward`s it) leaves the name to the by-name steps.

use super::{Graph, Imp, Ref, Rules};
use crate::code::packages::normalize;

pub(super) struct ScssRules;

pub(super) static RULES: ScssRules = ScssRules;

impl Rules for ScssRules {
    fn qualified(&self, graph: &Graph, r: &Ref, qual: &str, cands: &mut Vec<usize>) -> Option<Vec<usize>> {
        let import = graph.imports.get(&(r.file, qual.to_string()))?;
        let Some(modules) = graph.scss_module(r.file, &import.source) else {
            return Some(Vec::new());
        };
        let inside: Vec<usize> = cands
            .iter()
            .copied()
            .filter(|s| graph.syms.get(*s).is_some_and(|sym| modules.contains(&sym.file)))
            .collect();

        (!inside.is_empty()).then_some(inside)
    }

    fn external(&self, graph: &Graph, file: usize, import: &Imp) -> bool {
        graph.scss_module(file, &import.source).is_none()
    }
}

impl Graph {
    /// The files `@use "source"` loads from file `from`; `None` when no file of the repository is it.
    fn scss_module(&self, from: usize, source: &str) -> Option<Vec<usize>> {
        if source.contains(':') {
            return None; // `sass:math`
        }

        let dir = self.files.get(from)?.path.rsplit_once('/').map_or("", |(dir, _)| dir);
        let source = source.strip_prefix('~').unwrap_or(source);
        let found = [normalize(&format!("{dir}/{source}")), normalize(source)]
            .iter()
            .map(|base| self.scss_files(base))
            .find(|files| !files.is_empty())?;

        Some(found)
    }

    /// `base.scss`, `_base.scss`, `base/_index.scss`, `base/index.scss` (and the same with `.css`).
    fn scss_files(&self, base: &str) -> Vec<usize> {
        let (dir, name) = base.rsplit_once('/').map_or(("", base), |(dir, name)| (dir, name));
        let at = |rel: String| {
            self.path_idx
                .get(&if dir.is_empty() { rel } else { format!("{dir}/{rel}") })
                .copied()
        };
        let mut files = Vec::new();

        for ext in ["scss", "css"] {
            files.extend(at(format!("{name}.{ext}")));
            files.extend(at(format!("_{name}.{ext}")));
            files.extend(self.path_idx.get(&format!("{base}/_index.{ext}")).copied());
            files.extend(self.path_idx.get(&format!("{base}/index.{ext}")).copied());
        }

        files
    }
}
