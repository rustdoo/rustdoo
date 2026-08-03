//! Files kept next to a record, port of `ir.attachment` and the
//! `/web/binary/*` + `/web/content/*` routes of `odoo/http.py`.
//!
//! The bytes live in a filestore directory, not in the row: a database
//! dump should not carry every PDF anyone ever uploaded. The row says
//! what the file is, which record it belongs to, and where the bytes
//! went.
//!
//! Known deviation: Odoo names files by the sha1 of their content, which
//! also deduplicates them. Here a file is named by its attachment id —
//! no hashing, no collisions to reason about, and no dedup. Deleting the
//! row therefore leaves the file behind; a garbage collection pass is
//! not ported yet, and losing the bytes of a still-referenced attachment
//! would be worse than keeping an orphan.

use crate::dispatch::{OrmService, RpcError};
use crate::session::Session;
use axum::body::Body;
use axum::extract::{Multipart, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{AppendHeaders, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rusdoo_orm::access::Operation;
use serde_json::{json, Value};
use std::path::PathBuf;

/// The most a single upload may carry. Odoo announces a limit in
/// `get_session_info`; this is the one the server actually enforces.
const MAX_UPLOAD_BYTES: usize = 25 * 1024 * 1024;

/// Where the bytes go when nothing else is configured.
const DEFAULT_FILESTORE: &str = "filestore";

/// The filestore a service starts with: `RUSDOO_FILESTORE`, or
/// `./filestore`. A service may be pointed elsewhere.
pub fn default_filestore() -> PathBuf {
    std::env::var("RUSDOO_FILESTORE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_FILESTORE))
}

/// One file on its way in: what it is and which record it joins.
pub struct Upload<'a> {
    pub name: &'a str,
    pub mimetype: &'a str,
    pub res_model: &'a str,
    pub res_id: i64,
    pub bytes: Vec<u8>,
}

impl OrmService {
    /// Store an upload as an attachment of its record.
    ///
    /// Attaching a file to a record is writing to that record: the
    /// caller is held to write access on it, not only to create access
    /// on `ir.attachment` — otherwise anyone who may create attachments
    /// could staple a document to somebody else's invoice.
    pub async fn store_attachment(
        &self,
        uid: i64,
        session: Option<&Session>,
        upload: Upload<'_>,
    ) -> Result<i64, RpcError> {
        let Upload {
            name,
            mimetype,
            res_model,
            res_id,
            bytes,
        } = upload;
        if bytes.is_empty() {
            return Err(RpcError::invalid_params("o arquivo está vazio"));
        }
        if bytes.len() > MAX_UPLOAD_BYTES {
            return Err(RpcError::invalid_params(format!(
                "o arquivo passa do limite de {} MB",
                MAX_UPLOAD_BYTES / 1024 / 1024
            )));
        }
        if self.registry.get(res_model).is_none() {
            return Err(RpcError::invalid_params(format!(
                "modelo desconhecido: {res_model}"
            )));
        }
        if let Some(session) = session {
            self.check_access("ir.attachment", "create", session)?;
            self.check_access(res_model, "write", session)?;
        }
        self.check_records(uid, res_model, Operation::Write, &[res_id])
            .await?;

        let id = self
            .registry
            .create_as(
                &self.pool,
                uid,
                "ir.attachment",
                vec![
                    ("name", json!(clean_name(name))),
                    ("res_model", json!(res_model)),
                    ("res_id", json!(res_id)),
                    ("mimetype", json!(mimetype)),
                    ("file_size", json!(bytes.len() as i64)),
                ],
            )
            .await?;
        // the row first, then the bytes: a file with no row would be
        // invisible, and cleaning it up would need a scan of the disk
        let path = self.filestore.join(id.to_string());
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                RpcError::from(rusdoo_core::RusdooError::Validation(format!(
                    "não foi possível preparar o filestore: {error}"
                )))
            })?;
        }
        if let Err(error) = std::fs::write(&path, &bytes) {
            // the bytes did not land: the row must not claim they did
            let _ = self
                .registry
                .get("ir.attachment")
                .expect("registered")
                .unlink(&self.pool, &[id])
                .await;
            return Err(RpcError::from(rusdoo_core::RusdooError::Validation(
                format!("não foi possível gravar o arquivo: {error}"),
            )));
        }
        self.registry
            .write_as(
                &self.pool,
                uid,
                "ir.attachment",
                &[id],
                vec![("store_fname", json!(id.to_string()))],
            )
            .await?;
        Ok(id)
    }

    /// The bytes of an attachment, plus what to call them.
    ///
    /// Reading a file is reading the record it hangs from: the ACL of
    /// that model and its record rules decide, exactly as they do for
    /// the fields on the same screen.
    pub async fn read_attachment(
        &self,
        uid: i64,
        session: Option<&Session>,
        id: i64,
    ) -> Result<(String, String, Vec<u8>), RpcError> {
        if let Some(session) = session {
            self.check_access("ir.attachment", "read", session)?;
        }
        let rows = self
            .registry
            .read(
                &self.pool,
                "ir.attachment",
                &[id],
                &["name", "mimetype", "res_model", "res_id"],
            )
            .await?;
        let row = rows
            .first()
            .ok_or_else(|| RpcError::invalid_params(format!("anexo {id} não existe")))?;
        let res_model = row
            .get("res_model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let res_id = row.get("res_id").and_then(Value::as_i64);
        if let (Some(session), false) = (session, res_model.is_empty()) {
            self.check_access(&res_model, "read", session)?;
        }
        if let (false, Some(res_id)) = (res_model.is_empty(), res_id) {
            self.check_records(uid, &res_model, Operation::Read, &[res_id])
                .await?;
        }
        let bytes = std::fs::read(self.filestore.join(id.to_string())).map_err(|error| {
            RpcError::from(rusdoo_core::RusdooError::Validation(format!(
                "os bytes do anexo {id} não estão no filestore: {error}"
            )))
        })?;
        let name = row
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("arquivo")
            .to_string();
        let mimetype = row
            .get("mimetype")
            .and_then(Value::as_str)
            .filter(|mimetype| !mimetype.is_empty())
            .unwrap_or("application/octet-stream")
            .to_string();
        Ok((name, mimetype, bytes))
    }
}

