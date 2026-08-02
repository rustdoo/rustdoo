//! What the Owl client asks for before it can draw anything:
//! `/web/session/get_session_info` (`odoo/addons/web/models/ir_http.py`)
//! and `/web/webclient/load_menus`
//! (`odoo/addons/web/models/ir_ui_menu.py::load_web_menus`).
//!
//! Both answer with the subset rusdoo can honestly fill. Keys backed by
//! models that are not ported yet (user settings, currencies, config
//! parameters) are left out rather than invented — a fabricated value
//! would be indistinguishable from a real one to the client.

use crate::dispatch::{MenuItem, OrmService, RpcError};
use crate::session::{Session, SUPERUSER_ID};
use serde_json::{json, Map, Value};

/// The Odoo release whose JSON-RPC protocol this server speaks. It is
/// what the port targets, and what the web client gates its features on.
const PROTOCOL_VERSION: &str = "19.0";

/// Odoo's own defaults for the two limits the client reads out of
/// `ir.config_parameter`, which is not ported yet.
/// The language a user without one falls back to, like Odoo's default.
const DEFAULT_LANG: &str = "en_US";

const MAX_FILE_UPLOAD_SIZE: i64 = 128 * 1024 * 1024;
const ACTIVE_IDS_LIMIT: i64 = 20_000;

impl OrmService {
    /// `/web/session/get_session_info`: who the client is talking as.
    pub async fn session_info(&self, session: Option<&Session>) -> Value {
        let Some(session) = session else {
            // Odoo answers an anonymous session with uid null and an empty
            // context; the client then routes to the login page
            return json!({
                "uid": Value::Null,
                "is_system": false,
                "is_admin": false,
                "is_public": true,
                "is_internal_user": false,
                "user_context": {},
                "db": self.database_name(),
                "server_version": PROTOCOL_VERSION,
                "server_version_info": version_info(),
            });
        };
        let is_superuser = session.uid == SUPERUSER_ID;
        let (lang, tz) = self.user_locale(session.uid).await;
        json!({
            "uid": session.uid,
            // res.groups membership beyond the superuser bypass is not
            // modeled yet, so system/admin means exactly "is the superuser"
            "is_system": is_superuser,
            "is_admin": is_superuser,
            "is_public": false,
            "is_internal_user": true,
            // the context every call inherits, read off the user like
            // Odoo's context_get. A model without the fields falls back to
            // the server default rather than claiming a language nobody set
            "user_context": {"lang": lang, "tz": tz, "uid": session.uid},
            "db": self.database_name(),
            "server_version": PROTOCOL_VERSION,
            "server_version_info": version_info(),
            "name": session.login,
            "username": session.login,
            "partner_id": Value::Null,
            "max_file_upload_size": MAX_FILE_UPLOAD_SIZE,
            "active_ids_limit": ACTIVE_IDS_LIMIT,
        })
    }

    /// The user's language and timezone, as the client's context carries
    /// them. Unset (or unmodelled) means the server default for the
    /// language and no timezone — `false`, like Odoo, rather than a
    /// timezone the user never chose.
    async fn user_locale(&self, uid: i64) -> (String, Value) {
        let has = |name: &str| {
            self.registry
                .get("res.users")
                .and_then(|m| m.field(name))
                .is_some()
        };
        let wanted: Vec<&str> = ["lang", "tz"].into_iter().filter(|f| has(f)).collect();
        if wanted.is_empty() {
            return (DEFAULT_LANG.to_string(), Value::Bool(false));
        }
        let row = self
            .registry
            .read(&self.pool, "res.users", &[uid], &wanted)
            .await
            .ok()
            .and_then(|rows| rows.into_iter().next());
        let lang = row
            .as_ref()
            .and_then(|r| r.get("lang"))
            .and_then(Value::as_str)
            .filter(|lang| !lang.is_empty())
            .unwrap_or(DEFAULT_LANG)
            .to_string();
        let tz = row
            .as_ref()
            .and_then(|r| r.get("tz"))
            .and_then(Value::as_str)
            .filter(|tz| !tz.is_empty())
            .map_or(Value::Bool(false), Value::from);
        (lang, tz)
    }

    /// The database the pool is connected to, for the `db` the client
    /// echoes back in every request.
    fn database_name(&self) -> String {
        self.pool
            .connect_options()
            .get_database()
            .unwrap_or_default()
            .to_string()
    }

