//! Module loading: manifest parsing (Python dict literals), dependency
//! graph, addon discovery. Reference: odoo/modules/module.py, graph.py.

use rusdoo_modules::graph::dependency_order;
use rusdoo_modules::loader::{discover_addons, select};
use rusdoo_modules::manifest::{parse_manifest, Manifest};
use std::path::Path;

fn stub(name: &str, deps: &[&str]) -> Manifest {
    Manifest {
        name: name.into(),
        display_name: name.into(),
        version: "1.0".into(),
        category: "Test".into(),
        summary: String::new(),
        depends: deps.iter().map(|s| s.to_string()).collect(),
        data: vec![],
        assets: vec![],
        installable: true,
        auto_install: false,
        path: std::path::PathBuf::new(),
    }
}

// ---------- manifest parsing ----------

#[test]
fn parses_a_realistic_manifest() {
    let src = r##"
# -*- coding: utf-8 -*-
# Part of Odoo. See LICENSE file for full copyright and licensing details.
{
    'name': 'CRM',
    'version': '1.6',
    'category': 'Sales/CRM',
    'summary': 'Track leads ' 'and close opportunities',
    'description': """
Multi-line
description with 'quotes' and "double quotes"
""",
    'depends': [
        'base',
        'mail',  # chatter
    ],
    'data': ['views/crm_views.xml'],
    'installable': True,
    'auto_install': False,
    'sequence': 15,
    'price': 9.99,
    'external_dependencies': {'python': ['dateutil']},
}
"##;

    let m = parse_manifest(src, "crm").unwrap();

    assert_eq!(m.name, "crm");
    assert_eq!(m.display_name, "CRM");
    assert_eq!(m.version, "1.6");
    assert_eq!(m.depends, vec!["base", "mail"]);
    assert_eq!(m.data, vec!["views/crm_views.xml"]);
    assert!(m.installable);
    assert!(!m.auto_install);
    // adjacent string literals concatenate like in Python
    assert_eq!(m.summary, "Track leads and close opportunities");
}

#[test]
fn defaults_apply_when_keys_missing() {
    let m = parse_manifest("{'name': 'X'}", "x").unwrap();

    assert!(m.installable);
    assert!(!m.auto_install);
    assert!(m.depends.is_empty());
    assert_eq!(m.version, "1.0");
}

#[test]
fn auto_install_module_list_means_true() {
    let m = parse_manifest("{'auto_install': ['sale', 'stock']}", "x").unwrap();

    assert!(m.auto_install);
}

#[test]
fn set_literals_inside_assets_are_accepted() {
    // real manifests use python sets: 'assets': {'x': {'path/**/*',},}
    let src = "{'name': 'X', 'assets': {'web.assets_backend': {'a/static/**/*',},},}";

    assert!(parse_manifest(src, "x").is_ok());
}

#[test]
fn rejects_non_dict_manifest() {
    assert!(parse_manifest("['not', 'a', 'dict']", "x").is_err());
}

#[test]
fn rejects_unterminated_string() {
    assert!(parse_manifest("{'name': 'broken", "x").is_err());
}

// ---------- dependency graph ----------

#[test]
fn dependency_order_respects_depends() {
    let mods = vec![
        stub("mail", &["base"]),
        stub("base", &[]),
        stub("crm", &["mail", "base"]),
    ];

    let order = dependency_order(&mods).unwrap();

    let pos = |n: &str| order.iter().position(|x| x == n).unwrap();
    assert!(pos("base") < pos("mail"));
    assert!(pos("mail") < pos("crm"));
}

#[test]
fn dependency_cycle_is_rejected() {
    let mods = vec![stub("a", &["b"]), stub("b", &["a"])];

    assert!(dependency_order(&mods).is_err());
}

#[test]
fn missing_dependency_is_reported_by_name() {
    let mods = vec![stub("a", &["ghost"])];

    let err = dependency_order(&mods).unwrap_err();

    assert!(err.to_string().contains("ghost"));
}

