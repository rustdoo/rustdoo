//! The list of modules, as rows: `ir.module.module` kept in step with
//! what is on disk and what is running.
//!
//! Odoo's Apps screen reads a table, not a directory, and that is the
//! right shape: a screen cannot page, search or filter a filesystem, and
//! a person needs to see the module that is *not* installed as much as
//! the one that is.
//!
//! Two sources of truth meet here, and neither is allowed to overwrite
//! the other:
//!
//! * **the disk** says what exists — every manifest found becomes a row,
//!   and a row whose addon is gone is dropped;
//! * **the database** says what is running, and what somebody asked for.
//!   A module marked `to install` keeps that mark until a boot honours
//!   it; a module that is `installed` is never quietly demoted because
//!   this particular boot did not load it.
//!
//! That last rule is the one that matters. A server started with a
//! narrower `RUSDOO_INSTALL` than last time would otherwise rewrite the
//! history of the database to match one command line.

use crate::manifest::Manifest;
use rusdoo_core::RusdooError;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::registry::Registry;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::HashSet;

/// Read the modules marked `to install`, so a boot can honour them.
///
/// Raw SQL and not the ORM, because this runs *before* there is a
/// registry to ask: which models get registered is exactly the question
/// this answer decides. A database with no catalogue yet — a first boot —
/// wants nothing, which is not an error.
pub async fn wanted_modules(pool: &PgPool) -> Vec<String> {
    let known: bool = sqlx::query_scalar("SELECT to_regclass('ir_module_module') IS NOT NULL")
        .fetch_one(pool)
        .await
        .unwrap_or(false);
    if !known {
        return Vec::new();
    }
    sqlx::query_scalar(r#"SELECT "name" FROM "ir_module_module" WHERE "state" = 'to install'"#)
        .fetch_all(pool)
        .await
        .unwrap_or_default()
}

/// Rewrite the catalogue from the manifests on disk, and record which of
/// them this boot installed.
///
/// `on_disk` is everything found, installed or not — the screen exists to
/// show the difference. `installed` is what this boot actually loaded,
/// and `with_code` is what this *binary* can run: an addon can be on
/// disk, with a manifest and data files, and have no models compiled in,
/// which is the honest thing for a screen to say before somebody clicks.
pub async fn sync(
    registry: &Registry,
    pool: &PgPool,
    on_disk: &[Manifest],
    installed: &HashSet<String>,
    with_code: &HashSet<String>,
) -> Result<(), RusdooError> {
    if registry.get("ir.module.module").is_none() {
        return Ok(());
    }
    let existing = registry
        .search(
            pool,
            "ir.module.module",
            &parse_domain(&json!([]))?,
            &SearchOptions::default(),
        )
        .await?;
    // a first boot has no rows at all, and asking the ORM to read none
    // is an error rather than an empty answer
    let rows = if existing.is_empty() {
        Vec::new()
    } else {
        registry
            .read(pool, "ir.module.module", &existing, &["name", "state"])
            .await?
    };
    let known: Vec<(i64, String, String)> = rows
        .iter()
        .filter_map(|row| {
            Some((
                row.get("id")?.as_i64()?,
                row.get("name")?.as_str()?.to_string(),
                row.get("state")?.as_str().unwrap_or("uninstalled").to_string(),
            ))
        })
        .collect();

    for manifest in on_disk {
        let state = state_for(manifest, installed, &known);
        let values = values_of(manifest, with_code, &state);
        match known.iter().find(|(_, name, _)| name == &manifest.name) {
            Some((id, _, _)) => {
                registry.write(pool, "ir.module.module", &[*id], values).await?;
                sync_dependencies(registry, pool, *id, manifest).await?;
            }
            None => {
                let id = registry.create(pool, "ir.module.module", values).await?;
                sync_dependencies(registry, pool, id, manifest).await?;
            }
        }
    }

    // a row whose addon is no longer on disk is a module nobody can
    // install or read about: it goes, and its dependencies with it
    let present: HashSet<&str> = on_disk.iter().map(|m| m.name.as_str()).collect();
    let gone: Vec<i64> = known
        .iter()
        .filter(|(_, name, _)| !present.contains(name.as_str()))
        .map(|(id, _, _)| *id)
        .collect();
    if !gone.is_empty() {
        registry.unlink_as(pool, rusdoo_core::SUPERUSER_ID, "ir.module.module", &gone).await?;
    }
    Ok(())
}

/// What a module's state becomes after this boot.
///
/// Installed wins over everything: a module this boot did not load is
/// still installed in this database, and its tables and its data are
/// still there. That is why a narrower `RUSDOO_INSTALL` cannot rewrite
/// history — it changes what is *running*, not what the database has.
fn state_for(
    manifest: &Manifest,
    installed: &HashSet<String>,
    known: &[(i64, String, String)],
) -> String {
    if installed.contains(&manifest.name) {
        return "installed".to_string();
    }
    match known.iter().find(|(_, name, _)| name == &manifest.name) {
        // `to install` survives a boot that could not honour it yet, and
        // `installed` survives a boot that did not load it
        Some((_, _, state)) if state == "installed" || state == "to install" => state.clone(),
        _ => "uninstalled".to_string(),
    }
}

fn values_of<'a>(
    manifest: &'a Manifest,
    with_code: &HashSet<String>,
    state: &str,
) -> Vec<(&'a str, Value)> {
    vec![
        ("name", json!(manifest.name)),
        ("shortdesc", json!(manifest.display_name)),
        ("state", json!(state)),
        ("latest_version", json!(manifest.version)),
        ("category", json!(manifest.category)),
        ("summary", json!(manifest.summary)),
        ("installable", json!(manifest.installable)),
        ("auto_install", json!(manifest.auto_install)),
        ("has_code", json!(with_code.contains(&manifest.name))),
        // Odoo's `application` flag is a manifest key; this port reads
        // the same idea off the category the manifest declares, until the
        // key itself is parsed
        ("application", json!(!manifest.category.is_empty()
            && !manifest.category.eq_ignore_ascii_case("hidden")
            && !manifest.category.eq_ignore_ascii_case("oculto"))),
    ]
}

