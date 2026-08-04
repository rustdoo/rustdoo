//! What the client is told about itself at boot:
//! `/web/session/get_session_info`, port of
//! `addons/web/models/ir_http.py::session_info`.
//!
//! The answer is the subset this port can fill honestly. Keys backed by
//! models that are not ported (user settings, config parameters) are
//! left out rather than invented — a fabricated value is
//! indistinguishable from a real one to the client, and it is the client
//! that decides what to draw from them.

use crate::dispatch::OrmService;
use crate::session::{Session, SUPERUSER_ID};
use serde_json::{json, Value};

/// The Odoo release whose JSON-RPC protocol this server speaks. It is
/// what the port targets, and what the web client gates its features on.
const PROTOCOL_VERSION: &str = "19.0";

/// The language a user without one falls back to, like Odoo's default.
const DEFAULT_LANG: &str = "en_US";

/// Odoo's own defaults for the two limits the client reads out of
/// `ir.config_parameter`, which is not ported yet.
const MAX_FILE_UPLOAD_SIZE: i64 = 128 * 1024 * 1024;
const ACTIVE_IDS_LIMIT: i64 = 20_000;

/// The view types the client can draw, with the icon and the arity Odoo
/// gives each — port of `addons/web/models/ir_ui_view.py::_get_view_info`
/// and of the `type` selection's labels, which is where the names come
/// from. `qweb` is not among them: Odoo excludes it, because it is a
/// template and not something an action opens.
///
/// The action manager looks every type of an action up in here and
/// refuses the action when one is missing, so this table is what decides
/// whether a menu opens at all.
const VIEW_TYPES: [(&str, &str, &str, bool); 7] = [
    // type, display name, icon, whether it shows many records at once
    ("list", "List", "oi oi-view-list", true),
    ("form", "Form", "fa fa-address-card", false),
    ("graph", "Graph", "fa fa-area-chart", true),
    ("pivot", "Pivot", "oi oi-view-pivot", true),
    ("kanban", "Kanban", "oi oi-view-kanban", true),
    ("calendar", "Calendar", "fa fa-calendar", true),
    ("search", "Search", "oi oi-search", true),
];

/// The release tuple, as `odoo/release.py` spells it.
fn version_info() -> Value {
    json!([19, 0, 0, "final", 0, ""])
}

/// What `/web/webclient/version_info` answers — port of
/// `odoo/service/common.py::exp_version`. `protocol_version` is Odoo's
/// own constant 1, which is the RPC wire this port speaks.
pub(crate) fn version() -> Value {
    json!({
        "server_version": PROTOCOL_VERSION,
        "server_version_info": version_info(),
        "server_serie": PROTOCOL_VERSION,
        "protocol_version": 1,
    })
}

