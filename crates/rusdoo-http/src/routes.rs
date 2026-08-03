//! HTTP endpoints, mirroring the routes of `odoo/http.py` used by the
//! web client and classic RPC clients.

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{AppendHeaders, Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// Odoo's JSON-RPC error code for a missing/expired session.
const SESSION_EXPIRED: i64 = 100;

use crate::dispatch::{OrmService, RpcError};
use crate::jsonrpc::{
    JsonRpcRequest, JsonRpcResponse, INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND,
};

pub fn router(service: OrmService) -> Router {
    // the asset routes carry their own state, so they are merged after
    // the ORM routes have taken theirs
    let assets = crate::assets::routes(Arc::clone(&service.assets));
    Router::new()
        .route("/", get(index))
        .route("/web/dataset/call_kw", post(call_kw))
        .route("/web/session/authenticate", post(authenticate))
        .route("/web/session/destroy", post(destroy))
        .route("/web/session/get_session_info", post(session_info))
        .route("/web/webclient/load_menus", get(load_menus))
        .route(
            "/web/webclient/translations",
            get(crate::translations::translations),
        )
        .route("/web/action/load", post(action_load))
        .route("/web", get(web_index))
        .route("/web/view/{xml_id}", get(render_view_page))
        .route("/report/html/{xml_id}/{res_id}", get(render_report_page))
        .route("/report/pdf/{xml_id}/{res_id}", get(render_report_pdf))
        .route("/web/action/{xml_id}", get(render_action_page))
        .route("/jsonrpc", post(jsonrpc_endpoint))
        .with_state(service.clone())
        .merge(assets)
        .merge(crate::image::routes(service.clone()))
        .merge(crate::attachment::routes(service))
}

/// Browser-visible status page (the web client lands here in Phase 5).
async fn index() -> Html<&'static str> {
    Html(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8">
<title>rusdoo</title>
<style>body{font-family:system-ui;max-width:640px;margin:4rem auto;padding:0 1rem;line-height:1.6}
code,pre{background:#f4f4f4;border-radius:4px;padding:2px 6px}pre{padding:12px;overflow-x:auto}</style>
</head><body>
<h1>&#129408; rusdoo</h1>
<p><strong>The server is up.</strong> A port of Odoo 19 to Rust.</p>
<p>Endpoints JSON-RPC 2.0 (POST):</p>
<ul><li><code>/web/dataset/call_kw</code> — the web client gateway</li>
<li><code>/jsonrpc</code> — the classic RPC (object/execute_kw)</li></ul>
<p>Try it from a terminal:</p>
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
    warn_unverified_attribution(&service, uid);
    let outcome = service.call_kw(uid, model, method, &args, &kwargs).await;
    respond(request.id, outcome)
}

/// In insecure mode (`require_auth = false`) the acting uid is taken
/// unverified from the caller, yet still stamped into create_uid/write_uid.
/// Flag any non-superuser attribution so a misconfigured deployment can't
/// silently forge an audit trail without a trace.
fn warn_unverified_attribution(service: &OrmService, uid: i64) {
    if !service.require_auth && uid != crate::session::SUPERUSER_ID {
        tracing::warn!(
            uid,
            "insecure mode: stamping unverified user id into LOG_ACCESS audit columns"
        );
    }
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
                "access denied: invalid credentials",
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
    warn_unverified_attribution(&service, uid);
    let outcome = service.call_kw(uid, model, method, &args, &kwargs).await;
    respond(request.id, outcome)
}

/// `/web/session/get_session_info` — what the web client boots with.
/// Anonymous sessions get the public answer (uid null), like Odoo:
/// the client itself decides to route to the login page.
async fn session_info(
    State(service): State<OrmService>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<JsonRpcResponse> {
    let request = match parse_envelope(body) {
        Ok(request) => request,
        Err(error) => return Json(error),
    };
    let session = current_session(&service, &headers);
    let info = service.session_info(session.as_ref()).await;
    Json(JsonRpcResponse::result(request.id, info))
}

/// `/web/action/load` — params `{action_id}`: the action a menu click
/// opens, by database id or external id.
async fn action_load(
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
    let Some(reference) = request.params.get("action_id") else {
        return Json(JsonRpcResponse::error(
            request.id,
            INVALID_PARAMS,
            "params must include action_id",
        ));
    };
    let outcome = service.load_action(reference, session.as_ref()).await;
    respond(request.id, outcome)
}

/// `/web/webclient/load_menus` — the navigation tree, flat and keyed by
/// id. A plain JSON body (not a JSON-RPC envelope), like Odoo's http
/// route, and never cached: menus depend on who is asking.
async fn load_menus(State(service): State<OrmService>, headers: HeaderMap) -> Response {
    let session = current_session(&service, &headers);
    if service.require_auth && session.is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            AppendHeaders([(header::CACHE_CONTROL, "no-store")]),
            Json(json!({"error": "rusdoo session expired"})),
        )
            .into_response();
    }
    match service.web_menus(session.as_ref()).await {
        Ok(menus) => (
            AppendHeaders([(header::CACHE_CONTROL, "no-store")]),
            Json(menus),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error.message})),
        )
            .into_response(),
    }
}