// ---------- the install set (Odoo's `-i`) ----------

#[test]
fn an_install_set_brings_what_it_depends_on() {
    let mods = vec![
        stub("base", &[]),
        stub("crm", &["mail"]),
        stub("mail", &["base"]),
        stub("uom", &["base"]),
    ];

    let selected = select(mods, &["crm".to_string()]).unwrap();

    let names: Vec<&str> = selected.iter().map(|m| m.name.as_str()).collect();
    assert!(names.contains(&"crm") && names.contains(&"mail") && names.contains(&"base"));
    // a module nobody asked for stays on disk, uninstalled — which is
    // what keeps its assets out of the client's bundle
    assert!(!names.contains(&"uom"), "{names:?}");
}

#[test]
fn base_is_installed_whether_it_is_named_or_not() {
    // Odoo has no database without base: it is what res.users is in
    let mods = vec![stub("base", &[]), stub("loner", &[])];

    let selected = select(mods, &["loner".to_string()]).unwrap();

    let names: Vec<&str> = selected.iter().map(|m| m.name.as_str()).collect();
    assert!(names.contains(&"base"), "{names:?}");
}

#[test]
fn an_unknown_name_in_the_install_set_is_refused() {
    let mods = vec![stub("base", &[])];

    let err = select(mods, &["ghost".to_string()]).unwrap_err();

    assert!(err.to_string().contains("ghost"), "{err}");
}

// ---------- the real thing: all Odoo 19 addons ----------

#[test]
fn discovers_and_orders_the_real_odoo_addons() {
    let base_dir = Path::new("../../odoo/odoo/addons");
    let community = Path::new("../../odoo/addons");
    if !community.exists() {
        eprintln!("skipped: reference clone not present");
        return;
    }

    let manifests = discover_addons(&[base_dir, community]).unwrap();
    assert!(
        manifests.len() >= 600,
        "expected the full addon set, got {}",
        manifests.len()
    );

    let order = dependency_order(&manifests).unwrap();
    assert_eq!(order.len(), manifests.len());

    let pos = |n: &str| {
        order
            .iter()
            .position(|x| x == n)
            .unwrap_or_else(|| panic!("{n} missing from order"))
    };
    assert_eq!(pos("base"), 0, "base loads first");
    assert!(pos("web") < pos("mail"));
    assert!(pos("mail") < pos("crm"));

    // global validity: every module loads after all its dependencies
    for manifest in &manifests {
        for dep in &manifest.depends {
            assert!(
                pos(dep) < pos(&manifest.name),
                "{} must load after its dependency {dep}",
                manifest.name
            );
        }
    }
}

#[test]
fn absurd_nesting_errors_instead_of_crashing() {
    let bomb = format!("{{'depends': {}}}", "[".repeat(100_000));

    assert!(parse_manifest(&bomb, "x").is_err());
}

#[test]
fn unicode_escapes_are_decoded() {
    let m = parse_manifest(r"{'name': '\u00e9cole \x21'}", "x").unwrap();

    assert_eq!(m.display_name, "\u{e9}cole !");
}

#[test]
fn numeric_version_is_coerced_but_wrong_types_error() {
    assert_eq!(
        parse_manifest("{'version': 16.5}", "x").unwrap().version,
        "16.5"
    );
    assert!(parse_manifest("{'name': ['not', 'a', 'string']}", "x").is_err());
}

#[test]
fn duplicate_module_names_are_reported() {
    let mods = vec![stub("base", &[]), stub("foo", &["base"]), stub("foo", &[])];

    let err = dependency_order(&mods).unwrap_err();

    assert!(err.to_string().contains("duplicate module name: foo"));
}

