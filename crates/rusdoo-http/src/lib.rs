//! rusdoo-http — port of `odoo/http.py`: axum router, JSON-RPC 2.0
//! dispatch (`/jsonrpc`, `/web/dataset/call_kw`).
//!
//! Not yet ported: sessions, authentication, multi-database routing.

pub mod dispatch;
pub mod jsonrpc;
pub mod routes;
pub mod session;

use dispatch::OrmService;

/// Bind and serve the JSON-RPC endpoints.
pub async fn serve(addr: &str, service: OrmService) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, routes::router(service)).await
}
