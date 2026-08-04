//! Serving what an addon ships to the browser, port of the asset routes
//! of `odoo/addons/base/models/ir_asset.py` + `/web/static/...` in
//! `odoo/http.py`:
//!
//! * `GET /{module}/static/{path}` — one file out of one addon
//! * `GET /web/assets/{bundle}.{ext}` — a whole bundle, concatenated
//! * `GET /web/assets/{version}/{bundle}.{ext}` — the same, but the
//!   version in the URL makes the answer safe to cache forever
//!
//! Every path the browser can influence is checked against the addon it
//! claims to come from: an addon serves its own `static/` directory and
//! nothing else, symlinks included.

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use rusdoo_modules::assets::Bundles;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

/// A versioned bundle URL is immutable, so it may be cached for a year
/// (Odoo does the same with its `unique` segment).
const IMMUTABLE_CACHE: &str = "public, max-age=31536000, immutable";
/// Un-versioned answers are revalidated by ETag on every request.
const REVALIDATE_CACHE: &str = "no-cache";

/// The resolved bundles plus the addon directories they came from —
/// everything the HTTP layer needs to answer for static content.
pub struct AssetHub {
    bundles: Bundles,
    /// module technical name -> addon directory
    roots: HashMap<String, PathBuf>,
    /// bundle file name -> the concatenation, built once
    cache: RwLock<HashMap<String, Arc<Rendered>>>,
}

/// A ready answer: bytes, what they are, and the tag that lets a browser
/// skip re-downloading them.
struct Rendered {
    body: Vec<u8>,
    content_type: &'static str,
    etag: String,
    /// the newest mtime among the files it was built from — what tells a
    /// later request that the bundle on disk has moved on
    built_from: Option<std::time::SystemTime>,
}

impl AssetHub {
    pub fn new(bundles: Bundles, roots: HashMap<String, PathBuf>) -> Arc<AssetHub> {
        Arc::new(AssetHub {
            bundles,
            roots,
            cache: RwLock::new(HashMap::new()),
        })
    }

    /// An empty hub: no addon ships assets (or none were loaded). Every
    /// asset route then answers 404 rather than being absent, which is
    /// the difference between a missing file and a broken server.
    pub fn empty() -> Arc<AssetHub> {
        AssetHub::new(Bundles::default(), HashMap::new())
    }

    pub fn bundles(&self) -> &Bundles {
        &self.bundles
    }

    /// Where each installed module lives. The bundles are one thing an
    /// addon directory holds; its `i18n/` catalogues are another, and
    /// both are found the same way.
    pub fn module_roots(&self) -> impl Iterator<Item = (&str, &std::path::Path)> {
        self.roots
            .iter()
            .map(|(module, root)| (module.as_str(), root.as_path()))
    }