/// The session a request carries, for the routes that live in another
/// module (assets, attachments) and answer for the same user.
pub(crate) fn session_of(
    service: &OrmService,
    headers: &HeaderMap,
) -> Option<crate::session::Session> {
    current_session(service, headers)
}

pub(crate) fn current_session(service: &OrmService, headers: &HeaderMap) -> Option<crate::session::Session> {
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
            "access denied: wrong login or password",
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
            Html("<h1>401</h1><p>log in at /web/session/authenticate</p>".to_string()),
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
                Html("<h1>error</h1><p>the view could not be rendered</p>".to_string()),
            )
                .into_response()
        }
    }
}

/// `GET /report/html/{xml_id}/{res_id}` — a document to print.
///
/// The answer is a page, not a JSON-RPC result: it is opened in a tab
/// and printed from there. Odoo hands the same HTML to wkhtmltopdf.
async fn render_report_page(
    State(service): State<OrmService>,
    headers: HeaderMap,
    Path((xml_id, res_id)): Path<(String, i64)>,
) -> Response {
    let session = current_session(&service, &headers);
    if service.require_auth && session.is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            Html("<h1>401</h1><p>log in to print</p>".to_string()),
        )
            .into_response();
    }
    match service
        .render_report(&xml_id, res_id, session.as_ref())
        .await
    {
        Ok(html) => (
            // a printed document is what the database says now, never a
            // page a proxy kept from an hour ago
            AppendHeaders([(header::CACHE_CONTROL, "no-store")]),
            Html(html),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!("report {xml_id}/{res_id} failed: {}", error.message);
            (
                StatusCode::BAD_REQUEST,
                Html("<h1>error</h1><p>the document could not be produced</p>".to_string()),
            )
                .into_response()
        }
    }
}

/// `GET /report/pdf/{xml_id}/{res_id}` — the same document, converted.
///
/// The same HTML `/report/html/` serves, handed to whatever converter
/// the machine has. With none, this answers 503 and says which variable
/// names one — and `/report/html/` keeps working, so nothing is lost but
/// the file extension. What it must never do is serve the HTML under a
/// PDF content type: a reader that refuses a file without explaining
/// why sends the user looking at their own machine.
async fn render_report_pdf(
    State(service): State<OrmService>,
    headers: HeaderMap,
    Path((xml_id, res_id)): Path<(String, i64)>,
) -> Response {
    let session = current_session(&service, &headers);
    if service.require_auth && session.is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            Html("<h1>401</h1><p>log in to print</p>".to_string()),
        )
            .into_response();
    }
    let Some(renderer) = service.pdf.clone() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Html(
                "<h1>503</h1><p>this server has no PDF converter: install \
                 weasyprint (or name one in RUSDOO_PDF_BIN). The same \
                 document is at /report/html/.</p>"
                    .to_string(),
            ),
        )
            .into_response();
    };
    let html = match service.render_report(&xml_id, res_id, session.as_ref()).await {
        Ok(html) => html,
        Err(error) => {
            tracing::warn!("report {xml_id}/{res_id} failed: {}", error.message);
            return (
                StatusCode::BAD_REQUEST,
                Html("<h1>error</h1><p>the document could not be produced</p>".to_string()),
            )
                .into_response();
        }
    };
    // the converter is a process, and waiting on one blocks. Told, the
    // runtime keeps the rest of its work moving.
    let converted =
        tokio::task::spawn_blocking(move || renderer.render(&html)).await;
    let pdf = match converted {
        Ok(Ok(pdf)) => pdf,
        Ok(Err(error)) => {
            tracing::error!("report {xml_id}/{res_id} did not convert: {error}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<h1>error</h1><p>the document could not be converted</p>".to_string()),
            )
                .into_response();
        }
        Err(error) => {
            tracing::error!("report {xml_id}/{res_id} conversion panicked: {error}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<h1>error</h1><p>the document could not be converted</p>".to_string()),
            )
                .into_response();
        }
    };
    (
        AppendHeaders([
            (header::CONTENT_TYPE, "application/pdf".to_string()),
            // inline: a printed document is opened, not downloaded. The
            // name is the report and the record, because a browser's
            // download directory full of `document.pdf` helps nobody.
            (
                header::CONTENT_DISPOSITION,
                format!(
                    "inline; filename=\"{}-{res_id}.pdf\"",
                    xml_id.replace(['/', '\\', '"'], "-")
                ),
            ),
            // what the database says now, never what a proxy kept
            (header::CACHE_CONTROL, "no-store".to_string()),
        ]),
        pdf,
    )
        .into_response()
}

