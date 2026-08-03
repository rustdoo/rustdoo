//! Bundle resolution: the manifest `assets` of several addons, applied
//! in dependency order, become the ordered file list a client loads.

use rusdoo_modules::assets::resolve_bundles;
use rusdoo_modules::manifest::{parse_manifest, Manifest};
use std::path::{Path, PathBuf};

/// A throwaway addons tree. Dropped with the test, so a failing run
/// leaves nothing behind for the next one to trip over.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let root = std::env::temp_dir().join(format!("rusdoo-assets-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("fixture root");
        Fixture { root }
    }

    /// Write an addon: its manifest source and the files it ships.
    fn addon(&self, name: &str, manifest: &str, files: &[&str]) -> Manifest {
        let dir = self.root.join(name);
        std::fs::create_dir_all(&dir).expect("addon dir");
        std::fs::write(dir.join("__manifest__.py"), manifest).expect("manifest");
        for file in files {
            let path = dir.join(file);
            std::fs::create_dir_all(path.parent().expect("file has a parent")).expect("subdir");
            std::fs::write(&path, format!("/* {name}/{file} */\n")).expect("asset file");
        }
        let mut parsed = parse_manifest(manifest, name).expect("manifest parses");
        parsed.path = dir;
        parsed
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn paths(files: &[rusdoo_modules::assets::AssetFile]) -> Vec<&str> {
    files.iter().map(|file| file.path.as_str()).collect()
}

#[test]
fn a_glob_expands_in_sorted_order() {
    let fixture = Fixture::new("glob");
    let web = fixture.addon(
        "web",
        r#"{'name': 'Web', 'assets': {'web.assets_backend': ['web/static/src/**/*.js']}}"#,
        &[
            "static/src/b.js",
            "static/src/a.js",
            "static/src/core/z.js",
            "static/src/skip.css",
        ],
    );
    let bundles = resolve_bundles(&[&web]).expect("resolves");
    assert_eq!(
        paths(bundles.files("web.assets_backend")),
        vec![
            "web/static/src/a.js",
            "web/static/src/b.js",
            "web/static/src/core/z.js",
        ]
    );
}

#[test]
fn modules_contribute_in_dependency_order() {
    let fixture = Fixture::new("order");
    let web = fixture.addon(
        "web",
        r#"{'name': 'Web', 'assets': {'web.assets_backend': ['web/static/src/boot.js']}}"#,
        &["static/src/boot.js"],
    );
    let sale = fixture.addon(
        "sale",
        r#"{'name': 'Sale', 'depends': ['web'],
            'assets': {'web.assets_backend': ['sale/static/src/sale.js']}}"#,
        &["static/src/sale.js"],
    );
    let bundles = resolve_bundles(&[&web, &sale]).expect("resolves");
    assert_eq!(
        paths(bundles.files("web.assets_backend")),
        vec!["web/static/src/boot.js", "sale/static/src/sale.js"]
    );
}

#[test]
fn directives_position_prepend_before_after_replace_remove() {
    let fixture = Fixture::new("directives");
    let web = fixture.addon(
        "web",
        r#"{'name': 'Web', 'assets': {'web.assets_backend': [
               'web/static/src/one.js', 'web/static/src/two.js']}}"#,
        &["static/src/one.js", "static/src/two.js"],
    );
    let patch = fixture.addon(
        "patch",
        r#"{'name': 'Patch', 'depends': ['web'], 'assets': {'web.assets_backend': [
               ('prepend', 'patch/static/src/first.js'),
               ('before', 'web/static/src/two.js', 'patch/static/src/pre_two.js'),
               ('after', 'web/static/src/two.js', 'patch/static/src/post_two.js'),
               ('replace', 'web/static/src/one.js', 'patch/static/src/instead.js'),
           ]}}"#,
        &[
            "static/src/first.js",
            "static/src/pre_two.js",
            "static/src/post_two.js",
            "static/src/instead.js",
        ],
    );
    let bundles = resolve_bundles(&[&web, &patch]).expect("resolves");
    assert_eq!(
        paths(bundles.files("web.assets_backend")),
        vec![
            "patch/static/src/first.js",
            "patch/static/src/instead.js",
            "patch/static/src/pre_two.js",
            "web/static/src/two.js",
            "patch/static/src/post_two.js",
        ]
    );

    // and a later module can take a file back out
    let strip = fixture.addon(
        "strip",
        r#"{'name': 'Strip', 'depends': ['patch'], 'assets': {'web.assets_backend': [
               ('remove', 'web/static/src/two.js')]}}"#,
        &[],
    );
    let bundles = resolve_bundles(&[&web, &patch, &strip]).expect("resolves");
    assert!(!paths(bundles.files("web.assets_backend")).contains(&"web/static/src/two.js"));
}

#[test]
fn include_pulls_in_another_bundle_inline() {
    let fixture = Fixture::new("include");
    let web = fixture.addon(
        "web",
        r#"{'name': 'Web', 'assets': {
               'web.assets_common': ['web/static/src/common.js'],
               'web.assets_backend': [
                   ('include', 'web.assets_common'),
                   'web/static/src/backend.js']}}"#,
        &["static/src/common.js", "static/src/backend.js"],
    );
    let bundles = resolve_bundles(&[&web]).expect("resolves");
    assert_eq!(
        paths(bundles.files("web.assets_backend")),
        vec!["web/static/src/common.js", "web/static/src/backend.js"]
    );
}

