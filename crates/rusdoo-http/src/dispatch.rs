//! ORM method dispatch: the server side of `call_kw`, mirroring the
//! endpoints the Odoo web client uses on `odoo.models.BaseModel`.

use rusdoo_core::RusdooError;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::{parse_domain, Domain};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use std::sync::Arc;

use crate::jsonrpc::{INVALID_PARAMS, METHOD_NOT_FOUND, SERVER_ERROR};

#[derive(Debug)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    fn method_not_found(method: &str) -> Self {
        RpcError {
            code: METHOD_NOT_FOUND,
            message: format!("method not found: {method}"),
        }
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        RpcError {
            code: INVALID_PARAMS,
            message: message.into(),
        }
    }
}

impl From<RusdooError> for RpcError {
    fn from(err: RusdooError) -> Self {
        RpcError {
            code: SERVER_ERROR,
            message: err.to_string(),
        }
    }
}

#[derive(Clone)]
pub struct OrmService {
    pub(crate) registry: Arc<Registry>,
    pub(crate) pool: PgPool,
    pub(crate) sessions: crate::session::SessionStore,
    pub(crate) require_auth: bool,
}

impl OrmService {
    /// Production construction: every ORM endpoint requires a session.
    pub fn new(registry: Arc<Registry>, pool: PgPool) -> Self {
        OrmService {
            registry,
            pool,
            sessions: crate::session::SessionStore::new(),
            require_auth: true,
        }
    }

    /// No authentication — tests and trusted tooling only.
    pub fn insecure(registry: Arc<Registry>, pool: PgPool) -> Self {
        OrmService {
            require_auth: false,
            ..Self::new(registry, pool)
        }
    }

    /// Classic RPC credentials: uid + password verified per call.
    pub(crate) async fn check_credentials(&self, uid: i64, password: &str) -> bool {
        let Ok(rows) = self
            .registry
            .read(&self.pool, "res.users", &[uid], &["password"])
            .await
        else {
            return false;
        };
        let Some(hashed) = rows.first().and_then(|r| r["password"].as_str()) else {
            return false;
        };
        crate::session::verify_password(password, hashed)
    }

    /// Dispatch an ORM method call, Odoo's `call_kw`.
    pub async fn call_kw(
        &self,
        model: &str,
        method: &str,
        args: &[Value],
        kwargs: &Map<String, Value>,
    ) -> Result<Value, RpcError> {
        match method {
            "search" => {
                let domain = self.arg_domain(args.first().or_else(|| kwargs.get("domain")))?;
                let opts = search_options(kwargs)?;
                let ids = self
                    .registry
                    .search(&self.pool, model, &domain, &opts)
                    .await?;
                Ok(json!(ids))
            }
            "search_count" => {
                let domain = self.arg_domain(args.first().or_else(|| kwargs.get("domain")))?;
                let ids = self
                    .registry
                    .search(&self.pool, model, &domain, &SearchOptions::default())
                    .await?;
                Ok(json!(ids.len()))
            }
            "search_read" => {
                let domain = self.arg_domain(args.first().or_else(|| kwargs.get("domain")))?;
                let fields = parse_fields(args.get(1).or_else(|| kwargs.get("fields")))?;
                let opts = search_options(kwargs)?;
                let ids = self
                    .registry
                    .search(&self.pool, model, &domain, &opts)
                    .await?;
                if ids.is_empty() {
                    return Ok(json!([]));
                }
                let names: Vec<&str> = fields.iter().map(String::as_str).collect();
                let rows = self.registry.read(&self.pool, model, &ids, &names).await?;
                Ok(json!(rows))
            }
            "read" => {
                let ids = parse_ids(args.first())?;
                let fields = parse_fields(args.get(1).or_else(|| kwargs.get("fields")))?;
                let names: Vec<&str> = fields.iter().map(String::as_str).collect();
                let rows = self.registry.read(&self.pool, model, &ids, &names).await?;
                Ok(json!(rows))
            }
            "create" => {
                let values = parse_values(args.first())?;
                let pairs: Vec<(&str, Value)> = values
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.clone()))
                    .collect();
                let id = self.registry.create(&self.pool, model, pairs).await?;
                Ok(json!(id))
            }
            "write" => {
                let ids = parse_ids(args.first())?;
                let values = parse_values(args.get(1))?;
                let pairs: Vec<(&str, Value)> = values
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.clone()))
                    .collect();
                self.registry.write(&self.pool, model, &ids, pairs).await?;
                Ok(json!(true))
            }
            "unlink" => {
                let ids = parse_ids(args.first())?;
                let m = self.registry.get(model).ok_or_else(|| {
                    RpcError::from(RusdooError::Validation(format!("unknown model: {model}")))
                })?;
                m.unlink(&self.pool, &ids).await?;
                Ok(json!(true))
            }
            other => Err(RpcError::method_not_found(other)),
        }
    }

    fn arg_domain(&self, raw: Option<&Value>) -> Result<Domain, RpcError> {
        match raw {
            None => Ok(Domain::True),
            Some(value) => Ok(parse_domain(value)?),
        }
    }
}

fn search_options(kwargs: &Map<String, Value>) -> Result<SearchOptions, RpcError> {
    let mut opts = SearchOptions::default();
    if let Some(limit) = kwargs.get("limit") {
        opts.limit = limit.as_u64();
    }
    if let Some(offset) = kwargs.get("offset") {
        opts.offset = offset.as_u64();
    }
    if let Some(order) = kwargs.get("order").and_then(Value::as_str) {
        opts.order = Some(order.to_string());
    }
    Ok(opts)
}

fn parse_ids(raw: Option<&Value>) -> Result<Vec<i64>, RpcError> {
    let ids: Option<Vec<i64>> = match raw {
        Some(Value::Array(items)) => items.iter().map(Value::as_i64).collect(),
        Some(Value::Number(n)) => n.as_i64().map(|id| vec![id]),
        _ => None,
    };
    ids.ok_or_else(|| RpcError::invalid_params("expected record ids"))
}

fn parse_fields(raw: Option<&Value>) -> Result<Vec<String>, RpcError> {
    match raw {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| RpcError::invalid_params("field names must be strings"))
            })
            .collect(),
        Some(_) => Err(RpcError::invalid_params("fields must be a list")),
    }
}

fn parse_values(raw: Option<&Value>) -> Result<Map<String, Value>, RpcError> {
    match raw {
        Some(Value::Object(map)) => Ok(map.clone()),
        _ => Err(RpcError::invalid_params("expected a values object")),
    }
}
