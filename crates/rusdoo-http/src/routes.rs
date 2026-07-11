//! HTTP endpoints, mirroring the routes of `odoo/http.py` used by the
//! web client and classic RPC clients.

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap};
use axum::response::{AppendHeaders, Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use serde_json::{json, Map, Value};

/// Odoo's JSON-RPC error code for a missing/expired session.
const SESSION_EXPIRED: i64 = 100;

use crate::dispatch::{OrmService, RpcError};
use crate::jsonrpc::{
    JsonRpcRequest, JsonRpcResponse, INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND,
};

pub fn router(service: OrmService) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/web/dataset/call_kw", post(call_kw))
        .route("/web/session/authenticate", post(authenticate))
        .route("/web/session/destroy", post(destroy))
        .route("/web", get(web_index))
        .route("/web/view/{xml_id}", get(render_view_page))
        .route("/jsonrpc", post(jsonrpc_endpoint))
        .with_state(service)
}

/// Browser-visible status page (the web client lands here in Phase 5).
async fn index() -> Html<&'static str> {
    Html(
        r#"<!doctype html><html lang="pt-BR"><head><meta charset="utf-8">
<title>rusdoo</title>
<style>body{font-family:system-ui;max-width:640px;margin:4rem auto;padding:0 1rem;line-height:1.6}
code,pre{background:#f4f4f4;border-radius:4px;padding:2px 6px}pre{padding:12px;overflow-x:auto}</style>
</head><body>
<h1>&#129408; rusdoo</h1>
<p><strong>O servidor está no ar.</strong> Port do Odoo 19 para Rust — Fase 3 em andamento.</p>
<p>Endpoints JSON-RPC 2.0 (POST):</p>
<ul><li><code>/web/dataset/call_kw</code> — gateway do web client</li>
<li><code>/jsonrpc</code> — RPC clássico (object/execute_kw)</li></ul>
<p>Experimente no terminal:</p>
<pre>curl -s -X POST http://localhost:8069/web/dataset/call_kw   -H 'Content-Type: application/json'   -d '{"jsonrpc":"2.0","id":1,"method":"call","params":{
    "model":"res.partner","method":"search_read","args":[],
    "kwargs":{"fields":["name","email"]}}}'</pre>
</body></html>"#,
    )
}

fn respond(id: Option<Value>, outcome: Result<Value, RpcError>) -> Json<JsonRpcResponse> {
    Json(match outcome {
        Ok(result) => JsonRpcResponse::result(id, result),
        Err(err) => JsonRpcResponse::error(id, err.code, err.message),
    })
}

fn parse_envelope(body: Value) -> Result<JsonRpcRequest, JsonRpcResponse> {
    let id = body.get("id").cloned();
    serde_json::from_value::<JsonRpcRequest>(body)
        .map_err(|_| JsonRpcResponse::error(id, INVALID_REQUEST, "invalid JSON-RPC 2.0 request"))
}