#[test]
fn parses_asset_bundles_from_a_manifest() {
    use rusdoo_modules::manifest::AssetDirective;

    let manifest = parse_manifest(
        r#"{
            'name': 'Demo',
            'assets': {
                'web.assets_backend': [
                    'demo/static/src/main.js',
                    ('prepend', 'demo/static/src/first.js'),
                    ('after', 'web/static/src/core/utils.js', 'demo/static/src/late.js'),
                    ('remove', 'web/static/src/unwanted.js'),
                    'demo/static/src/**/*.scss',
                ],
                'web.assets_frontend': ['demo/static/src/public.js'],
            },
        }"#,
        "demo",
    )
    .unwrap();

    assert_eq!(manifest.assets.len(), 6);
    let backend: Vec<_> = manifest
        .assets
        .iter()
        .filter(|a| a.bundle == "web.assets_backend")
        .collect();
    // a bare path appends, in declaration order
    assert_eq!(backend[0].directive, AssetDirective::Append);
    assert_eq!(backend[0].path, "demo/static/src/main.js");
    assert_eq!(backend[1].directive, AssetDirective::Prepend);
    // a positioning directive keeps what it positions against
    assert_eq!(backend[2].directive, AssetDirective::After);
    assert_eq!(
        backend[2].target.as_deref(),
        Some("web/static/src/core/utils.js")
    );
    assert_eq!(backend[2].path, "demo/static/src/late.js");
    assert_eq!(backend[3].directive, AssetDirective::Remove);
    // a glob is kept verbatim: expanding it needs the addon paths
    assert_eq!(backend[4].path, "demo/static/src/**/*.scss");
    assert_eq!(manifest.assets[5].bundle, "web.assets_frontend");
}

