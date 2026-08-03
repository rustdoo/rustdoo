//! Port of `odoo/addons/base/models/ir_asset.py`: turning the `assets`
//! declarations of the installed addons into the ordered file list of
//! each client bundle.
//!
//! An addon does not own a bundle — it contributes to one. The order the
//! contributions land in is the dependency order of the modules, and
//! inside a module the declaration order of the manifest. The directives
//! (`before`/`after`/`replace`/`remove`) then move things around, which
//! is how a downstream addon patches a bundle it did not create.

use crate::manifest::{AssetDirective, AssetEntry, Manifest};
use rusdoo_core::RusdooError;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// One file in a bundle: the module-qualified path the client asks for
/// (`web/static/src/core/utils.js`) plus where it lives on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetFile {
    pub module: String,
    /// module-qualified, `/`-separated, always relative
    pub path: String,
    pub disk: PathBuf,
}

impl AssetFile {
    /// The lowercased extension, which is what splits a bundle into its
    /// js / css / xml halves when serving.
    pub fn extension(&self) -> &str {
        self.path
            .rsplit_once('.')
            .map(|(_, ext)| ext)
            .unwrap_or("")
    }
}

/// Every bundle of the installed addons, resolved to real files.
#[derive(Debug, Default, Clone)]
pub struct Bundles {
    bundles: BTreeMap<String, Vec<AssetFile>>,
}

impl Bundles {
    /// The files of a bundle, in load order. An unknown bundle is empty
    /// rather than an error: the client asks for bundles that only exist
    /// once some addon is installed.
    pub fn files(&self, bundle: &str) -> &[AssetFile] {
        self.bundles.get(bundle).map_or(&[], Vec::as_slice)
    }

    /// The files of a bundle carrying one of `extensions`, in load order.
    pub fn files_with_extension<'a>(
        &'a self,
        bundle: &str,
        extensions: &'a [&str],
    ) -> impl Iterator<Item = &'a AssetFile> {
        self.files(bundle)
            .iter()
            .filter(move |file| extensions.contains(&file.extension()))
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.bundles.keys().map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.bundles.is_empty()
    }
}

/// The bundles of every installable addon under `paths`, plus where each
/// addon lives — what a server needs to serve assets, whatever else the
/// boot does. It only reads the filesystem, so it is safe (and correct)
/// to run on every start, not just when installing.
pub fn resolve_installed(
    paths: &[&Path],
) -> Result<(Bundles, HashMap<String, PathBuf>), RusdooError> {
    let manifests = crate::loader::discover_addons(paths)?;
    let order = crate::graph::dependency_order(&manifests)?;
    let by_name: HashMap<&str, &Manifest> =
        manifests.iter().map(|m| (m.name.as_str(), m)).collect();
    let installed: Vec<&Manifest> = order
        .iter()
        .map(|name| by_name[name.as_str()])
        .filter(|manifest| manifest.installable)
        .collect();
    let roots = installed
        .iter()
        .map(|manifest| (manifest.name.clone(), manifest.path.clone()))
        .collect();
    Ok((resolve_bundles(&installed)?, roots))
}

/// A declaration still to be applied: which module made it, and what it
/// said. Kept per bundle in module order.
struct Declaration<'a> {
    module: &'a str,
    entry: &'a AssetEntry,
}