/// A file name as a header may carry it: one line, no path, no quotes.
/// The name is data a client sent, and it ends up in a header and on a
/// screen — never in a path, which is why the bytes are stored under the
/// attachment id instead.
fn clean_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && *c != '"')
        .take(200)
        .collect();
    if cleaned.trim().is_empty() {
        "arquivo".to_string()
    } else {
        cleaned
    }
}

pub fn routes(service: OrmService) -> Router {
    Router::new()
        .route("/web/binary/upload_attachment", post(upload))
        .route("/web/content/{id}", get(download))
        .with_state(service)
}

/// `POST /web/binary/upload_attachment` — multipart: `model`, `id` and
/// one or more `ufile` parts, like Odoo's own upload endpoint.
async fn upload(
    State(service): State<OrmService>,
    headers: HeaderMap,
    mut form: Multipart,
) -> Response {
    let session = crate::routes::session_of(&service, &headers);
    if service.require_auth && session.is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "rusdoo session expired"})),
        )
            .into_response();
    }
    let uid = session
        .as_ref()
        .map(|session| session.uid)
        .unwrap_or(crate::session::SUPERUSER_ID);

    let mut model = String::new();
    let mut res_id: Option<i64> = None;
    let mut files: Vec<(String, String, Vec<u8>)> = Vec::new();
    loop {
        let field = match form.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("upload malformado: {error}")})),
                )
                    .into_response()
            }
        };
        let name = field.name().unwrap_or_default().to_string();
        let file_name = field.file_name().unwrap_or_default().to_string();
        let mimetype = field
            .content_type()
            .unwrap_or("application/octet-stream")
            .to_string();
        let Ok(bytes) = field.bytes().await else {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "não foi possível ler o arquivo enviado"})),
            )
                .into_response();
        };
        match name.as_str() {
            "model" => model = String::from_utf8_lossy(&bytes).into_owned(),
            "id" => res_id = String::from_utf8_lossy(&bytes).trim().parse().ok(),
            _ => files.push((file_name, mimetype, bytes.to_vec())),
        }
    }

    let (Some(res_id), false) = (res_id, model.is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "o upload precisa dizer model e id"})),
        )
            .into_response();
    };
    let mut stored = Vec::new();
    for (name, mimetype, bytes) in files {
        match service
            .store_attachment(
                uid,
                session.as_ref(),
                Upload {
                    name: &name,
                    mimetype: &mimetype,
                    res_model: &model,
                    res_id,
                    bytes,
                },
            )
            .await
        {
            Ok(id) => stored.push(json!({"id": id, "name": clean_name(&name)})),
            Err(error) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": error.message})),
                )
                    .into_response()
            }
        }
    }
    if stored.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "nenhum arquivo no upload"})),
        )
            .into_response();
    }
    Json(json!({"attachments": stored})).into_response()
}

/// `GET /web/content/{id}` — the file itself.
async fn download(
    State(service): State<OrmService>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    let session = crate::routes::session_of(&service, &headers);
    if service.require_auth && session.is_none() {
        return (StatusCode::UNAUTHORIZED, "faça login para baixar").into_response();
    }
    let uid = session
        .as_ref()
        .map(|session| session.uid)
        .unwrap_or(crate::session::SUPERUSER_ID);
    match service.read_attachment(uid, session.as_ref(), id).await {
        Ok((name, mimetype, bytes)) => (
            [(header::CONTENT_TYPE, mimetype)],
            AppendHeaders([
                (
                    header::CONTENT_DISPOSITION,
                    // always an attachment: an uploaded file is not
                    // something this origin should render in a tab
                    format!("attachment; filename=\"{}\"", clean_name(&name)),
                ),
                (
                    header::HeaderName::from_static("x-content-type-options"),
                    "nosniff".to_string(),
                ),
                (header::CACHE_CONTROL, "private, max-age=0".to_string()),
            ]),
            Body::from(bytes),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!("anexo {id}: {}", error.message);
            (StatusCode::NOT_FOUND, "anexo não encontrado").into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_name_never_becomes_a_path() {
        assert_eq!(clean_name("../../etc/passwd"), "passwd");
        assert_eq!(clean_name("C:\\Users\\ana\\nota.pdf"), "nota.pdf");
        // a name with a separator keeps only its last segment, quotes
        // and control characters go, and nothing empty survives
        assert_eq!(clean_name("nota\"; rm -rf /.pdf"), ".pdf");
        assert_eq!(clean_name("relatório \"final\".pdf"), "relatório final.pdf");
        assert_eq!(clean_name("linha\nquebrada.txt"), "linhaquebrada.txt");
        assert_eq!(clean_name("   "), "arquivo");
        assert_eq!(clean_name(""), "arquivo");
    }
}