#[test]
fn malformed_asset_entries_are_refused() {
    for (source, why) in [
        (
            r#"{'assets': ['not', 'a', 'dict']}"#,
            "assets must be a dict",
        ),
        (r#"{'assets': {'b': 'not a list'}}"#, "a bundle is a list"),
        (
            r#"{'assets': {'b': [('nope', 'x.js')]}}"#,
            "unknown directive",
        ),
        (
            r#"{'assets': {'b': [('after', 'x.js')]}}"#,
            "after needs a target",
        ),
        (
            r#"{'assets': {'b': [('append', 'a.js', 'b.js')]}}"#,
            "append takes one path",
        ),
        (
            r#"{'assets': {'b': [42]}}"#,
            "an entry is a path or a tuple",
        ),
    ] {
        assert!(
            parse_manifest(source, "demo").is_err(),
            "{why}: {source} must be refused"
        );
    }
}

/// A permanent measurement, like the data-file one: how much of the real
/// Odoo asset declaration this parser understands.
#[test]
fn measure_real_odoo_asset_coverage() {
    let roots = [
        Path::new("../../odoo/addons"),
        Path::new("../../odoo/odoo/addons"),
    ];
    if !roots[0].exists() {
        eprintln!("skipped: reference clone not present");
        return;
    }
    let (mut addons, mut with_assets, mut entries, mut failed) = (0, 0, 0, Vec::new());
    for root in roots {
        let Ok(dir) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in dir.flatten() {
            let manifest_path = entry.path().join("__manifest__.py");
            let Ok(source) = std::fs::read_to_string(&manifest_path) else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().to_string();
            addons += 1;
            match parse_manifest(&source, &name) {
                Ok(manifest) => {
                    if !manifest.assets.is_empty() {
                        with_assets += 1;
                        entries += manifest.assets.len();
                    }
                }
                Err(error) => failed.push(format!("{name}: {error}")),
            }
        }
    }
    eprintln!(
        "asset coverage: {addons} addons parsed, {with_assets} declare assets, \
         {entries} entries; {} failures",
        failed.len()
    );
    for failure in failed.iter().take(5) {
        eprintln!("  {failure}");
    }
    assert!(
        failed.is_empty(),
        "{} manifest(s) no longer parse",
        failed.len()
    );
    assert!(
        with_assets > 100,
        "expected the real addons to declare assets, got {with_assets}"
    );
}

/// The first file of the real backend bundle, which decides whether the
/// client starts at all: every other file opens with `odoo.define(...)`,
/// so anything ahead of `module_loader.js` is a TypeError that takes the
/// whole bundle down.
///
/// It is the install set that puts it there, not luck: 141 of the 143
/// addons contributing to `web.assets_backend` depend on `web`, so the
/// order of `ir.asset` places web's own files first. The two that do not
/// — `uom` and `test_translation_import` — sort ahead of `web` by name,
/// and a server that installs every addon on disk really does serve
/// them first.
#[test]
fn a_web_install_serves_the_module_loader_first() {
    let roots = [
        Path::new("../../odoo/odoo/addons"),
        Path::new("../../odoo/addons"),
    ];
    if !roots[1].exists() {
        eprintln!("skipped: reference clone not present");
        return;
    }
    let manifests = discover_addons(&roots).unwrap();
    let selected = select(manifests, &["web".to_string()]).unwrap();
    let (bundles, _) = rusdoo_modules::assets::resolve_manifests(&selected).unwrap();

    let first = bundles
        .files_with_extension("web.assets_web", &["js", "mjs"])
        .next()
        .expect("the backend bundle ships JavaScript");
    assert_eq!(first.path, "web/static/src/module_loader.js");
}

/// Every ES6 module the real Odoo tree ships, put through the
/// transpiler.
///
/// The unit tests say it matches Odoo on the cases Odoo wrote tests for.
/// This says it survives the corpus those tests are a sample of — which
/// is the thing that actually decides whether an addon's JavaScript
/// loads, because a single file that fails to transpile is a syntax
/// error in a bundle and takes down every file after it.
///
/// A smoke test and not a proof: it checks that the transform completes
/// and that no bare `import`/`export` statement survived into the
/// output. Whether the result *behaves* is what the browser decides.
#[test]
fn measure_real_odoo_js_transpile_coverage() {
    let root = Path::new("../../odoo/addons");
    if !root.exists() {
        eprintln!("skipped: reference clone not present");
        return;
    }
    let (mut transpiled, mut failed, mut leftovers) = (0usize, Vec::new(), Vec::new());
    // how many came out naming something they need. A transpiler that
    // silently collected nothing would still pass every other assertion
    // here — and every module would then load before its dependencies,
    // which is a race nobody could debug from the browser.
    let mut with_dependencies = 0usize;
    let mut stack: Vec<std::path::PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("js") {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let url = format!("/{}", relative.display());
            if !rusdoo_modules::js_module::is_odoo_module(&url, &content) {
                continue;
            }
            match rusdoo_modules::js_module::transpile(&url, &content) {
                Ok(out) => {
                    transpiled += 1;
                    if !out.starts_with(&format!("odoo.define('{}', [],", 
                        rusdoo_modules::js_module::url_to_module_path(&url).unwrap_or_default()))
                    {
                        with_dependencies += 1;
                    }
                    // a statement the client's loader has no idea what to
                    // do with is the same as a failure, one step later
                    if out.lines().any(|line| {
                        let line = line.trim_start();
                        (line.starts_with("import ") && !line.contains('('))
                            || line.starts_with("export ")
                    }) {
                        leftovers.push(url.clone());
                    }
                }
                Err(error) => failed.push(format!("{url}: {error}")),
            }
        }
    }
    eprintln!("transpiled {transpiled} real Odoo module(s)");
    assert!(
        transpiled > 3000,
        "the corpus should be thousands of files, found {transpiled} — \
         is the clone complete?"
    );
    assert!(
        failed.is_empty(),
        "{} file(s) did not transpile, first few: {:?}",
        failed.len(),
        &failed[..failed.len().min(5)]
    );
    assert!(
        with_dependencies * 2 > transpiled,
        "only {with_dependencies} of {transpiled} modules named a dependency — \
         the requires are not being collected"
    );
    assert!(
        leftovers.is_empty(),
        "{} file(s) still carry ES6 the loader cannot read, first few: {:?}",
        leftovers.len(),
        &leftovers[..leftovers.len().min(5)]
    );
}