/// Resolve the bundles of `manifests`, which MUST already be in
/// dependency order (see [`crate::graph::dependency_order`]) — that
/// order is the load order of the files.
pub fn resolve_bundles(manifests: &[&Manifest]) -> Result<Bundles, RusdooError> {
    let mut roots: HashMap<&str, &Path> = HashMap::new();
    for manifest in manifests {
        roots.insert(manifest.name.as_str(), manifest.path.as_path());
    }

    // pass 1: collect the declarations of every bundle, in module order,
    // so that a bundle may `include` one declared by a later module.
    let mut declarations: BTreeMap<&str, Vec<Declaration>> = BTreeMap::new();
    for manifest in manifests {
        for entry in &manifest.assets {
            declarations
                .entry(entry.bundle.as_str())
                .or_default()
                .push(Declaration {
                    module: manifest.name.as_str(),
                    entry,
                });
        }
    }

    // pass 2: apply them. Globs are expanded against the addon
    // directories, with each directory walked at most once.
    let mut walker = Walker::new(roots);
    let mut bundles = BTreeMap::new();
    for bundle in declarations.keys().copied() {
        let mut building = Vec::new();
        build_bundle(bundle, &declarations, &mut walker, &mut building)?;
    }
    // build_bundle memoizes into the walker; harvest the results
    for (bundle, files) in walker.done {
        bundles.insert(bundle, files);
    }
    Ok(Bundles { bundles })
}

/// Build one bundle, expanding `include` directives inline (as Odoo does)
/// so the result is flat. `building` is the include stack, which is what
/// makes a cycle an error instead of a hang.
fn build_bundle(
    bundle: &str,
    declarations: &BTreeMap<&str, Vec<Declaration>>,
    walker: &mut Walker,
    building: &mut Vec<String>,
) -> Result<Vec<AssetFile>, RusdooError> {
    if let Some(done) = walker.done.get(bundle) {
        return Ok(done.clone());
    }
    if building.iter().any(|name| name == bundle) {
        building.push(bundle.to_string());
        return Err(RusdooError::Validation(format!(
            "asset bundle include cycle: {}",
            building.join(" -> ")
        )));
    }
    building.push(bundle.to_string());

    let mut paths = AssetPaths::default();
    for declaration in declarations.get(bundle).map_or(&[][..], Vec::as_slice) {
        let entry = declaration.entry;
        if entry.directive == AssetDirective::Include {
            let included = build_bundle(&entry.path, declarations, walker, building)?;
            paths.append(included);
            continue;
        }
        let files = walker.expand(declaration.module, &entry.path)?;
        match entry.directive {
            AssetDirective::Append => paths.append(files),
            AssetDirective::Prepend => paths.prepend(files),
            AssetDirective::Remove => {
                let target = entry.path.clone();
                let removed = paths.remove(&files);
                if removed == 0 {
                    return Err(RusdooError::Validation(format!(
                        "module {}: 'remove' of {target:?} in bundle {bundle:?} \
                         matched nothing already in the bundle",
                        declaration.module
                    )));
                }
            }
            AssetDirective::Before | AssetDirective::After | AssetDirective::Replace => {
                let target = entry.target.as_deref().unwrap_or_default();
                let target_files = walker.expand(declaration.module, target)?;
                let Some(index) = paths.position_of(&target_files) else {
                    return Err(RusdooError::Validation(format!(
                        "module {}: {:?} in bundle {bundle:?} targets {target:?}, \
                         which is not in the bundle",
                        declaration.module, entry.directive
                    )));
                };
                match entry.directive {
                    AssetDirective::Before => paths.insert(index, files),
                    AssetDirective::After => paths.insert(index + 1, files),
                    AssetDirective::Replace => {
                        paths.insert(index, files);
                        paths.remove(&target_files);
                    }
                    _ => unreachable!("outer match narrowed the directive"),
                }
            }
            AssetDirective::Include => unreachable!("handled above"),
        }
    }

    building.pop();
    let files = paths.into_vec();
    walker.done.insert(bundle.to_string(), files.clone());
    Ok(files)
}

/// The list being built for one bundle. A file appears once: re-adding
/// one already there is a no-op, like Odoo's `AssetPaths`.
#[derive(Default)]
struct AssetPaths {
    files: Vec<AssetFile>,
    seen: HashSet<String>,
}

impl AssetPaths {
    fn append(&mut self, files: Vec<AssetFile>) {
        let at = self.files.len();
        self.insert(at, files);
    }

    fn prepend(&mut self, files: Vec<AssetFile>) {
        self.insert(0, files);
    }