    /// `/web/webclient/load_menus`: every menu the user may see, flat and
    /// keyed by id, plus the synthetic `root` entry holding the apps.
    ///
    /// Visibility comes from [`OrmService::menu_tree`], so a menu whose
    /// action targets a model the user cannot read never reaches here.
    pub async fn web_menus(&self, session: Option<&Session>) -> Result<Value, RpcError> {
        let forest = self.menu_tree(session).await?;
        let xml_ids = self.menu_xml_ids().await?;
        let mut menus = Map::new();
        let mut roots = Vec::new();
        for item in &forest {
            roots.push(json!(item.id));
            self.flatten_menu(item, item.id, &xml_ids, &mut menus).await;
        }
        menus.insert(
            "root".into(),
            json!({
                "id": "root",
                "name": "root",
                "children": roots,
                "appID": false,
                "xmlid": "",
                "actionID": false,
                "actionModel": false,
                "actionPath": false,
                "webIcon": Value::Null,
                "webIconData": Value::Null,
                "webIconDataMimetype": Value::Null,
            }),
        );
        Ok(Value::Object(menus))
    }

    /// Add `item` and its descendants to `out`. `app` is the top-level
    /// ancestor: the client groups every menu under the app it belongs to.
    fn flatten_menu<'a>(
        &'a self,
        item: &'a MenuItem,
        app: i64,
        xml_ids: &'a std::collections::HashMap<i64, String>,
        out: &'a mut Map<String, Value>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            // an app opens the action of its first descendant that has one,
            // like load_web_menus — the app menu itself rarely carries one
            let action = match &item.action {
                Some(action) => Some(action.clone()),
                None if item.id == app => first_action(item),
                None => None,
            };
            let action_id = match &action {
                Some(xml_id) => self.action_id(xml_id).await,
                None => None,
            };
            out.insert(
                item.id.to_string(),
                json!({
                    "id": item.id,
                    "name": item.name,
                    "children": item.children.iter().map(|c| json!(c.id)).collect::<Vec<_>>(),
                    "appID": app,
                    "xmlid": xml_ids.get(&item.id).cloned().unwrap_or_default(),
                    // an action that cannot be resolved is reported as
                    // absent, never as a numeric id the client would call
                    "actionID": action_id.map_or(Value::Bool(false), Value::from),
                    "actionModel": action_id
                        .map_or(Value::Bool(false), |_| json!("ir.actions.act_window")),
                    "actionPath": false,
                    // web_icon* live on ir.ui.menu in Odoo; not modeled yet
                    "webIcon": Value::Null,
                    "webIconData": Value::Null,
                    "webIconDataMimetype": Value::Null,
                }),
            );
            for child in &item.children {
                self.flatten_menu(child, app, xml_ids, out).await;
            }
        })
    }

    /// External id of every `ir.ui.menu` row, for the `xmlid` the client
    /// uses to address menus.
    async fn menu_xml_ids(&self) -> Result<std::collections::HashMap<i64, String>, RpcError> {
        let rows: Vec<(String, String, i32)> = sqlx::query_as(
            r#"SELECT "module", "name", "res_id" FROM "ir_model_data"
               WHERE "model" = 'ir.ui.menu'"#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| RpcError {
            code: crate::jsonrpc::SERVER_ERROR,
            message: e.to_string(),
        })?;
        Ok(rows
            .into_iter()
            .map(|(module, name, res_id)| (i64::from(res_id), format!("{module}.{name}")))
            .collect())
    }
}

impl OrmService {
    /// `get_views` (`odoo/addons/base/models/ir_ui_view.py`): the arch of
    /// each requested view, plus the fields the client needs to render
    /// them. This is what an action load calls before its first search.
    ///
    /// Deviation: Odoo returns only the fields the arch mentions; until
    /// the arch is parsed for field usage, the model's whole `fields_get`
    /// is returned — a superset, and one that already hides private
    /// fields.
    pub(crate) async fn get_views_payload(
        &self,
        uid: i64,
        model: &str,
        specs: &[(Option<i64>, String)],
    ) -> Result<Value, RpcError> {
        if specs.is_empty() {
            return Err(RpcError::invalid_params(
                "get_views needs at least one [view_id, view_type]",
            ));
        }
        // the views themselves are records: reading them is an ACL check
        // of its own, on top of the one the dispatch gate ran for `model`
        if self.require_auth {
            let ident = self.identity(uid).await;
            self.check_access("ir.ui.view", "read", &ident)?;
        }
        let mut views = Map::new();
        for (view_id, kind) in specs {
            views.insert(kind.clone(), self.find_view(model, *view_id, kind).await?);
        }
        let fields = self.fields_metadata(model, &std::collections::HashSet::new())?;
        Ok(json!({
            "views": views,
            "models": {model: {"fields": Value::Object(fields)}},
        }))
    }