    /// Concatenate a bundle, or answer `None` when the name matches no
    /// bundle (or no file of that type inside one).
    fn render_bundle(&self, name: &str) -> Option<Arc<Rendered>> {
        let (bundle, extension) = name.rsplit_once('.')?;
        let (content_type, extensions) = match extension {
            "js" => ("text/javascript; charset=utf-8", &["js", "mjs"][..]),
            "css" => ("text/css; charset=utf-8", &["css", "scss", "less"][..]),
            "xml" => ("text/xml; charset=utf-8", &["xml"][..]),
            _ => return None,
        };
        let files: Vec<_> = self
            .bundles
            .files_with_extension(bundle, extensions)
            .collect();
        if files.is_empty() {
            return None;
        }
        // The JS half of a bundle carries the XML half with it: Odoo
        // compiles the templates into one more module at the end
        // (`<bundle>.bundle.xml`), which is where the client's OWL
        // components find them. They are part of this answer, so their
        // mtimes decide its freshness too.
        let template_files: Vec<_> = if extension == "js" {
            self.bundles.files_with_extension(bundle, &["xml"]).collect()
        } else {
            Vec::new()
        };
        // a cached bundle is only good while the files it was built from
        // are the ones on disk: a deploy that replaces them without
        // restarting the server must not keep serving yesterday's client
        let newest = newest_mtime(&files)
            .into_iter()
            .chain(newest_mtime(&template_files))
            .max();
        if let Some(hit) = self.cache.read().expect("asset cache lock").get(name) {
            if hit.built_from == newest {
                return Some(Arc::clone(hit));
            }
        }
        // the bundle's Sass compiles as one unit, before anything is
        // concatenated. See [`compile_sass`] for why it cannot be per
        // file.
        let compiled = match compile_sass(&files, &self.roots) {
            Ok(compiled) => compiled,
            Err(error) => {
                tracing::error!("bundle {name} does not compile: {error}");
                return None;
            }
        };
        let mut body = Vec::new();
        // `@charset` is only a charset where it is first, so the sheet
        // opens with it rather than carrying grass's own copy inline
        if compiled.is_some() {
            body.extend_from_slice(b"@charset \"UTF-8\";\n");
        }
        let mut sheet_emitted = false;
        for file in files {
            let is_sass = matches!(file.extension(), "scss" | "sass");
            let content = match (is_sass, &compiled) {
                // the whole compiled sheet takes the place of the first
                // Sass file of the bundle; the rest contributed to it
                (true, Some(sheet)) if !sheet_emitted => {
                    sheet_emitted = true;
                    sheet.clone().into_bytes()
                }
                (true, _) => Vec::new(),
                _ => match std::fs::read(&file.disk) {
                    Ok(content) => match as_module(file, content) {
                        Ok(content) => content,
                        Err(error) => {
                            // one file that does not transpile is a syntax
                            // error inside the bundle, and a syntax error
                            // takes down every file after it — so it fails
                            // the bundle rather than shipping the wreck
                            tracing::error!("asset {} does not transpile: {error}", file.path);
                            return None;
                        }
                    },
                    Err(error) => {
                        // the file was there when the bundle resolved;
                        // losing it now must not serve a silently
                        // truncated bundle
                        tracing::error!("asset {} unreadable: {error}", file.path);
                        return None;
                    }
                },
            };
            if content.is_empty() {
                continue;
            }
            // keep the origin of each chunk visible: a stack trace in the
            // browser is otherwise a line number into a file nobody wrote.
            // The compiled sheet carries the markers of its own files.
            if !is_sass {
                body.extend_from_slice(format!("/* {} */\n", file.path).as_bytes());
            }
            body.extend_from_slice(&content);
            body.push(b'\n');
        }
        if !template_files.is_empty() {
            let mut sources: Vec<(String, String)> = Vec::with_capacity(template_files.len());
            for file in &template_files {
                match std::fs::read_to_string(&file.disk) {
                    // the URL the client sees, which is what the template
                    // is registered under
                    Ok(source) => sources.push((format!("/{}", file.path), source)),
                    Err(error) => {
                        tracing::error!("template {} unreadable: {error}", file.path);
                        return None;
                    }
                }
            }
            let templates: Vec<rusdoo_modules::templates::TemplateFile<'_>> = sources
                .iter()
                .map(|(url, source)| rusdoo_modules::templates::TemplateFile { url, source })
                .collect();
            body.extend_from_slice(
                rusdoo_modules::templates::template_module(bundle, &templates).as_bytes(),
            );
        }
        let rendered = Arc::new(Rendered {
            etag: etag_of(&body),
            content_type,
            body,
            built_from: newest,
        });
        self.cache
            .write()
            .expect("asset cache lock")
            .insert(name.to_string(), Arc::clone(&rendered));
        Some(rendered)
    }

    /// Resolve `module` + `path` to a file inside that addon's `static/`
    /// directory, or `None` if it is not one.
    fn static_file(&self, module: &str, path: &str) -> Option<PathBuf> {
        // reject traversal before touching the filesystem
        if path.is_empty()
            || path.starts_with('/')
            || path.contains('\0')
            || path.split('/').any(|segment| segment == ".." || segment == ".")
        {
            return None;
        }
        let root = self.roots.get(module)?;
        // the addon's static/ directory is the only thing it may serve;
        // canonicalizing both sides is what makes a symlink out of the
        // tree a 404 instead of an exfiltration
        let base = root.join("static").canonicalize().ok()?;
        let target = base.join(path).canonicalize().ok()?;
        if !target.starts_with(&base) || !target.is_file() {
            return None;
        }
        Some(target)
    }
}

/// The asset routes, with their own state so they compose with the
/// JSON-RPC router.
pub fn routes(hub: Arc<AssetHub>) -> Router {
    Router::new()
        .route("/web/assets/{file}", get(serve_bundle))
        .route("/web/assets/{version}/{file}", get(serve_versioned_bundle))
        .route("/{module}/static/{*path}", get(serve_static))
        .with_state(hub)
}

async fn serve_bundle(
    State(hub): State<Arc<AssetHub>>,
    headers: HeaderMap,
    Path(file): Path<String>,
) -> Response {
    answer(hub.render_bundle(&file), &headers, REVALIDATE_CACHE)
}

/// The version segment is not checked against anything: it exists so a
/// client that asks for a specific one may cache it forever, and a client
/// that asks for a stale one still gets today's bundle.
async fn serve_versioned_bundle(
    State(hub): State<Arc<AssetHub>>,
    headers: HeaderMap,
    Path((_version, file)): Path<(String, String)>,
) -> Response {
    answer(hub.render_bundle(&file), &headers, IMMUTABLE_CACHE)
}