/// `/web/dataset/call_kw` — the web client's ORM gateway.
/// params: `{model, method, args, kwargs}`.
async fn call_kw(
    State(service): State<OrmService>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<JsonRpcResponse> {
    let request = match parse_envelope(body) {
        Ok(request) => request,
        Err(error) => return Json(error),
    };
    let session = current_session(&service, &headers);
    if service.require_auth && session.is_none() {
        return Json(JsonRpcResponse::error(
            request.id,
            SESSION_EXPIRED,
            "rusdoo session expired",
        ));
    }
    let params = &request.params;
    let (Some(model), Some(method)) = (
        params.get("model").and_then(Value::as_str),
        params.get("method").and_then(Value::as_str),
    ) else {
        return Json(JsonRpcResponse::error(
            request.id,
            INVALID_PARAMS,
            "params must include model and method",
        ));
    };
    // ir.model.access enforcement (skipped for insecure/no-session tooling)
    if let Some(session) = &session {
        if let Err(error) = service.check_access(model, method, session) {
            return Json(JsonRpcResponse::error(
                request.id,
                error.code,
                error.message,
            ));
        }
    }
    let args = params
        .get("args")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let kwargs = params
        .get("kwargs")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    // attribute writes to the session user; unauthenticated tooling acts as superuser
    let uid = session
        .as_ref()
        .map(|s| s.uid)
        .unwrap_or(crate::session::SUPERUSER_ID);
    let outcome = service.call_kw(uid, model, method, &args, &kwargs).await;
    respond(request.id, outcome)
}

/// `/jsonrpc` — classic RPC: `{service: "object", method: "execute_kw",
/// args: [db, uid, password, model, method, args, kwargs?]}`.
/// uid+password are verified per call and the ACL is enforced with the
/// verified uid as a transient identity (db is not yet routed).
async fn jsonrpc_endpoint(
    State(service): State<OrmService>,
    Json(body): Json<Value>,
) -> Json<JsonRpcResponse> {
    let request = match parse_envelope(body) {
        Ok(request) => request,
        Err(error) => return Json(error),
    };
    let params = &request.params;
    let rpc_service = params.get("service").and_then(Value::as_str);
    let rpc_method = params.get("method").and_then(Value::as_str);
    if rpc_service != Some("object") || rpc_method != Some("execute_kw") {
        return Json(JsonRpcResponse::error(
            request.id,
            METHOD_NOT_FOUND,
            "only service 'object' method 'execute_kw' is supported",
        ));
    }
    let empty = Vec::new();
    let call = params
        .get("args")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    if service.require_auth {
        let authorized = match (
            call.get(1).and_then(Value::as_i64),
            call.get(2).and_then(Value::as_str),
        ) {
            (Some(uid), Some(password)) => service.check_credentials(uid, password).await,
            _ => false,
        };
        if !authorized {
            return Json(JsonRpcResponse::error(
                request.id,
                SESSION_EXPIRED,
                "acesso negado: credenciais inválidas",
            ));
        }
    }
    let (Some(model), Some(method)) = (
        call.get(3).and_then(Value::as_str),
        call.get(4).and_then(Value::as_str),
    ) else {
        return Json(JsonRpcResponse::error(
            request.id,
            INVALID_PARAMS,
            "execute_kw args must be [db, uid, password, model, method, args, kwargs?]",
        ));
    };
    // ir.model.access enforcement on the classic RPC path too — the
    // verified uid becomes a transient access identity
    if service.require_auth {
        if let Some(uid) = call.get(1).and_then(Value::as_i64) {
            let identity = service.identity(uid).await;
            if let Err(error) = service.check_access(model, method, &identity) {
                return Json(JsonRpcResponse::error(
                    request.id,
                    error.code,
                    error.message,
                ));
            }
        }
    }
    let args = call
        .get(5)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let kwargs: Map<String, Value> = call
        .get(6)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    // the verified uid (or superuser when auth is disabled) owns the write
    let uid = call
        .get(1)
        .and_then(Value::as_i64)
        .unwrap_or(crate::session::SUPERUSER_ID);
    let outcome = service.call_kw(uid, model, method, &args, &kwargs).await;
    respond(request.id, outcome)
}

fn current_session(service: &OrmService, headers: &HeaderMap) -> Option<crate::session::Session> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    let token = cookie
        .split(';')
        .find_map(|part| part.trim().strip_prefix("session_id="))?;
    service.sessions.get(token)
}