    /// Insert at `index`, keeping the given order and skipping files the
    /// bundle already holds.
    fn insert(&mut self, index: usize, files: Vec<AssetFile>) {
        let mut at = index.min(self.files.len());
        for file in files {
            if !self.seen.insert(file.path.clone()) {
                continue;
            }
            self.files.insert(at, file);
            at += 1;
        }
    }

    /// Drop every file of `files` from the bundle; answers how many went.
    fn remove(&mut self, files: &[AssetFile]) -> usize {
        let dropping: HashSet<&str> = files.iter().map(|file| file.path.as_str()).collect();
        let before = self.files.len();
        self.files.retain(|file| !dropping.contains(file.path.as_str()));
        self.seen.retain(|path| !dropping.contains(path.as_str()));
        before - self.files.len()
    }

    /// Where the first of `files` sits in the bundle — the anchor a
    /// `before`/`after`/`replace` positions against.
    fn position_of(&self, files: &[AssetFile]) -> Option<usize> {
        let wanted: HashSet<&str> = files.iter().map(|file| file.path.as_str()).collect();
        self.files
            .iter()
            .position(|file| wanted.contains(file.path.as_str()))
    }

    fn into_vec(self) -> Vec<AssetFile> {
        self.files
    }
}

/// Expands asset path definitions against the addon directories, walking
/// each directory at most once, and memoizes the built bundles.
struct Walker<'a> {
    roots: HashMap<&'a str, &'a Path>,
    /// module -> every file under it, module-qualified and sorted
    listings: HashMap<String, Vec<String>>,
    done: BTreeMap<String, Vec<AssetFile>>,
}

impl<'a> Walker<'a> {
    fn new(roots: HashMap<&'a str, &'a Path>) -> Self {
        Walker {
            roots,
            listings: HashMap::new(),
            done: BTreeMap::new(),
        }
    }

    /// Expand one path definition — a literal file or a glob — into the
    /// files it names, in a deterministic order.
    ///
    /// `declaring` is the module that wrote the declaration, used only to
    /// say who is at fault when the definition is bad.
    fn expand(&mut self, declaring: &str, definition: &str) -> Result<Vec<AssetFile>, RusdooError> {
        let bad = |what: &str| {
            RusdooError::Validation(format!(
                "module {declaring}: asset path {definition:?} {what}"
            ))
        };
        let definition = definition.trim_start_matches('/');
        // a path is `module/inside/the/module`, and never leaves it
        if definition.split('/').any(|segment| segment == "..") {
            return Err(bad("must not escape its module with '..'"));
        }
        let Some((module, rest)) = definition.split_once('/') else {
            return Err(bad("must start with a module name"));
        };
        if rest.is_empty() {
            return Err(bad("names no file inside the module"));
        }
        let Some(root) = self.roots.get(module).copied() else {
            return Err(bad(&format!(
                "belongs to module {module:?}, which is not installed"
            )));
        };
        let root = root.to_path_buf();

        if !is_glob(definition) {
            let disk = root.join(rest);
            if !disk.is_file() {
                // warned and skipped, not refused, which is Odoo's own
                // behaviour (`IrAsset._get_paths` logs and returns
                // nothing). It is not the stricter rule that is right
                // here: Odoo's own `web` manifest names a file its tree
                // does not ship, and a port that refused it could not
                // serve the addon this port exists to be compatible with.
                tracing::warn!("asset path {definition:?} resolves to nothing, skipped");
                return Ok(Vec::new());
            }
            return Ok(vec![AssetFile {
                module: module.to_string(),
                path: definition.to_string(),
                disk,
            }]);
        }

        // a glob that matches nothing is fine (an addon may declare a
        // directory it only fills in some configurations)
        let listing = self.listing(module, &root)?;
        Ok(listing
            .iter()
            .filter(|path| glob_match(definition, path))
            .map(|path| AssetFile {
                module: module.to_string(),
                path: path.clone(),
                disk: root.join(path.strip_prefix(module).unwrap_or(path).trim_start_matches('/')),
            })
            .collect())
    }

