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

    /// Concatenate a bundle, or answer `None` when the name matches no
    /// bundle (or no file of that type inside one).
    fn render_bundle(&self, name: &str) -> Option<Arc<Rendered>> {
        if let Some(hit) = self.cache.read().expect("asset cache lock").get(name) {
            return Some(Arc::clone(hit));
        }
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
        let mut body = Vec::new();
        for file in files {
            match std::fs::read(&file.disk) {
                Ok(content) => {
                    // keep the origin of each chunk visible: a stack trace
                    // in the browser is otherwise a line number into a
                    // file nobody wrote
                    body.extend_from_slice(format!("/* {} */\n", file.path).as_bytes());
                    body.extend_from_slice(&content);
                    body.push(b'\n');
                }
                Err(error) => {
                    // the file was there when the bundle resolved; losing
                    // it now must not serve a silently truncated bundle
                    tracing::error!("asset {} unreadable: {error}", file.path);
                    return None;
                }
            }
        }
        let rendered = Arc::new(Rendered {
            etag: etag_of(&body),
            content_type,
            body,
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