/// `/web/session/authenticate` — params `{db, login, password}`.
/// Success sets the `session_id` cookie; failures share one message
/// (no user enumeration).
async fn authenticate(State(service): State<OrmService>, Json(body): Json<Value>) -> Response {
    let request = match parse_envelope(body) {
        Ok(request) => request,
        Err(error) => return Json(error).into_response(),
    };
    let params = &request.params;
    let (Some(login), Some(password)) = (
        params.get("login").and_then(Value::as_str),
        params.get("password").and_then(Value::as_str),
    ) else {
        return Json(JsonRpcResponse::error(
            request.id,
            INVALID_PARAMS,
            "params must include login and password",
        ))
        .into_response();
    };

    // resolve the user (if any), then always spend the same verify work
    let mut candidate: Option<(i64, String)> = None;
    if let Ok(domain) = parse_domain(&json!([["login", "=", login]])) {
        if let Ok(ids) = service
            .registry
            .search(
                &service.pool,
                "res.users",
                &domain,
                &SearchOptions::default(),
            )
            .await
        {
            if let Some(uid) = ids.first() {
                if let Ok(rows) = service
                    .registry
                    .read(&service.pool, "res.users", &[*uid], &["password"])
                    .await
                {
                    if let Some(hash) = rows.first().and_then(|row| row["password"].as_str()) {
                        candidate = Some((*uid, hash.to_string()));
                    }
                }
            }
        }
    }
    let hash = candidate.as_ref().map(|(_, hash)| hash.as_str());
    let authenticated_uid = if service.verify(password, hash).await {
        candidate.as_ref().map(|(uid, _)| *uid)
    } else {
        None
    };

    match authenticated_uid {
        None => Json(JsonRpcResponse::error(
            request.id,
            SESSION_EXPIRED,
            "acesso negado: login ou senha inválidos",
        ))
        .into_response(),
        Some(uid) => {
            let groups = service.resolve_groups(uid).await;
            let token = service.sessions.open(uid, login, groups);
            (
                AppendHeaders([(
                    header::SET_COOKIE,
                    format!(
                        "session_id={token}; HttpOnly; Path=/; SameSite=Lax{}",
                        if service.secure_cookies {
                            "; Secure"
                        } else {
                            ""
                        }
                    ),
                )]),
                Json(JsonRpcResponse::result(
                    request.id,
                    json!({"uid": uid, "username": login}),
                )),
            )
                .into_response()
        }
    }
}

/// `/web/session/destroy` — logout.
async fn destroy(
    State(service): State<OrmService>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<JsonRpcResponse> {
    let id = body.get("id").cloned();
    if let Some(cookie) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) {
        if let Some(token) = cookie
            .split(';')
            .find_map(|part| part.trim().strip_prefix("session_id="))
        {
            service.sessions.close(token);
        }
    }
    Json(JsonRpcResponse::result(id, Value::Bool(true)))
}

/// `GET /web/view/{xml_id}` — render a stored ir.ui.view to an HTML page.
async fn render_view_page(
    State(service): State<OrmService>,
    headers: HeaderMap,
    Path(xml_id): Path<String>,
) -> Response {
    let session = current_session(&service, &headers);
    if service.require_auth && session.is_none() {
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            Html("<h1>401</h1><p>faça login em /web/session/authenticate</p>".to_string()),
        )
            .into_response();
    }
    match service.render_view(&xml_id, session.as_ref()).await {
        Ok(html) => Html(html).into_response(),
        Err(err) => {
            // never reflect the request or internal error text into HTML
            // (XSS / info leak): log server-side, return a generic page
            tracing::warn!("render_view {xml_id:?} failed: {}", err.message);
            (
                axum::http::StatusCode::BAD_REQUEST,
                Html("<h1>erro</h1><p>não foi possível renderizar a view</p>".to_string()),
            )
                .into_response()
        }
    }
}

/// Minimal HTML escaping for text interpolated into pages.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `GET /web` — a navigable index of the stored views.
async fn web_index(State(service): State<OrmService>, headers: HeaderMap) -> Response {
    let session = current_session(&service, &headers);
    if service.require_auth && session.is_none() {
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            Html("<h1>401</h1><p>faça login em /web/session/authenticate</p>".to_string()),
        )
            .into_response();
    }
    match service.list_views(session.as_ref()).await {
        Ok(views) => {
            let mut body = String::from(
                "<!doctype html><meta charset=\"utf-8\"><title>rusdoo</title>\
                 <div style=\"font-family:system-ui;max-width:640px;margin:3rem auto\">\
                 <h1>&#129408; rusdoo</h1><p>Views disponíveis:</p><ul>",
            );
            if views.is_empty() {
                body.push_str("<li>nenhuma view instalada (rode --init sobre um addon)</li>");
            }
            for (xml_id, name) in views {
                body.push_str(&format!(
                    "<li><a href=\"/web/view/{}\">{}</a></li>",
                    html_escape(&xml_id),
                    html_escape(&name)
                ));
            }
            body.push_str("</ul></div>");
            Html(body).into_response()
        }
        Err(err) => {
            tracing::warn!("web_index failed: {}", err.message);
            (
                axum::http::StatusCode::BAD_REQUEST,
                Html("<h1>erro</h1><p>não foi possível listar as views</p>".to_string()),
            )
                .into_response()
        }
    }
}