async fn serve_static(
    State(hub): State<Arc<AssetHub>>,
    headers: HeaderMap,
    Path((module, path)): Path<(String, String)>,
) -> Response {
    let Some(disk) = hub.static_file(&module, &path) else {
        return not_found();
    };
    let Ok(body) = std::fs::read(&disk) else {
        return not_found();
    };
    let rendered = Rendered {
        etag: etag_of(&body),
        content_type: content_type_of(&path),
        body,
        built_from: None,
    };
    answer(Some(Arc::new(rendered)), &headers, REVALIDATE_CACHE)
}

/// Send a rendered answer, honouring `If-None-Match` — a matching ETag
/// costs one 304 instead of the whole bundle.
fn answer(rendered: Option<Arc<Rendered>>, headers: &HeaderMap, cache: &str) -> Response {
    let Some(rendered) = rendered else {
        return not_found();
    };
    let known = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|candidate| candidate.trim() == rendered.etag)
        });
    let common = [
        (header::ETAG, rendered.etag.clone()),
        (header::CACHE_CONTROL, cache.to_string()),
        // a served asset is never interpreted as anything but its type
        (
            header::HeaderName::from_static("x-content-type-options"),
            "nosniff".to_string(),
        ),
    ];
    if known {
        return (StatusCode::NOT_MODIFIED, common).into_response();
    }
    (
        [(header::CONTENT_TYPE, rendered.content_type.to_string())],
        common,
        rendered.body.clone(),
    )
        .into_response()
}

/// The newest modification time among a bundle's files. `None` when the
/// filesystem cannot say — in which case the cache is kept, because
/// rebuilding on every request would be worse than a stale byte.
/// A JavaScript asset as the module the client defines and requires.
///
/// Only the ones that are modules: a vendored library under `static/lib`
/// is loaded as it was published, and wrapping it in an `odoo.define`
/// would break the global it exists to install. `is_odoo_module` is what
/// draws that line, and it draws it exactly where Odoo does.
///
/// Anything that is not JavaScript passes through untouched.
fn as_module(
    file: &rusdoo_modules::assets::AssetFile,
    content: Vec<u8>,
) -> Result<Vec<u8>, rusdoo_core::RusdooError> {
    if !matches!(file.extension(), "js" | "mjs") {
        return Ok(content);
    }
    let url = format!("/{}", file.path);
    // a file that is not valid UTF-8 is not source anybody wrote; it goes
    // out as it came in rather than failing the bundle over an encoding
    let source = match String::from_utf8(content) {
        Ok(source) => source,
        Err(not_text) => return Ok(not_text.into_bytes()),
    };
    if !rusdoo_modules::js_module::is_odoo_module(&url, &source) {
        return Ok(source.into_bytes());
    }
    Ok(rusdoo_modules::js_module::transpile(&url, &source)?.into_bytes())
}

/// The bundle's Sass as the CSS the browser can actually read, port of
/// `preprocess_css` in `odoo/addons/base/models/assetsbundle.py`.
///
/// **One unit, not one per file**, and that is the whole difficulty. An
/// Odoo stylesheet is written expecting the variables and mixins that
/// earlier files of the same bundle defined — `$o-brand-primary`, the
/// Bootstrap helpers — and never imports them itself. Compiled
/// separately, 133 of the 190 stylesheets in the `web` addon fail on an
/// undefined variable. Compiled together, as Odoo compiles them, they
/// have what they were written against.
///
/// The output is then split back per file. Sass reorders nothing inside
/// a unit, so a marker in front of each file's source comes out in front
/// of that file's rules, and the bundle can be reassembled in
/// declaration order with the plain `.css` files still in their places —
/// which is what cascade order means. Odoo splits it the same way, for
/// the same reason.
///
/// The load paths are every addons root plus every `static/lib/*/scss`
/// an addon ships, which is where `@import "variables"` finds Bootstrap:
/// unlike a plain stylesheet's, a Sass `@import` is left exactly as the
/// addon wrote it (`PreprocessedCSS.rx_import = None` there) and is the
/// importer's to resolve.
fn compile_sass(
    files: &[&rusdoo_modules::assets::AssetFile],
    roots: &HashMap<String, PathBuf>,
) -> Result<Option<String>, String> {
    let sass: Vec<&&rusdoo_modules::assets::AssetFile> = files
        .iter()
        .filter(|file| matches!(file.extension(), "scss" | "sass"))
        .collect();
    if sass.is_empty() {
        return Ok(None);
    }
    let mut source = String::new();
    for file in &sass {
        let content = std::fs::read_to_string(&file.disk)
            .map_err(|error| format!("{} unreadable: {error}", file.path))?;
        // a loud comment: Sass keeps it, and every style of output keeps
        // it, which is what makes it usable as a marker
        source.push_str(&format!("/*!{}*/\n", file.path));
        source.push_str(&close_include_statements(&content));
        source.push('\n');
    }

    let mut options = grass::Options::default().style(grass::OutputStyle::Expanded);
    for root in load_paths(files, roots) {
        options = options.load_path(root);
    }
    let css = grass::from_string(source, &options).map_err(|error| error.to_string())?;
    Ok(Some(as_one_sheet(&css)))
}

