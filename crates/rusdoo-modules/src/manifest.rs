//! Port of manifest handling in `odoo/modules/module.py`.

use crate::pyliteral::parse_py_literal;
use rusdoo_core::RusdooError;
use serde_json::Value;

/// The keys of an addon `__manifest__.py` that the loader consumes.
#[derive(Debug, Clone)]
pub struct Manifest {
    /// technical name (the addon directory name)
    pub name: String,
    /// the manifest 'name' key
    pub display_name: String,
    pub version: String,
    pub category: String,
    pub summary: String,
    pub depends: Vec<String>,
    /// data files (XML/CSV) in load order
    pub data: Vec<String>,
    /// `assets`: what this addon contributes to each client bundle, in
    /// declaration order (`odoo/addons/base/models/assets.py`). The paths
    /// may be globs, and an entry may carry a directive.
    pub assets: Vec<AssetEntry>,
    pub installable: bool,
    pub auto_install: bool,
    /// addon directory on disk (set by the loader; empty in unit parses)
    pub path: std::path::PathBuf,
}

/// What an addon adds to a bundle: one path (possibly a glob), with the
/// directive saying where it lands. Odoo writes an entry either as a
/// bare path or as `(directive, path[, target])`.
#[derive(Debug, Clone, PartialEq)]
pub struct AssetEntry {
    /// bundle name, e.g. `web.assets_backend`
    pub bundle: String,
    pub directive: AssetDirective,
    /// the file this entry is about (a glob is kept verbatim: expanding
    /// it needs the addon paths, which the loader has and this does not)
    pub path: String,
    /// the existing entry a `before`/`after`/`replace` positions against
    pub target: Option<String>,
}

/// Odoo's asset directives. `append` is the default when an entry is a
/// bare path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetDirective {
    Append,
    Prepend,
    Before,
    After,
    Replace,
    Remove,
    Include,
}

impl AssetDirective {
    fn parse(word: &str) -> Option<AssetDirective> {
        Some(match word {
            "append" => AssetDirective::Append,
            "prepend" => AssetDirective::Prepend,
            "before" => AssetDirective::Before,
            "after" => AssetDirective::After,
            "replace" => AssetDirective::Replace,
            "remove" => AssetDirective::Remove,
            "include" => AssetDirective::Include,
            _ => return None,
        })
    }

    /// Whether the directive positions against an existing entry, which
    /// is what makes a second path meaningful.
    fn needs_target(self) -> bool {
        matches!(
            self,
            AssetDirective::Before | AssetDirective::After | AssetDirective::Replace
        )
    }
}

/// Read the `assets` dict: `{bundle: [entry, ...]}`, where an entry is a
/// path or a `(directive, path[, target])` tuple.
///
/// An entry that cannot be understood is an error rather than a skipped
/// file: a bundle silently missing a script is a client that breaks at
/// runtime, far from here.
fn parse_assets(
    map: &serde_json::Map<String, Value>,
    module: &str,
) -> Result<Vec<AssetEntry>, RusdooError> {
    let bundles = match map.get("assets") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => return Ok(Vec::new()),
        Some(Value::Object(bundles)) => bundles,
        Some(_) => {
            return Err(RusdooError::Validation(format!(
                "manifest {module}: 'assets' must be a dict of bundles"
            )))
        }
    };
    let malformed = |bundle: &str, what: &str| {
        RusdooError::Validation(format!(
            "manifest {module}: asset entry in {bundle:?} {what}"
        ))
    };
    let mut entries = Vec::new();
    for (bundle, items) in bundles {
        let Some(items) = items.as_array() else {
            return Err(malformed(bundle, "list expected"));
        };
        for item in items {
            let entry = match item {
                Value::String(path) => AssetEntry {
                    bundle: bundle.clone(),
                    directive: AssetDirective::Append,
                    path: path.clone(),
                    target: None,
                },
                // a tuple/list: (directive, path) or (directive, target, path)
                Value::Array(parts) => {
                    let words: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
                    if words.len() != parts.len() || words.len() < 2 {
                        return Err(malformed(bundle, "must be strings, at least two"));
                    }
                    let directive = AssetDirective::parse(words[0])
                        .ok_or_else(|| malformed(bundle, "has an unknown directive"))?;
                    if directive.needs_target() {
                        if words.len() != 3 {
                            return Err(malformed(bundle, "needs a target and a path"));
                        }
                        AssetEntry {
                            bundle: bundle.clone(),
                            directive,
                            path: words[2].to_string(),
                            target: Some(words[1].to_string()),
                        }
                    } else {
                        if words.len() != 2 {
                            return Err(malformed(bundle, "takes a single path"));
                        }
                        AssetEntry {
                            bundle: bundle.clone(),
                            directive,
                            path: words[1].to_string(),
                            target: None,
                        }
                    }
                }
                _ => return Err(malformed(bundle, "must be a path or a directive tuple")),
            };
            entries.push(entry);
        }
    }
    Ok(entries)
}

pub fn parse_manifest(source: &str, technical_name: &str) -> Result<Manifest, RusdooError> {
    let value = parse_py_literal(source)
        .map_err(|e| RusdooError::Validation(format!("manifest {technical_name}: {e}")))?;
    let Value::Object(map) = value else {
        return Err(RusdooError::Validation(format!(
            "manifest {technical_name} must be a python dict"
        )));
    };
    let get_str = |key: &str, default: &str| -> Result<String, RusdooError> {
        match map.get(key) {
            None | Some(Value::Null) => Ok(default.to_string()),
            Some(Value::String(text)) => Ok(text.clone()),
            // seen in the wild: 'version': 16.0 as a bare number
            Some(Value::Number(number)) => Ok(number.to_string()),
            Some(_) => Err(RusdooError::Validation(format!(
                "manifest {technical_name}: '{key}' must be a string"
            ))),
        }
    };
    let str_list = |key: &str| -> Result<Vec<String>, RusdooError> {
        match map.get(key) {
            None | Some(Value::Null) | Some(Value::Bool(false)) => Ok(Vec::new()),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| {
                    item.as_str().map(str::to_string).ok_or_else(|| {
                        RusdooError::Validation(format!(
                            "manifest {technical_name}: '{key}' entries must be strings"
                        ))
                    })
                })
                .collect(),
            Some(_) => Err(RusdooError::Validation(format!(
                "manifest {technical_name}: '{key}' must be a list"
            ))),
        }
    };
    Ok(Manifest {
        name: technical_name.to_string(),
        display_name: get_str("name", technical_name)?,
        version: get_str("version", "1.0")?,
        category: get_str("category", "Uncategorized")?,
        summary: get_str("summary", "")?.trim().to_string(),
        depends: str_list("depends")?,
        data: str_list("data")?,
        assets: parse_assets(&map, technical_name)?,
        installable: map
            .get("installable")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        // bool, or a list of modules meaning "install when those are"
        auto_install: match map.get("auto_install") {
            Some(Value::Bool(enabled)) => *enabled,
            Some(Value::Array(_)) => true,
            _ => false,
        },
        path: std::path::PathBuf::new(),
    })
}