#[test]
fn an_include_cycle_is_an_error_not_a_hang() {
    let fixture = Fixture::new("cycle");
    let web = fixture.addon(
        "web",
        r#"{'name': 'Web', 'assets': {
               'a': [('include', 'b')],
               'b': [('include', 'a')]}}"#,
        &[],
    );
    let error = resolve_bundles(&[&web]).expect_err("cycle rejected");
    assert!(
        error.to_string().contains("cycle"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_file_lands_once_however_many_times_it_is_declared() {
    let fixture = Fixture::new("dedup");
    let web = fixture.addon(
        "web",
        r#"{'name': 'Web', 'assets': {'web.assets_backend': [
               'web/static/src/one.js',
               'web/static/src/**/*.js']}}"#,
        &["static/src/one.js", "static/src/two.js"],
    );
    let bundles = resolve_bundles(&[&web]).expect("resolves");
    assert_eq!(
        paths(bundles.files("web.assets_backend")),
        vec!["web/static/src/one.js", "web/static/src/two.js"]
    );
}

/// A path naming no file is skipped, not refused.
///
/// The stricter rule was here first and it was wrong — not as a matter
/// of taste, but because Odoo does the other thing (`IrAsset._get_paths`
/// logs a warning and returns nothing) and Odoo's own `web` manifest
/// names a file its tree does not ship. Refusing it means being unable
/// to serve the addon this port exists to be compatible with, over a
/// typo in somebody else's manifest.
#[test]
fn a_literal_path_that_does_not_exist_is_skipped() {
    let fixture = Fixture::new("missing");
    let web = fixture.addon(
        "web",
        r#"{'name': 'Web', 'assets': {'web.assets_backend': [
            'web/static/src/typo.js', 'web/static/src/right.js']}}"#,
        &["static/src/right.js"],
    );
    let bundles = resolve_bundles(&[&web]).expect("the bundle still resolves");
    assert_eq!(
        paths(bundles.files("web.assets_backend")),
        vec!["web/static/src/right.js"],
        "the file that exists is served, the one that does not is dropped"
    );
}

#[test]
fn a_glob_matching_nothing_is_allowed() {
    let fixture = Fixture::new("empty-glob");
    let web = fixture.addon(
        "web",
        r#"{'name': 'Web', 'assets': {'web.assets_backend': ['web/static/tests/**/*.js']}}"#,
        &["static/src/one.js"],
    );
    let bundles = resolve_bundles(&[&web]).expect("resolves");
    assert!(bundles.files("web.assets_backend").is_empty());
}

#[test]
fn a_path_may_not_escape_its_module() {
    let fixture = Fixture::new("traversal");
    let web = fixture.addon(
        "web",
        r#"{'name': 'Web', 'assets': {'web.assets_backend': ['web/../../etc/passwd']}}"#,
        &["static/src/one.js"],
    );
    let error = resolve_bundles(&[&web]).expect_err("traversal rejected");
    assert!(
        error.to_string().contains(".."),
        "unexpected error: {error}"
    );
}

#[test]
fn a_path_in_an_uninstalled_module_is_an_error() {
    let fixture = Fixture::new("uninstalled");
    let web = fixture.addon(
        "web",
        r#"{'name': 'Web', 'assets': {'web.assets_backend': ['nowhere/static/src/x.js']}}"#,
        &["static/src/one.js"],
    );
    let error = resolve_bundles(&[&web]).expect_err("unknown module rejected");
    assert!(
        error.to_string().contains("not installed"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_directive_targeting_an_absent_file_is_an_error() {
    let fixture = Fixture::new("bad-target");
    let web = fixture.addon(
        "web",
        r#"{'name': 'Web', 'assets': {'web.assets_backend': ['web/static/src/one.js']}}"#,
        &["static/src/one.js", "static/src/two.js"],
    );
    let patch = fixture.addon(
        "patch",
        r#"{'name': 'Patch', 'depends': ['web'], 'assets': {'web.assets_backend': [
               ('after', 'web/static/src/two.js', 'patch/static/src/late.js')]}}"#,
        &["static/src/late.js"],
    );
    let error = resolve_bundles(&[&web, &patch]).expect_err("bad target rejected");
    assert!(
        error.to_string().contains("not in the bundle"),
        "unexpected error: {error}"
    );
}

#[test]
fn extensions_split_a_bundle_into_its_js_and_css_halves() {
    let fixture = Fixture::new("extensions");
    let web = fixture.addon(
        "web",
        r#"{'name': 'Web', 'assets': {'web.assets_backend': ['web/static/src/**/*']}}"#,
        &[
            "static/src/app.js",
            "static/src/app.css",
            "static/src/tpl.xml",
        ],
    );
    let bundles = resolve_bundles(&[&web]).expect("resolves");
    let js: Vec<&str> = bundles
        .files_with_extension("web.assets_backend", &["js"])
        .map(|file| file.path.as_str())
        .collect();
    assert_eq!(js, vec!["web/static/src/app.js"]);
    let css: Vec<&str> = bundles
        .files_with_extension("web.assets_backend", &["css", "scss"])
        .map(|file| file.path.as_str())
        .collect();
    assert_eq!(css, vec!["web/static/src/app.css"]);
}

#[test]
fn resolved_files_point_at_real_files_on_disk() {
    let fixture = Fixture::new("disk");
    let web = fixture.addon(
        "web",
        r#"{'name': 'Web', 'assets': {'web.assets_backend': ['web/static/src/**/*.js']}}"#,
        &["static/src/deep/nested.js"],
    );
    let bundles = resolve_bundles(&[&web]).expect("resolves");
    let file = &bundles.files("web.assets_backend")[0];
    assert_eq!(file.module, "web");
    assert_eq!(file.path, "web/static/src/deep/nested.js");
    assert!(Path::new(&file.disk).is_file(), "{:?}", file.disk);
    let content = std::fs::read_to_string(&file.disk).expect("readable");
    assert!(content.contains("static/src/deep/nested.js"));
}