/// What the client knows about view types, keyed by type.
fn view_info() -> Value {
    let mut out = serde_json::Map::new();
    for (kind, display_name, icon, multi_record) in VIEW_TYPES {
        out.insert(
            kind.to_string(),
            json!({
                "display_name": display_name,
                "icon": icon,
                "multi_record": multi_record,
            }),
        );
    }
    json!(out)
}

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
        // `name` is the person, `username` is what they type to log in —
        // Odoo's session_info keeps them apart, and the navbar shows the
        // first. A login in the greeting is what the port used to show.
        let name = self
            .registry
            .read(&self.pool, "res.users", &[session.uid], &["name"])
            .await
            .ok()
            .and_then(|rows| rows.into_iter().next())
            .and_then(|row| row.get("name").and_then(Value::as_str).map(str::to_string))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| session.login.clone());
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
            // whether this machine has a converter, so the print button
            // knows which route to open. Asked once at login rather than
            // guessed per click, and answered by the server because the
            // client has no way to see a binary on somebody's PATH.
            "can_print_pdf": self.pdf.is_some(),
            "db": self.database_name(),
            "server_version": PROTOCOL_VERSION,
            "server_version_info": version_info(),
            "name": name,
            "username": session.login,
            "partner_id": Value::Null,
            "max_file_upload_size": MAX_FILE_UPLOAD_SIZE,
            "active_ids_limit": ACTIVE_IDS_LIMIT,
            // The company switcher is drawn on every page of the backend
            // and reads these without checking: no `user_companies` is a
            // client that renders its navbar and throws.
            "user_companies": self.user_companies(session.uid).await,
            // whether an action may celebrate itself with a rainbow. A
            // constant here because `base_setup` is what makes it a
            // setting, and that addon is not ported.
            "show_effect": true,
            "currencies": self.currencies().await,
            // Odoo's `bundle_params` carries the language a lazy bundle
            // must be fetched in, so a dialog loaded later is not in
            // English inside a Portuguese client.
            "bundle_params": {"lang": lang},
            // Which view types exist, which the action manager looks up
            // for every view of an action and refuses the action without.
            "view_info": view_info(),
        })
    }

    /// The companies the user may act for, as the switcher reads them.
    ///
    /// Odoo walks `res.company` through the hierarchy the user is allowed
    /// (`_get_company_ids`) plus the ancestors it must name to draw the
    /// tree. Multi-company allowance is not modelled in this port yet, so
    /// what a user has is the company on their own record — stated here
    /// rather than pretended: the switcher then shows one company, which
    /// is the truth about this database.
    async fn user_companies(&self, uid: i64) -> Value {
        let company = self.user_company(uid).await;
        let Some((id, name)) = company else {
            // A database whose `res.company` was never installed: Odoo
            // sends no `user_companies` for a non-internal user either,
            // and the switcher then draws nothing.
            return Value::Null;
        };
        json!({
            "current_company": id,
            "allowed_companies": {
                id.to_string(): {
                    "id": id,
                    "name": name,
                    "sequence": 10,
                    "child_ids": [],
                    "parent_id": false,
                }
            },
            "disallowed_ancestor_companies": {},
        })
    }

    /// The company on the user's own record, with its name. A user whose
    /// company is unset acts for the first one there is: Odoo requires
    /// `company_id` on `res.users`, so a row without it is this port's
    /// own gap and not a user who belongs nowhere.
    async fn user_company(&self, uid: i64) -> Option<(i64, String)> {
        self.registry.get("res.company")?;
        let own = match self
            .registry
            .get("res.users")
            .and_then(|model| model.field("company_id"))
        {
            Some(_) => self
                .registry
                .read(&self.pool, "res.users", &[uid], &["company_id"])
                .await
                .ok()
                .and_then(|rows| rows.into_iter().next())
                // a many2one reads back as [id, display_name]
                .and_then(|row| {
                    let pair = row.get("company_id")?.as_array()?.clone();
                    let id = pair.first()?.as_i64()?;
                    let name = pair
                        .get(1)
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    Some((id, name))
                }),
            None => None,
        };
        match own {
            Some(company) => Some(company),
            None => self.first_company().await,
        }
    }

    /// The company a database starts with, by id.
    async fn first_company(&self) -> Option<(i64, String)> {
        let domain = rusdoo_orm::domain::parse_domain(&json!([])).ok()?;
        let opts = rusdoo_orm::crud::SearchOptions {
            limit: Some(1),
            ..Default::default()
        };
        let ids = self
            .registry
            .search(&self.pool, "res.company", &domain, &opts)
            .await
            .ok()?;
        let row = self
            .registry
            .read(&self.pool, "res.company", &ids, &["name"])
            .await
            .ok()?
            .into_iter()
            .next()?;
        Some((
            row.get("id")?.as_i64()?,
            row.get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ))
    }

    /// Every currency the client may have to format, keyed by id — port
    /// of `res.currency.get_all_currencies`. A database without the model
    /// answers none, and the client formats a monetary field with the
    /// digits of the field itself.
    async fn currencies(&self) -> Value {
        let Some(model) = self.registry.get("res.currency") else {
            return json!({});
        };
        let wanted: Vec<&str> = ["name", "symbol", "position", "decimal_places"]
            .into_iter()
            .filter(|field| model.field(field).is_some())
            .collect();
        let Ok(domain) = rusdoo_orm::domain::parse_domain(&json!([])) else {
            return json!({});
        };
        let Ok(ids) = self
            .registry
            .search(
                &self.pool,
                "res.currency",
                &domain,
                &rusdoo_orm::crud::SearchOptions::default(),
            )
            .await
        else {
            return json!({});
        };
        let Ok(rows) = self
            .registry
            .read(&self.pool, "res.currency", &ids, &wanted)
            .await
        else {
            return json!({});
        };
        let mut out = serde_json::Map::new();
        for row in rows {
            let Some(id) = row.get("id").and_then(Value::as_i64) else {
                continue;
            };
            let digits = row
                .get("decimal_places")
                .and_then(Value::as_i64)
                .unwrap_or(2);
            out.insert(
                id.to_string(),
                json!({
                    "name": row.get("name").cloned().unwrap_or(Value::Null),
                    "symbol": row.get("symbol").cloned().unwrap_or(Value::Null),
                    "position": row.get("position").cloned().unwrap_or(json!("after")),
                    // Odoo's own pair: the total width nobody uses, then
                    // the decimal places that matter
                    "digits": [69, digits],
                }),
            );
        }
        json!(out)
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_view_type_an_action_may_open_is_described() {
        let info = view_info();
        // the action manager throws `View types not defined` for a type
        // it cannot find here, so a missing one is a menu that refuses
        for kind in ["list", "form", "kanban", "calendar", "graph", "pivot", "search"] {
            assert!(!info[kind]["icon"].is_null(), "{kind} has no icon");
            assert!(!info[kind]["display_name"].is_null(), "{kind} has no name");
        }
        // a form shows one record; that is how the client picks a view
        // for a small screen
        assert_eq!(info["form"]["multi_record"], json!(false));
        assert_eq!(info["list"]["multi_record"], json!(true));
        // a template is not a view an action opens, as in Odoo
        assert!(info["qweb"].is_null());
    }
}