/// The compiled sheet, with the markers turned into the comments the
/// concatenation uses elsewhere and the `@charset` lifted out.
///
/// It is served whole, never cut: Sass emits a rule's nested children
/// after the parent's own declarations, so a marker can land inside an
/// open block — and cutting there hands the browser an unbalanced brace,
/// which makes it read every rule after it as nested and apply none of
/// them. That is what left the real client unstyled.
fn as_one_sheet(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    for line in css.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("@charset") {
            continue;
        }
        match trimmed
            .strip_prefix("/*!")
            .and_then(|rest| rest.strip_suffix("*/"))
        {
            Some(path) => out.push_str(&format!("/* {path} */\n")),
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

/// Put back the semicolon a `@include` statement is missing.
///
/// libsass accepts `@include mixin($arg)` with nothing after it; this
/// compiler does not, and eight lines across Odoo's own stylesheets are
/// written that way. Refusing them would be correct and useless — the
/// same trade the JS transpiler makes by copying Odoo's regular
/// expressions instead of improving on them. Odoo is the compatibility
/// target, and its leniency is part of what has to be matched.
///
/// Narrow on purpose. A line only qualifies when it is an `@include`
/// with balanced parentheses, nothing after the closing one, and no
/// brace of its own — and even then, not when the next line opens a
/// block, because `@include framed` followed by `{` is a mixin taking
/// content and a semicolon would cut it off from its own body.
fn close_include_statements(source: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let mut out = String::with_capacity(source.len());
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        let looks_unterminated = trimmed.starts_with("@include ")
            && trimmed.ends_with(')')
            && !trimmed.contains('{')
            && !trimmed.contains('}')
            && !trimmed.contains(';');
        let opens_a_block = lines[index + 1..]
            .iter()
            .find(|next| !next.trim().is_empty())
            .is_some_and(|next| next.trim_start().starts_with('{'));
        out.push_str(line);
        if looks_unterminated && !opens_a_block {
            out.push(';');
        }
        out.push('\n');
    }
    out
}

/// Where an `@import` inside the bundle's Sass may be looked up.
///
/// Three kinds, and each is needed by real addons: the directory holding
/// the addons (so a module-qualified `web/static/src/scss/x` resolves),
/// the `scss` directory of every vendored library (so Bootstrap's bare
/// `variables` resolves), and each contributing file's own directory (so
/// a partial next to it resolves, as it would compiling that file alone).
fn load_paths(
    files: &[&rusdoo_modules::assets::AssetFile],
    roots: &HashMap<String, PathBuf>,
) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut add = |path: PathBuf| {
        if path.is_dir() && !paths.contains(&path) {
            paths.push(path);
        }
    };
    for file in files {
        if let Some(directory) = file.disk.parent() {
            add(directory.to_path_buf());
        }
    }
    for root in roots.values() {
        if let Some(addons) = root.parent() {
            add(addons.to_path_buf());
        }
        let lib = root.join("static").join("lib");
        let Ok(entries) = std::fs::read_dir(&lib) else {
            continue;
        };
        for entry in entries.flatten() {
            add(entry.path().join("scss"));
        }
    }
    paths
}

fn newest_mtime(files: &[&rusdoo_modules::assets::AssetFile]) -> Option<std::time::SystemTime> {
    files
        .iter()
        .filter_map(|file| std::fs::metadata(&file.disk).ok()?.modified().ok())
        .max()
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "asset not found").into_response()
}

/// A content-derived tag. Not a checksum anyone relies on for integrity —
/// it only has to change when the bytes do.
fn etag_of(body: &[u8]) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut hasher);
    format!("\"{:016x}\"", hasher.finish())
}

/// What a browser should make of a file, by extension. Anything unknown
/// is served as bytes to download rather than something to execute.
fn content_type_of(path: &str) -> &'static str {
    let extension = path.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");
    match extension.to_ascii_lowercase().as_str() {
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "xml" => "text/xml; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "html" | "htm" => "text/html; charset=utf-8",
        "txt" | "md" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "map" => "application/json; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_extensions_are_not_executable_types() {
        assert_eq!(content_type_of("a/b.js"), "text/javascript; charset=utf-8");
        assert_eq!(content_type_of("a/b.PNG"), "image/png");
        assert_eq!(content_type_of("a/b.exe"), "application/octet-stream");
        assert_eq!(content_type_of("noextension"), "application/octet-stream");
    }

    #[test]
    fn the_etag_follows_the_bytes() {
        assert_eq!(etag_of(b"one"), etag_of(b"one"));
        assert_ne!(etag_of(b"one"), etag_of(b"two"));
    }
}