/// `GET /web/action/{xml_id}` — open an ir.actions.act_window: render its
/// target model in a view. The navigation target of a menu click.
async fn render_action_page(
    State(service): State<OrmService>,
    headers: HeaderMap,
    Path(xml_id): Path<String>,
) -> Response {
    let session = current_session(&service, &headers);
    if service.require_auth && session.is_none() {
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            Html("<h1>401</h1><p>log in at /web/session/authenticate</p>".to_string()),
        )
            .into_response();
    }
    match service.render_action(&xml_id, session.as_ref()).await {
        Ok(html) => Html(html).into_response(),
        Err(err) => {
            tracing::warn!("render_action {xml_id:?} failed: {}", err.message);
            (
                axum::http::StatusCode::BAD_REQUEST,
                Html("<h1>error</h1><p>the action could not be opened</p>".to_string()),
            )
                .into_response()
        }
    }
}

/// Render the menu forest as nested `<ul>`; leaves with an action link to
/// `/web/action/{xml_id}`.
fn render_menu(items: &[crate::dispatch::MenuItem], out: &mut String) {
    out.push_str("<ul>");
    for item in items {
        out.push_str("<li>");
        // only a well-formed xml_id (module.name) becomes a link. Refusing
        // anything else keeps the value a single, safe path segment: a `/`,
        // `..`, `?` or `#` can never redirect the click outside /web/action/.
        match item.action.as_deref().filter(|a| is_xml_id(a)) {
            Some(action) => out.push_str(&format!(
                "<a href=\"/web/action/{}\">{}</a>",
                action,
                html_escape(&item.name)
            )),
            None => out.push_str(&html_escape(&item.name)),
        }
        if !item.children.is_empty() {
            render_menu(&item.children, out);
        }
        out.push_str("</li>");
    }
    out.push_str("</ul>");
}

/// A safe external id: `module.name` using only identifier characters, so
/// it is a single URL path segment with no traversal or query injection.
fn is_xml_id(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Minimal HTML escaping for text interpolated into pages.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `GET /web` — the client, when an addon ships one; otherwise a
/// server-rendered index of the stored views.
async fn web_index(State(service): State<OrmService>, headers: HeaderMap) -> Response {
    let session = current_session(&service, &headers);
    // The shell carries no record data — only the session the client
    // reads to decide what to draw. Serving it to an anonymous visitor
    // (uid null, like `/web/session/get_session_info` answers) is what
    // lets the client draw its own login screen, as Odoo's /web/login does.
    if let Some(shell) = crate::shell::client_shell(&service, session.as_ref()).await {
        return Html(shell).into_response();
    }
    if service.require_auth && session.is_none() {
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            Html("<h1>401</h1><p>log in at /web/session/authenticate</p>".to_string()),
        )
            .into_response();
    }
    match service.list_views(session.as_ref()).await {
        Ok(views) => {
            let mut body = String::from(
                "<!doctype html><meta charset=\"utf-8\"><title>rusdoo</title>\
                 <div style=\"font-family:system-ui;max-width:640px;margin:3rem auto\">\
                 <h1>&#129408; rusdoo</h1>",
            );
            // the menu tree, when present, is the primary navigation
            match service.menu_tree(session.as_ref()).await {
                Ok(menu) if !menu.is_empty() => {
                    body.push_str("<p>Menu:</p>");
                    render_menu(&menu, &mut body);
                }
                Ok(_) => {}
                Err(err) => tracing::warn!("menu_tree failed: {}", err.message),
            }
            body.push_str("<p>Available views:</p><ul>");
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
                Html("<h1>error</h1><p>the views could not be listed</p>".to_string()),
            )
                .into_response()
        }
    }
}
