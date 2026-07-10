//! Addon discovery over the configured addons paths, port of the
//! module scanning in `odoo/modules/module.py`.

use crate::manifest::{parse_manifest, Manifest};
use rusdoo_core::RusdooError;
use std::collections::HashSet;
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