    /// The view of `kind` for `model`: the one asked for by id, or the
    /// model's lowest-priority one of that type (ties broken by id, so
    /// the answer is stable). A view asked for by id must really belong
    /// to this model and type — answering with someone else's arch would
    /// render the wrong screen.
    async fn find_view(
        &self,
        model: &str,
        view_id: Option<i64>,
        kind: &str,
    ) -> Result<Value, RpcError> {
        let row: Option<ViewRow> = match view_id {
            Some(id) => sqlx::query_as(
                r#"SELECT "id", "model", "type", "arch" FROM "ir_ui_view" WHERE "id" = $1"#,
            )
            .bind(id as i32)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_error)?,
            None => sqlx::query_as(
                r#"SELECT "id", "model", "type", "arch" FROM "ir_ui_view"
                   WHERE "model" = $1 AND "type" = $2
                   ORDER BY "priority" NULLS LAST, "id" LIMIT 1"#,
            )
            .bind(model)
            .bind(kind)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_error)?,
        };
        let Some((id, view_model, view_type, arch)) = row else {
            return Err(RpcError::invalid_params(match view_id {
                Some(id) => format!("no ir.ui.view with id {id}"),
                None => format!("no {kind} view for model {model}"),
            }));
        };
        if view_model.as_deref() != Some(model) {
            return Err(RpcError::invalid_params(format!(
                "view {id} belongs to model {:?}, not {model}",
                view_model.unwrap_or_default()
            )));
        }
        if view_type.as_deref() != Some(kind) {
            return Err(RpcError::invalid_params(format!(
                "view {id} is a {:?} view, not a {kind} one",
                view_type.unwrap_or_default()
            )));
        }
        Ok(json!({
            "id": id,
            "model": model,
            "type": kind,
            "arch": arch.unwrap_or_default(),
        }))
    }
}

/// The view types an act_window falls back to when it declares none.
const DEFAULT_VIEW_MODE: &str = "list,form";

impl OrmService {
    /// `/web/action/load` (`odoo/addons/web/controllers/action.py`): the
    /// action record a menu click opens, addressed by database id or by
    /// external id.
    ///
    /// The client renders whatever comes back, so this is a read of a
    /// record like any other: it goes through the same ACL the ORM
    /// endpoints use, and it never invents a model the caller may not
    /// see.
    pub async fn load_action(
        &self,
        reference: &Value,
        session: Option<&Session>,
    ) -> Result<Value, RpcError> {
        if let Some(session) = session {
            self.check_access("ir.actions.act_window", "read", session)?;
        }
        let id = match reference {
            Value::Number(number) => number.as_i64().ok_or_else(|| {
                RpcError::invalid_params("action_id must be an integer or an external id")
            })?,
            Value::String(xml_id) => {
                let (module, name) = xml_id.split_once('.').ok_or_else(|| {
                    RpcError::invalid_params("an action external id reads module.name")
                })?;
                self.resolve_action_id(module, name).await?
            }
            _ => {
                return Err(RpcError::invalid_params(
                    "action_id must be an integer or an external id",
                ))
            }
        };
        let rows = self
            .registry
            .read(
                &self.pool,
                "ir.actions.act_window",
                &[id],
                &["name", "res_model", "view_mode", "domain"],
            )
            .await?;
        let action = rows
            .first()
            .ok_or_else(|| RpcError::invalid_params(format!("no action with id {id}")))?;
        let res_model = action
            .get("res_model")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if res_model.is_empty() {
            return Err(RpcError::invalid_params(format!(
                "action {id} has no res_model"
            )));
        }
        // reading the action must not become a way around the ACL of the
        // model it opens: a user who cannot read the model gets the same
        // answer as for an action that does not exist
        if let Some(session) = session {
            self.check_access(res_model, "read", session)?;
        }
        let view_mode = action
            .get("view_mode")
            .and_then(Value::as_str)
            .filter(|mode| !mode.trim().is_empty())
            .unwrap_or(DEFAULT_VIEW_MODE);
        let views: Vec<Value> = view_mode
            .split(',')
            .map(str::trim)
            .filter(|kind| !kind.is_empty())
            .map(|kind| json!([Value::Bool(false), kind]))
            .collect();
        Ok(json!({
            "id": id,
            "type": "ir.actions.act_window",
            "name": action.get("name").cloned().unwrap_or(Value::Bool(false)),
            "res_model": res_model,
            "view_mode": view_mode,
            "views": views,
            // the domain the client sends back on every search of this
            // action, already a domain rather than the text the record
            // holds — an action whose domain cannot be read is an error,
            // never an unscoped list
            "domain": action_domain(action.get("domain").and_then(Value::as_str))?,
            "context": json!({}),
            "target": "current",
        }))
    }

