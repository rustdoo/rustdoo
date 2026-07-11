//! ORM method dispatch: the server side of `call_kw`, mirroring the
//! endpoints the Odoo web client uses on `odoo.models.BaseModel`.

use rusdoo_core::RusdooError;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::{parse_domain, Domain};
use rusdoo_orm::fields::FieldType;
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Semaphore;

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

/// Bounds concurrent Argon2id verifications so a flood of bad logins
/// cannot exhaust memory (each verify is memory-hard).
const MAX_CONCURRENT_VERIFY: usize = 8;

/// Hard cap on rows rendered into a single view page.
const VIEW_RECORD_LIMIT: u64 = 200;

#[derive(Clone)]
pub struct OrmService {
    pub(crate) registry: Arc<Registry>,
    pub(crate) pool: PgPool,
    pub(crate) sessions: crate::session::SessionStore,
    pub(crate) require_auth: bool,
    pub(crate) secure_cookies: bool,
    pub(crate) verify_gate: Arc<Semaphore>,
    pub(crate) access: Arc<rusdoo_orm::access::AccessControl>,
}

impl OrmService {
    /// Production construction: every ORM endpoint requires a session.
    pub fn new(registry: Arc<Registry>, pool: PgPool) -> Self {
        OrmService {
            registry,
            pool,
            sessions: crate::session::SessionStore::new(),
            require_auth: true,
            secure_cookies: true,
            verify_gate: Arc::new(Semaphore::new(MAX_CONCURRENT_VERIFY)),
            access: Arc::new(rusdoo_orm::access::AccessControl::new()),
        }
    }

    /// Install the access-control table (`ir.model.access`).
    pub fn with_access(mut self, access: rusdoo_orm::access::AccessControl) -> Self {
        self.access = Arc::new(access);
        self
    }

    /// Enforce `ir.model.access` for the operation implied by `method`.
    /// Fail-closed: a method with no CRUD mapping is denied for
    /// non-superusers (a future custom-method dispatch must map it or be
    /// explicitly allowlisted, never silently bypass the ACL).
    pub(crate) fn check_access(
        &self,
        model: &str,
        method: &str,
        session: &crate::session::Session,
    ) -> Result<(), RpcError> {
        let Some(op) = rusdoo_orm::access::Operation::for_method(method) else {
            if session.is_superuser {
                return Ok(());
            }
            return Err(RpcError {
                code: crate::jsonrpc::SERVER_ERROR,
                message: format!("method {method:?} is not permitted on {model}"),
            });
        };
        self.access
            .check(model, op, &session.groups, session.is_superuser)
            .map_err(|e| RpcError {
                code: crate::jsonrpc::SERVER_ERROR,
                message: e.to_string(),
            })
    }

