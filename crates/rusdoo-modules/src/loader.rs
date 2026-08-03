//! Addon discovery over the configured addons paths, port of the
//! module scanning in `odoo/modules/module.py`.

use crate::manifest::{parse_manifest, Manifest};
use rusdoo_core::RusdooError;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

/// Scan each path for directories holding a `__manifest__.py`. When the
/// same addon appears in several paths, the FIRST path wins (like
/// Odoo's addons_path precedence).
pub fn discover_addons(paths: &[&Path]) -> Result<Vec<Manifest>, RusdooError> {
    let mut manifests: Vec<Manifest> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for base in paths {
        let entries = fs::read_dir(base).map_err(|e| {
            RusdooError::Validation(format!("cannot read addons path {}: {e}", base.display()))
        })?;
        for entry in entries {
            let entry = entry.map_err(|e| {
                RusdooError::Validation(format!("cannot read addons path entry: {e}"))
            })?;
            let dir = entry.path();
            let manifest_path = dir.join("__manifest__.py");
            if !manifest_path.is_file() {
                continue;
            }
            let Some(technical_name) = dir.file_name().and_then(|n| n.to_str()) else {
                tracing::warn!(
                    "skipping addon with non-UTF8 directory name: {}",
                    dir.display()
                );
                continue;
            };
            let technical_name = technical_name.to_string();
            if !seen.insert(technical_name.clone()) {
                continue;
            }
            let source = fs::read_to_string(&manifest_path).map_err(|e| {
                RusdooError::Validation(format!("cannot read {}: {e}", manifest_path.display()))
            })?;
            let mut manifest = parse_manifest(&source, &technical_name)?;
            manifest.path = dir.clone();
            manifests.push(manifest);
        }
    }
    manifests.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(manifests)
}

/// The addons a server runs, out of everything found on disk: the named
/// modules plus every module they depend on, transitively — Odoo's `-i`.
///
/// Being on disk is not being installed. Odoo installs a chosen set, and
/// what that set leaves out is absent from the client's bundles, from the
/// schema and from the menus. A server that installs all 652 addons of
/// the reference tree is not a faithful one, and it does not even boot a
/// browser: `uom` contributes ES6 to `web.assets_backend` without
/// depending on `web`, so it lands ahead of the module loader.
///
/// An unknown name is an error rather than a warning: a typo in the list
/// is otherwise a module that is mysteriously not there.
pub fn select(manifests: Vec<Manifest>, wanted: &[String]) -> Result<Vec<Manifest>, RusdooError> {
    let by_name: HashMap<&str, &Manifest> =
        manifests.iter().map(|m| (m.name.as_str(), m)).collect();
    // base is not optional: it is where res.users lives, so a database
    // without it has nobody to log in as. Odoo installs it too, named or
    // not, whenever it is on disk.
    let mut queue: Vec<&str> = wanted.iter().map(String::as_str).collect();
    if by_name.contains_key("base") {
        queue.push("base");
    }
    let mut keep: HashSet<&str> = HashSet::new();
    while let Some(name) = queue.pop() {
        let Some(manifest) = by_name.get(name) else {
            return Err(RusdooError::Validation(format!(
                "module {name} is in the install set but not on any addons path"
            )));
        };
        if !keep.insert(manifest.name.as_str()) {
            continue;
        }
        for dep in &manifest.depends {
            queue.push(dep.as_str());
        }
    }
    let keep: HashSet<String> = keep.into_iter().map(str::to_string).collect();
    Ok(manifests
        .into_iter()
        .filter(|manifest| keep.contains(&manifest.name))
        .collect())
}