    /// Every file under a module, module-qualified and sorted. Walked
    /// once per module, however many globs point at it.
    fn listing(&mut self, module: &str, root: &Path) -> Result<&Vec<String>, RusdooError> {
        if !self.listings.contains_key(module) {
            let mut files = Vec::new();
            walk_dir(root, module, &mut files)?;
            files.sort();
            self.listings.insert(module.to_string(), files);
        }
        Ok(self.listings.get(module).expect("just inserted"))
    }
}

/// Collect every file under `dir` as `prefix/relative/path`. Symlinked
/// directories are not followed, so a loop on disk cannot hang the boot.
fn walk_dir(dir: &Path, prefix: &str, out: &mut Vec<String>) -> Result<(), RusdooError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // a module without the directory a glob points at is not an error
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(RusdooError::Validation(format!(
                "cannot read {}: {error}",
                dir.display()
            )))
        }
    };
    for entry in entries {
        let entry = entry.map_err(|error| {
            RusdooError::Validation(format!("cannot read {}: {error}", dir.display()))
        })?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            tracing::warn!("skipping non-UTF8 asset file in {}", dir.display());
            continue;
        };
        let file_type = entry.file_type().map_err(|error| {
            RusdooError::Validation(format!("cannot stat {}: {error}", entry.path().display()))
        })?;
        let qualified = format!("{prefix}/{name}");
        if file_type.is_dir() {
            walk_dir(&entry.path(), &qualified, out)?;
        } else if file_type.is_file() {
            out.push(qualified);
        }
    }
    Ok(())
}

fn is_glob(pattern: &str) -> bool {
    pattern.contains(['*', '?', '['])
}

/// Match a `/`-separated path against a glob: `*` and `?` inside a
/// segment, `**` for zero or more segments (Python's `glob(recursive=True)`,
/// which is what Odoo manifests are written against).
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').collect();
    let path: Vec<&str> = path.split('/').collect();
    match_segments(&pattern, &path)
}

fn match_segments(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.first() {
        None => path.is_empty(),
        Some(&"**") => (0..=path.len()).any(|skip| match_segments(&pattern[1..], &path[skip..])),
        Some(segment) => {
            !path.is_empty()
                && match_segment(segment, path[0])
                && match_segments(&pattern[1..], &path[1..])
        }
    }
}

/// `*` (any run of characters) and `?` (one character) within a segment.
fn match_segment(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    // classic two-pointer wildcard match: linear, and no recursion to
    // blow the stack on a pathological pattern
    let (mut p, mut n) = (0usize, 0usize);
    let (mut star, mut resume) = (None, 0usize);
    while n < name.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            resume = n;
            p += 1;
        } else if let Some(last_star) = star {
            p = last_star + 1;
            resume += 1;
            n = resume;
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_wildcards() {
        assert!(match_segment("*.js", "utils.js"));
        assert!(!match_segment("*.js", "utils.css"));
        assert!(match_segment("a?c.js", "abc.js"));
        assert!(!match_segment("a?c.js", "ac.js"));
        assert!(match_segment("*", "anything"));
        assert!(match_segment("a*b*c", "aXXbYYc"));
        assert!(!match_segment("a*b*c", "aXXbYY"));
    }

    #[test]
    fn double_star_spans_directories() {
        assert!(glob_match("web/static/**/*.js", "web/static/src/core/x.js"));
        // ** matches zero directories too
        assert!(glob_match("web/static/**/*.js", "web/static/x.js"));
        assert!(!glob_match("web/static/**/*.js", "web/static/x.css"));
        assert!(!glob_match("web/static/*.js", "web/static/src/x.js"));
    }

    #[test]
    fn a_literal_path_is_not_a_glob() {
        assert!(!is_glob("web/static/src/main.js"));
        assert!(is_glob("web/static/**/*.js"));
    }
}
