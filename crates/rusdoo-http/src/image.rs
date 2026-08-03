//! `/web/image/<model>/<id>/<field>` — a `Binary` field served as bytes,
//! port of `odoo/addons/web/controllers/binary.py`.
//!
//! A list of forty products with a picture each would otherwise mean
//! forty base64 blobs inside one JSON response: a third bigger than the
//! bytes they carry, held as strings, and re-sent whenever anything on
//! the screen changes. An `<img src>` is what a browser is good at —
//! it caches, it loads in parallel, and it does not block the rest.
//!
//! The access check is the record's own: a picture is a field, so seeing
//! it is reading the record it belongs to. A route that skipped that
//! would be a way to read fields the ACL refuses through `call_kw`.

use crate::dispatch::OrmService;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{AppendHeaders, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use rusdoo_orm::access::Operation;
use rusdoo_orm::fields::FieldType;

pub fn routes(service: OrmService) -> Router {
    Router::new()
        .route("/web/image/{model}/{id}/{field}", get(image))
        .with_state(service)
}

/// What the bytes look like, guessed from the bytes themselves.
///
/// Never from a name or a parameter the caller controls: this response
/// is served from the application's own origin, and letting a caller
/// choose the content type of bytes they uploaded is how a stored image
/// becomes stored script.
fn sniff(bytes: &[u8]) -> &'static str {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => "image/png",
        [0xFF, 0xD8, 0xFF, ..] => "image/jpeg",
        [b'G', b'I', b'F', b'8', ..] => "image/gif",
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => "image/webp",
        // an SVG is a document with script in it; served as an image it
        // would run on this origin, so it is downloaded, never rendered
        _ => "application/octet-stream",
    }
}

async fn image(
    State(service): State<OrmService>,
    headers: HeaderMap,
    Path((model, id, field)): Path<(String, i64, String)>,
) -> Response {
    let session = crate::routes::session_of(&service, &headers);
    if service.require_auth && session.is_none() {
        return (StatusCode::UNAUTHORIZED, "log in").into_response();
    }
    let uid = session
        .as_ref()
        .map(|session| session.uid)
        .unwrap_or(crate::session::SUPERUSER_ID);

    // the field has to be a Binary of this model, and readable: a route
    // that answered for any field would read around `fields_get`'s
    // exposure rules
    let Some(meta) = service.registry_ref().get(&model) else {
        return (StatusCode::NOT_FOUND, "modelo desconhecido").into_response();
    };
    match meta.field(&field) {
        Some(f) if matches!(f.ty, FieldType::Binary) && f.exposed => {}
        _ => return (StatusCode::NOT_FOUND, "field is not an image").into_response(),
    }
    if let Err(error) = service
        .check_records(uid, &model, Operation::Read, &[id])
        .await
    {
        tracing::debug!("imagem {model}/{id}/{field}: {}", error.message);
        return (StatusCode::NOT_FOUND, "record not found").into_response();
    }

    let rows = match meta.read(service.pool_ref(), &[id], &[field.as_str()]).await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!("imagem {model}/{id}/{field}: {error}");
            return (StatusCode::NOT_FOUND, "record not found").into_response();
        }
    };
    let encoded = rows
        .first()
        .and_then(|row| row.get(&field))
        .and_then(|value| value.as_str());
    let Some(encoded) = encoded else {
        // an empty field is not an error: it is a record with no picture,
        // and the client draws its placeholder from the status
        return (StatusCode::NO_CONTENT, "").into_response();
    };
    use base64::Engine;
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "unreadable content").into_response();
    };

    let mime = sniff(&bytes);
    (
        [(header::CONTENT_TYPE, mime.to_string())],
        AppendHeaders([
            (
                header::HeaderName::from_static("x-content-type-options"),
                "nosniff".to_string(),
            ),
            // what makes the route worth having: the browser stops asking
            (header::CACHE_CONTROL, "private, max-age=3600".to_string()),
        ]),
        Body::from(bytes),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_type_comes_from_the_bytes_not_from_the_caller() {
        assert_eq!(sniff(&[0x89, b'P', b'N', b'G', 13, 10]), "image/png");
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), "image/jpeg");
        assert_eq!(sniff(b"GIF89a..."), "image/gif");
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), "image/webp");
        // an SVG names itself an image and is a document: it downloads
        assert_eq!(sniff(b"<svg xmlns=\"...\"><script/></svg>"), "application/octet-stream");
        assert_eq!(sniff(b""), "application/octet-stream");
    }
}