    /// List every `ir.ui.view` as (external id, display name), for a
    /// navigable index page. ACL-checked when a session is present.
    pub(crate) async fn list_views(
        &self,
        session: Option<&crate::session::Session>,
    ) -> Result<Vec<(String, String)>, RpcError> {
        if let Some(s) = session {
            self.check_access("ir.ui.view", "read", s)?;
        }
        let rows: Vec<(String, String, i32)> = sqlx::query_as(
            r#"SELECT "module", "name", "res_id" FROM "ir_model_data"
               WHERE "model" = 'ir.ui.view' ORDER BY "module", "name""#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| RpcError {
            code: crate::jsonrpc::SERVER_ERROR,
            message: e.to_string(),
        })?;
        let ids: Vec<i64> = rows.iter().map(|(_, _, r)| i64::from(*r)).collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let recs = self
            .registry
            .read(&self.pool, "ir.ui.view", &ids, &["name"])
            .await?;
        let name_by_id: HashMap<i64, String> = recs
            .iter()
            .filter_map(|r| Some((r.get("id")?.as_i64()?, r.get("name")?.as_str()?.to_string())))
            .collect();
        Ok(rows
            .into_iter()
            .map(|(m, n, rid)| {
                let xml_id = format!("{m}.{n}");
                let name = name_by_id
                    .get(&i64::from(rid))
                    .cloned()
                    .unwrap_or_else(|| xml_id.clone());
                (xml_id, name)
            })
            .collect())
    }

    /// Render an `ir.ui.view` (resolved by external id) to HTML: read the
    /// view's arch, gather its target model's records as the context, and
    /// run them through QWeb. The Rust side of an Odoo QWeb report/list.
    pub(crate) async fn render_view(
        &self,
        xml_id: &str,
        session: Option<&crate::session::Session>,
    ) -> Result<String, RpcError> {
        // read access on the view record itself
        if let Some(s) = session {
            self.check_access("ir.ui.view", "read", s)?;
        }
        let (module, name) = xml_id
            .split_once('.')
            .ok_or_else(|| RpcError::invalid_params("xml_id must be module.name"))?;
        let res_id = self.resolve_external_id(module, name, "ir.ui.view").await?;
        let views = self
            .registry
            .read(
                &self.pool,
                "ir.ui.view",
                &[res_id],
                &["name", "model", "arch"],
            )
            .await?;
        let view = views.first().ok_or_else(|| RpcError {
            code: crate::jsonrpc::SERVER_ERROR,
            message: "view record vanished".into(),
        })?;
        let arch = view.get("arch").and_then(Value::as_str).unwrap_or("");
        let target = view.get("model").and_then(Value::as_str).unwrap_or("");
        let title = view.get("name").and_then(Value::as_str).unwrap_or("");
        self.render_arch(arch, target, title, &Domain::True, session)
            .await
    }

    /// Render a view arch against its target model's records: gather the
    /// model's data fields as context and run QWeb. Shared by `render_view`
    /// (view by external id) and `render_action` (view chosen by an action).
    async fn render_arch(
        &self,
        arch: &str,
        target: &str,
        title: &str,
        domain: &Domain,
        session: Option<&crate::session::Session>,
    ) -> Result<String, RpcError> {
        let mut ctx = Map::new();
        ctx.insert("title".into(), Value::from(title));
        // the caller must be allowed to read the model the view renders
        if let (Some(s), false) = (session, target.is_empty()) {
            self.check_access(target, "read", s)?;
        }
        if let Some(m) = self.registry.get(target) {
            // a generic list shows data fields, not the system audit
            // columns (readonly) — this also avoids pulling create_uid/
            // write_uid (m2o -> res.users) and their name-resolution
            // round-trips into every auto-rendered view
            let names: Vec<&str> = m
                .fields()
                .iter()
                .filter(|f| f.stored && f.exposed && !f.readonly && is_readable_scalar(&f.ty))
                .map(|f| f.name.as_str())
                .collect();
            // cap the rows a single page renders (no full-table dumps)
            let opts = SearchOptions {
                limit: Some(VIEW_RECORD_LIMIT),
                ..SearchOptions::default()
            };
            let ids = self
                .registry
                .search(&self.pool, target, domain, &opts)
                .await?;
            let records = self.registry.read(&self.pool, target, &ids, &names).await?;
            ctx.insert("records".into(), json!(records));
        }
        // Load only the templates actually referenced by t-call (the
        // transitive closure), and access-check each called view's own
        // model — never preload every arch (leak + full-table read).
        let templates = self.collect_templates(arch, session).await?;
        rusdoo_qweb::render_with(arch, &Value::Object(ctx), &templates).map_err(|e| RpcError {
            code: crate::jsonrpc::SERVER_ERROR,
            message: e.to_string(),
        })
    }

    /// Render an `ir.actions.act_window` (by external id): read its target
    /// model, pick a view for that model, and render it — Odoo's "open an
    /// action opens the model in a view" navigation step.
    pub(crate) async fn render_action(
        &self,
        xml_id: &str,
        session: Option<&crate::session::Session>,
    ) -> Result<String, RpcError> {
        if let Some(s) = session {
            self.check_access("ir.actions.act_window", "read", s)?;
        }
        let (module, name) = xml_id
            .split_once('.')
            .ok_or_else(|| RpcError::invalid_params("xml_id must be module.name"))?;
        let res_id = self
            .resolve_external_id(module, name, "ir.actions.act_window")
            .await?;
        let actions = self
            .registry
            .read(
                &self.pool,
                "ir.actions.act_window",
                &[res_id],
                &["name", "res_model", "domain"],
            )
            .await?;
        let action = actions.first().ok_or_else(|| RpcError {
            code: crate::jsonrpc::SERVER_ERROR,
            message: "action record vanished".into(),
        })?;
        let title = action.get("name").and_then(Value::as_str).unwrap_or("");
        let res_model = action
            .get("res_model")
            .and_then(Value::as_str)
            .unwrap_or("");
        // apply the action's domain: it scopes the rows a user may see, so
        // ignoring it would over-expose data (a "My X" action relies on it)
        let domain = parse_action_domain(action.get("domain").and_then(Value::as_str))?;
        // pick a view for the model. NOTE: view_mode and view type/priority
        // are not yet honored — we render the lowest-id view (deterministic
        // but arbitrary between a list and a form). Tracked for follow-up.
        let view: Option<(String,)> = sqlx::query_as(
            r#"SELECT "arch" FROM "ir_ui_view" WHERE "model" = $1 ORDER BY "id" LIMIT 1"#,
        )
        .bind(res_model)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| RpcError {
            code: crate::jsonrpc::SERVER_ERROR,
            message: e.to_string(),
        })?;
        let (arch,) = view.ok_or_else(|| RpcError {
            code: crate::jsonrpc::SERVER_ERROR,
            message: format!("no view for model {res_model:?}"),
        })?;
        self.render_arch(&arch, res_model, title, &domain, session)
            .await
    }

    /// Resolve an external id to its `res_id`, asserting the record's model.
    async fn resolve_external_id(
        &self,
        module: &str,
        name: &str,
        expected_model: &str,
    ) -> Result<i64, RpcError> {
        let row: Option<(String, i32)> = sqlx::query_as(
            r#"SELECT "model", "res_id" FROM "ir_model_data" WHERE "module" = $1 AND "name" = $2"#,
        )
        .bind(module)
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| RpcError {
            code: crate::jsonrpc::SERVER_ERROR,
            message: e.to_string(),
        })?;
        let (model, res_id) = row.ok_or_else(|| RpcError {
            code: crate::jsonrpc::SERVER_ERROR,
            message: format!("unknown external id: {module}.{name}"),
        })?;
        if model != expected_model {
            return Err(RpcError::invalid_params(format!(
                "{module}.{name} is a {model}, not an {expected_model}"
            )));
        }
        Ok(i64::from(res_id))
    }

    /// The `ir.ui.menu` tree as nested items for the navigation index:
    /// each item carries its display name and, on leaves, the external id
    /// of the action it opens. Ordered by (parent, sequence).
    pub(crate) async fn menu_tree(
        &self,
        session: Option<&crate::session::Session>,
    ) -> Result<Vec<MenuItem>, RpcError> {
        if let Some(s) = session {
            self.check_access("ir.ui.menu", "read", s)?;
        }
        // (id, name, parent_id, sequence, action external id). NULLS LAST:
        // an unset sequence sorts after explicit ones (Odoo defaults ~10),
        // not to the front of its sibling group.
        let rows: Vec<MenuRow> = sqlx::query_as(
            r#"SELECT "id", "name", "parent_id", "sequence", "action"
                   FROM "ir_ui_menu" ORDER BY "sequence" NULLS LAST, "id""#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| RpcError {
            code: crate::jsonrpc::SERVER_ERROR,
            message: e.to_string(),
        })?;
        // group children by parent, preserving the query's order
        let mut children: HashMap<Option<i64>, Vec<MenuItem>> = HashMap::new();
        for (id, name, parent, _seq, action) in &rows {
            let item = MenuItem {
                id: i64::from(*id),
                name: name.clone().unwrap_or_default(),
                action: action.clone(),
                children: Vec::new(),
            };
            children
                .entry(parent.map(i64::from))
                .or_default()
                .push(item);
        }
        let forest = build_menu_forest(None, &mut children, 0);
        // rows unreachable from a real root (orphaned parent_id or a parent
        // cycle) are dropped — surface that rather than vanish silently
        let dropped: usize = children.values().map(Vec::len).sum();
        if dropped > 0 {
            tracing::warn!("ir.ui.menu: {dropped} row(s) dropped (orphaned parent_id or cycle)");
        }
        // per-item visibility: a user only sees a menu whose action they may
        // open (its target model is readable). Odoo uses ir.ui.menu.groups_id
        // for this; until that field is modeled we gate on the action's
        // target-model read ACL. No session (insecure tooling) => show all.
        let Some(s) = session else {
            return Ok(forest);
        };
        let mut actions = Vec::new();
        collect_menu_actions(&forest, &mut actions);
        let mut allowed: std::collections::HashSet<String> = std::collections::HashSet::new();
        for action in actions {
            if let Some(model) = self.action_res_model(&action).await {
                if self.check_access(&model, "read", s).is_ok() {
                    allowed.insert(action);
                }
            }
        }
        Ok(prune_menu(forest, &allowed))
    }

    /// The target model of an `ir.actions.act_window` external id, or None
    /// if it can't be resolved (used to gate menu visibility).
    async fn action_res_model(&self, xml_id: &str) -> Option<String> {
        let (module, name) = xml_id.split_once('.')?;
        let res_id = self
            .resolve_external_id(module, name, "ir.actions.act_window")
            .await
            .ok()?;
        let recs = self
            .registry
            .read(
                &self.pool,
                "ir.actions.act_window",
                &[res_id],
                &["res_model"],
            )
            .await
            .ok()?;
        recs.first()?.get("res_model")?.as_str().map(String::from)
    }

    /// Resolve the transitive closure of `t-call` targets referenced from
    /// `arch`, reading each referenced view's arch only after checking the
    /// caller may read that view's target model.
    async fn collect_templates(
        &self,
        arch: &str,
        session: Option<&crate::session::Session>,
    ) -> Result<rusdoo_qweb::Templates, RpcError> {
        let mut templates = rusdoo_qweb::Templates::new();
        let mut queue = rusdoo_qweb::t_call_refs(arch).map_err(RpcError::from)?;
        let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
        while let Some(name) = queue.pop() {
            if !visited.insert(name.clone()) {
                continue;
            }
            let Some((module, view_name)) = name.split_once('.') else {
                continue; // malformed ref surfaces at render as unknown template
            };
            let row: Option<(i32,)> = sqlx::query_as(
                r#"SELECT "res_id" FROM "ir_model_data"
                   WHERE "module" = $1 AND "name" = $2 AND "model" = 'ir.ui.view'"#,
            )
            .bind(module)
            .bind(view_name)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| RpcError {
                code: crate::jsonrpc::SERVER_ERROR,
                message: e.to_string(),
            })?;
            let Some((res_id,)) = row else {
                continue;
            };
            let views = self
                .registry
                .read(
                    &self.pool,
                    "ir.ui.view",
                    &[i64::from(res_id)],
                    &["model", "arch"],
                )
                .await?;
            let Some(view) = views.first() else {
                continue;
            };
            let target = view.get("model").and_then(Value::as_str).unwrap_or("");
            // a called view carrying a model requires read access to it;
            // model-less layout templates are shareable
            if let Some(s) = session {
                if !target.is_empty() {
                    self.check_access(target, "read", s)?;
                }
            }
            let sub_arch = view
                .get("arch")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            queue.extend(rusdoo_qweb::t_call_refs(&sub_arch).map_err(RpcError::from)?);
            templates.insert(name, sub_arch);
        }
        Ok(templates)
    }

    /// Resolve a user's `res.groups` ids from the `groups_id` m2m field,
    /// when the res.users model defines it. Empty otherwise (superuser
    /// bypass keeps admin usable until groups are modelled).
    pub(crate) async fn resolve_groups(&self, uid: i64) -> Vec<i64> {
        let has_groups = self
            .registry
            .get("res.users")
            .and_then(|m| m.field("groups_id"))
            .is_some();
        if !has_groups {
            return Vec::new();
        }
        self.registry
            .read(&self.pool, "res.users", &[uid], &["groups_id"])
            .await
            .ok()
            .and_then(|rows| {
                rows.into_iter().next().map(|row| {
                    row.get("groups_id")
                        .and_then(|v| v.as_array())
                        .map(|ids| ids.iter().filter_map(|v| v.as_i64()).collect())
                        .unwrap_or_default()
                })
            })
            .unwrap_or_default()
    }

    /// Build a transient access identity for the classic RPC path, which
    /// verifies credentials per call instead of holding a cookie session.
    pub(crate) async fn identity(&self, uid: i64) -> crate::session::Session {
        crate::session::Session {
            uid,
            login: uid.to_string(),
            is_superuser: uid == crate::session::SUPERUSER_ID,
            groups: self.resolve_groups(uid).await,
        }
    }

    /// No authentication — tests and trusted tooling only.
    #[doc(hidden)]
    pub fn insecure(registry: Arc<Registry>, pool: PgPool) -> Self {
        OrmService {
            require_auth: false,
            ..Self::new(registry, pool)
        }
    }

    /// Allow the session cookie over plain HTTP (local dev only).
    pub fn allow_insecure_cookies(mut self) -> Self {
        self.secure_cookies = false;
        self
    }

    /// Verify a password in constant work whether or not the user exists,
    /// under the concurrency gate. `hash == None` still spends a full
    /// Argon2 verify against a dummy hash, then fails.
    pub(crate) async fn verify(&self, password: &str, hash: Option<&str>) -> bool {
        let _permit = self.verify_gate.acquire().await;
        match hash {
            Some(hash) => crate::session::verify_password(password, hash),
            None => {
                let _ = crate::session::verify_password(password, crate::session::dummy_hash());
                false
            }
        }
    }

    /// Reject a read/search_read requesting a non-exposed field.
    pub(crate) fn ensure_exposed(&self, model: &str, fields: &[String]) -> Result<(), RpcError> {
        let Some(m) = self.registry.get(model) else {
            return Ok(());
        };
        for name in fields {
            if let Some(field) = m.field(name) {
                if !field.exposed {
                    return Err(RpcError::invalid_params(format!(
                        "field {name:?} is not readable"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Classic RPC credentials: uid + password verified per call, in
    /// constant work whether or not the uid exists.
    pub(crate) async fn check_credentials(&self, uid: i64, password: &str) -> bool {
        let hash = self
            .registry
            .read(&self.pool, "res.users", &[uid], &["password"])
            .await
            .ok()
            .and_then(|rows| {
                rows.first()
                    .and_then(|r| r["password"].as_str().map(str::to_string))
            });
        self.verify(password, hash.as_deref()).await
    }

    /// Dispatch an ORM method call, Odoo's `call_kw`.
    pub async fn call_kw(
        &self,
        uid: i64,
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
                self.ensure_exposed(model, &fields)?;
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
                self.ensure_exposed(model, &fields)?;
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
                let id = self
                    .registry
                    .create_as(&self.pool, uid, model, pairs)
                    .await?;
                Ok(json!(id))
            }
            "write" => {
                let ids = parse_ids(args.first())?;
                let values = parse_values(args.get(1))?;
                let pairs: Vec<(&str, Value)> = values
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.clone()))
                    .collect();
                self.registry
                    .write_as(&self.pool, uid, model, &ids, pairs)
                    .await?;
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

/// Field types the view context can safely read (decodable scalars).
fn is_readable_scalar(ty: &FieldType) -> bool {
    matches!(
        ty,
        FieldType::Char { .. }
            | FieldType::Text
            | FieldType::Html
            | FieldType::Integer
            | FieldType::Boolean
            | FieldType::Selection(_)
            | FieldType::Many2one { .. }
            | FieldType::Float { digits: None }
    )
}

/// Parse an `ir.actions.act_window.domain` into a [`Domain`]. An empty or
/// `[]` domain means "everything". Only a JSON-encoded domain is supported;
/// a non-empty Python-expression domain is REFUSED (error) rather than
/// silently ignored — ignoring it would render the model unscoped and
/// over-expose rows the action author meant to filter out.
fn parse_action_domain(raw: Option<&str>) -> Result<Domain, RpcError> {
    let trimmed = raw.unwrap_or("").trim();
    if trimmed.is_empty() || trimmed == "[]" {
        return Ok(Domain::True);
    }
    let value: Value = serde_json::from_str(trimmed).map_err(|_| RpcError {
        code: SERVER_ERROR,
        message: "action domain is not a JSON domain; refusing to render unscoped data".into(),
    })?;
    parse_domain(&value).map_err(RpcError::from)
}

/// A raw `ir.ui.menu` row: (id, name, parent_id, sequence, action xml_id).
type MenuRow = (
    i32,
    Option<String>,
    Option<i32>,
    Option<i32>,
    Option<String>,
);

/// One node of the `ir.ui.menu` navigation tree.
#[derive(Debug, Clone)]
pub struct MenuItem {
    pub id: i64,
    pub name: String,
    /// external id of the action this leaf opens, if any
    pub action: Option<String>,
    pub children: Vec<MenuItem>,
}

/// Guards against a malformed (cyclic) menu parent chain.
const MAX_MENU_DEPTH: usize = 16;

/// Assemble the menu forest under `parent`, draining `by_parent` so each
/// node is placed once; depth-capped against parent cycles.
fn build_menu_forest(
    parent: Option<i64>,
    by_parent: &mut HashMap<Option<i64>, Vec<MenuItem>>,
    depth: usize,
) -> Vec<MenuItem> {
    if depth > MAX_MENU_DEPTH {
        // a legitimately deep tree or a cycle guard trip — either way the
        // subtree below is not rendered, so say so
        tracing::warn!("ir.ui.menu: depth cap {MAX_MENU_DEPTH} reached, subtree truncated");
        return Vec::new();
    }
    let Some(mut items) = by_parent.remove(&parent) else {
        return Vec::new();
    };
    for item in &mut items {
        item.children = build_menu_forest(Some(item.id), by_parent, depth + 1);
    }
    items
}

/// Collect every action external id referenced anywhere in the forest.
fn collect_menu_actions(items: &[MenuItem], out: &mut Vec<String>) {
    for item in items {
        if let Some(action) = &item.action {
            out.push(action.clone());
        }
        collect_menu_actions(&item.children, out);
    }
}

/// Drop menu items the user can't reach: a leaf survives only if its action
/// is in `allowed`; a branch survives only if it keeps a visible child. A
/// label-only item with no surviving child is pruned.
fn prune_menu(items: Vec<MenuItem>, allowed: &std::collections::HashSet<String>) -> Vec<MenuItem> {
    items
        .into_iter()
        .filter_map(|mut item| {
            item.children = prune_menu(item.children, allowed);
            let can_open = item.action.as_ref().is_some_and(|a| allowed.contains(a));
            if can_open || !item.children.is_empty() {
                Some(item)
            } else {
                None
            }
        })
        .collect()
}