/// The dependency rows of one module, rewritten from its manifest.
async fn sync_dependencies(
    registry: &Registry,
    pool: &PgPool,
    module: i64,
    manifest: &Manifest,
) -> Result<(), RusdooError> {
    if registry.get("ir.module.module.dependency").is_none() {
        return Ok(());
    }
    let mine = registry
        .search(
            pool,
            "ir.module.module.dependency",
            &parse_domain(&json!([["module_id", "=", module]]))?,
            &SearchOptions::default(),
        )
        .await?;
    if !mine.is_empty() {
        registry
            .unlink_as(
                pool,
                rusdoo_core::SUPERUSER_ID,
                "ir.module.module.dependency",
                &mine,
            )
            .await?;
    }
    for depends in &manifest.depends {
        registry
            .create(
                pool,
                "ir.module.module.dependency",
                vec![("name", json!(depends)), ("module_id", json!(module))],
            )
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(name: &str, category: &str) -> Manifest {
        Manifest {
            name: name.into(),
            display_name: name.into(),
            version: "19.0.1.0".into(),
            category: category.into(),
            summary: String::new(),
            depends: vec!["base".into()],
            data: Vec::new(),
            assets: Vec::new(),
            installable: true,
            auto_install: false,
            path: Default::default(),
        }
    }

    #[test]
    fn a_boot_that_did_not_load_a_module_does_not_uninstall_it() {
        let known = vec![(1, "sale".to_string(), "installed".to_string())];
        let loaded = HashSet::new();
        assert_eq!(state_for(&manifest("sale", "Vendas"), &loaded, &known), "installed");
    }

    #[test]
    fn a_request_to_install_survives_until_a_boot_honours_it() {
        let known = vec![(1, "crm".to_string(), "to install".to_string())];
        let loaded = HashSet::new();
        assert_eq!(state_for(&manifest("crm", "Vendas"), &loaded, &known), "to install");
        // and the boot that loads it settles the matter
        let loaded: HashSet<String> = ["crm".to_string()].into_iter().collect();
        assert_eq!(state_for(&manifest("crm", "Vendas"), &loaded, &known), "installed");
    }

    #[test]
    fn a_module_nobody_has_touched_is_not_installed() {
        assert_eq!(
            state_for(&manifest("fleet", "Frota"), &HashSet::new(), &[]),
            "uninstalled"
        );
    }

    #[test]
    fn a_technical_module_is_not_an_application() {
        let with_code = HashSet::new();
        let uom = manifest("uom", "Oculto");
        let sale = manifest("sale", "Vendas");
        let hidden = values_of(&uom, &with_code, "installed");
        let app = values_of(&sale, &with_code, "installed");
        let flag = |values: &[(&str, Value)]| {
            values
                .iter()
                .find(|(key, _)| *key == "application")
                .map(|(_, value)| value.clone())
                .unwrap()
        };
        assert_eq!(flag(&hidden), json!(false));
        assert_eq!(flag(&app), json!(true));
    }
}
