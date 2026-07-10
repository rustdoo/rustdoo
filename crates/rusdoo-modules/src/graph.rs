//! Port of `odoo/modules/graph.py`: deterministic topological order of
//! addons by their `depends`.

use crate::manifest::Manifest;
use rusdoo_core::RusdooError;
use std::collections::{BTreeSet, HashMap};

pub fn dependency_order(manifests: &[Manifest]) -> Result<Vec<String>, RusdooError> {
    let mut known: HashMap<&str, &Manifest> = HashMap::new();
    for manifest in manifests {
        if known.insert(manifest.name.as_str(), manifest).is_some() {
            return Err(RusdooError::Validation(format!(
                "duplicate module name: {}",
                manifest.name
            )));
        }
    }
    for manifest in manifests {
        for dep in &manifest.depends {
            if !known.contains_key(dep.as_str()) {
                return Err(RusdooError::Validation(format!(
                    "module {} depends on unknown module {dep}",
                    manifest.name
                )));
            }
        }
    }

    let mut indegree: HashMap<&str, usize> = HashMap::new();
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for manifest in manifests {
        indegree.entry(manifest.name.as_str()).or_insert(0);
        for dep in &manifest.depends {
            *indegree.entry(manifest.name.as_str()).or_insert(0) += 1;
            dependents
                .entry(dep.as_str())
                .or_default()
                .push(manifest.name.as_str());
        }
    }

    let mut ready: BTreeSet<&str> = indegree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(name, _)| *name)
        .collect();
    let mut order = Vec::with_capacity(manifests.len());
    while !ready.is_empty() {
        // deterministic: base (the framework) first, then alphabetical
        let next = if ready.contains("base") {
            "base"
        } else {
            ready.iter().next().copied().expect("checked non-empty")
        };
        ready.remove(next);
        order.push(next.to_string());
        for dependent in dependents.remove(next).unwrap_or_default() {
            let degree = indegree.get_mut(dependent).expect("all modules counted");
            *degree -= 1;
            if *degree == 0 {
                ready.insert(dependent);
            }
        }
    }

    if order.len() != manifests.len() {
        let stuck: Vec<&str> = indegree
            .iter()
            .filter(|(_, degree)| **degree > 0)
            .map(|(name, _)| *name)
            .collect();
        return Err(RusdooError::Validation(format!(
            "dependency cycle among modules: {stuck:?}"
        )));
    }
    Ok(order)
}
