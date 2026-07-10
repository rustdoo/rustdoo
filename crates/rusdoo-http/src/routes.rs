//! HTTP endpoints, mirroring the routes of `odoo/http.py` used by the
//! web client and classic RPC clients.

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Map, Value};

use crate::dispatch::{OrmService, RpcError};
use crate::jsonrpc::{
    JsonRpcRequest, JsonRpcResponse, INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND,
};

pub fn router(service: OrmService) -> Router {
    Router::new()
        .route("/web/dataset/call_kw", post(call_kw))
        .route("/jsonrpc", post(jsonrpc_endpoint))
        .with_state(service)
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
    Json(body): Json<Value>,
) -> Json<JsonRpcResponse> {
    let request = match parse_envelope(body) {
        Ok(request) => request,
        Err(error) => return Json(error),
    };
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
    let outcome = service.call_kw(model, method, &args, &kwargs).await;
    respond(request.id, outcome)
}

/// `/jsonrpc` — classic RPC: `{service: "object", method: "execute_kw",
/// args: [db, uid, password, model, method, args, kwargs?]}`.
/// Authentication is not implemented yet: db/uid/password are ignored.
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
    let outcome = service.call_kw(model, method, &args, &kwargs).await;
    respond(request.id, outcome)
}
