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
        json!({
            "uid": session.uid,
            // res.groups membership beyond the superuser bypass is not
            // modeled yet, so system/admin means exactly "is the superuser"
            "is_system": is_superuser,
            "is_admin": is_superuser,
            "is_public": false,
            "is_internal_user": true,
            // the context every call inherits. lang/tz live on res.users in
            // Odoo; until those fields exist, the server default is what
            // every record was written with
            "user_context": {"lang": "en_US", "tz": false, "uid": session.uid},
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