    /// External id -> `ir.actions.act_window` id, without leaking whether
    /// the id exists under another model.
    async fn resolve_action_id(&self, module: &str, name: &str) -> Result<i64, RpcError> {
        let row: Option<(i32,)> = sqlx::query_as(
            r#"SELECT "res_id" FROM "ir_model_data"
               WHERE "module" = $1 AND "name" = $2 AND "model" = 'ir.actions.act_window'"#,
        )
        .bind(module)
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_error)?;
        row.map(|(id,)| i64::from(id))
            .ok_or_else(|| RpcError::invalid_params(format!("unknown action {module}.{name}")))
    }
}

/// The domain of an act_window as the client receives it: a real domain,
/// checked here so a malformed one fails the action load instead of
/// silently listing every row of the model.
fn action_domain(raw: Option<&str>) -> Result<Value, RpcError> {
    let trimmed = raw.unwrap_or("").trim();
    if trimmed.is_empty() || trimmed == "[]" {
        return Ok(json!([]));
    }
    let value: Value = serde_json::from_str(trimmed).map_err(|_| {
        RpcError::invalid_params("action domain is not a JSON domain; refusing to open it unscoped")
    })?;
    rusdoo_orm::domain::parse_domain(&value).map_err(RpcError::from)?;
    Ok(value)
}

/// Parse the `views` argument: `[[view_id_or_false, "type"], ...]`.
pub(crate) fn parse_view_specs(
    raw: Option<&Value>,
) -> Result<Vec<(Option<i64>, String)>, RpcError> {
    let malformed =
        || RpcError::invalid_params("views must be a list of [view_id, view_type] pairs");
    let Some(Value::Array(items)) = raw else {
        return Err(malformed());
    };
    items
        .iter()
        .map(|item| {
            let Some(pair) = item.as_array() else {
                return Err(malformed());
            };
            let kind = pair
                .get(1)
                .and_then(Value::as_str)
                .filter(|kind| !kind.is_empty())
                .ok_or_else(malformed)?;
            // false/null is Odoo's "pick the default view of this type"
            let id = match pair.first() {
                None | Some(Value::Null) | Some(Value::Bool(false)) => None,
                Some(value) => Some(value.as_i64().ok_or_else(malformed)?),
            };
            Ok((id, kind.to_string()))
        })
        .collect()
}

/// An `ir.ui.view` row as the lookup reads it: (id, model, type, arch),
/// every text column nullable because the table is data, not a schema.
type ViewRow = (i32, Option<String>, Option<String>, Option<String>);

fn db_error(e: sqlx::Error) -> RpcError {
    RpcError {
        code: crate::jsonrpc::SERVER_ERROR,
        message: e.to_string(),
    }
}

/// The action of the first descendant that has one, depth first — what an
/// app menu opens when clicked.
fn first_action(item: &MenuItem) -> Option<String> {
    for child in &item.children {
        if let Some(action) = &child.action {
            return Some(action.clone());
        }
        if let Some(action) = first_action(child) {
            return Some(action);
        }
    }
    None
}

fn version_info() -> Value {
    json!([19, 0, 0, "final", 0, ""])
}
