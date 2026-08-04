//! `res.config.settings` — the Settings screen, port of
//! `odoo/addons/base/models/res_config.py`.
//!
//! The form everybody means when they say "Configurações": a page of
//! switches, a Save button, and nothing stored. It is a **transient**
//! model, and that is the whole trick — the record exists for the length
//! of one visit, holds what the screen is showing, and is swept. What
//! survives is what `execute` writes: parameters, and decisions about
//! which modules run.
//!
//! ## The three kinds of switch, and which are here
//!
//! Odoo's settings fields come in three shapes:
//!
//! * `config_parameter='key'` — the value goes to `ir.config_parameter`.
//!   **Ported**, and the two here are read by the client on every boot:
//!   the upload limit and the active-ids limit, which were constants in
//!   this port until now.
//! * `module_<name>` — ticking it installs the module. **Ported**, on top
//!   of the catalogue: it records `to install`, and the next boot honours
//!   it, because models here are compiled into the binary. The Apps
//!   screen says the same thing in more words.
//! * `group_<name>` — ticking it grants a group to a set of users.
//!   **Not ported.** It is not a switch but a membership change over
//!   whoever counts as an internal user, and getting that wrong grants
//!   access nobody asked for. It belongs with the user screens.
//!
//! Reading the current state is what a settings form is mostly made of,
//! and it is done here the way the ORM already offers: a dynamic default
//! per field, run when the screen opens.

use crate::{char, m2o, meta};
use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::fields::{DefaultCtx, Field, FieldType};
use rusdoo_orm::methods::{MethodCtx, MethodFuture, MethodRegistry};
use rusdoo_orm::model::Model;
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};
use std::future::Future;
use std::pin::Pin;

type Answer<'a> = Pin<Box<dyn Future<Output = Result<Value, RusdooError>> + Send + 'a>>;

/// The parameters this screen writes, with the key each one lands under
/// and the default Odoo uses when nobody has set it.
///
/// Kept as a table because `execute` walks it: a switch that is read by
/// one function and written by another drifts apart the first time
/// somebody adds a field.
const PARAMETERS: [(&str, &str, i64); 2] = [
    ("max_file_upload_size", "web.max_file_upload_size", 128 * 1024 * 1024),
    ("active_ids_limit", "web.active_ids_limit", 20_000),
];

/// The modules this screen can turn on, and the field that turns them.
///
/// Odoo generates one field per installable module; here they are named,
/// because a field is a column and a column is not generated at runtime.
/// Anything missing from this list is still reachable from the Apps
/// screen, which lists what is on disk rather than what somebody wrote
/// down.
const MODULES: [(&str, &str); 8] = [
    ("module_crm", "crm"),
    ("module_project", "project"),
    ("module_hr", "hr"),
    ("module_maintenance", "maintenance"),
    ("module_fleet", "fleet"),
    ("module_lunch", "lunch"),
    ("module_contacts", "contacts"),
    ("module_stock", "stock"),
];

/// Read a parameter as a number, falling back to Odoo's own default.
async fn number_param(ctx: &mut DefaultCtx<'_>, key: &str, fallback: i64) -> Value {
    let found: Option<String> = sqlx::query_scalar(
        r#"SELECT "value" FROM "ir_config_parameter" WHERE "key" = $1"#,
    )
    .bind(key)
    .fetch_optional(&mut *ctx.conn)
    .await
    .unwrap_or(None);
    json!(found
        .and_then(|value| value.trim().parse::<i64>().ok())
        .unwrap_or(fallback))
}

/// Whether a module is running, or on its way: the screen shows a tick
/// for a module somebody already asked for, or the next visit would
/// unask it.
async fn module_is_on(ctx: &mut DefaultCtx<'_>, module: &str) -> Value {
    let state: Option<String> = sqlx::query_scalar(
        r#"SELECT "state" FROM "ir_module_module" WHERE "name" = $1"#,
    )
    .bind(module)
    .fetch_optional(&mut *ctx.conn)
    .await
    .unwrap_or(None);
    json!(matches!(state.as_deref(), Some("installed") | Some("to install")))
}

macro_rules! parameter_default {
    ($name:ident, $key:expr, $fallback:expr) => {
        fn $name(mut ctx: DefaultCtx<'_>) -> Answer<'_> {
            Box::pin(async move { Ok(number_param(&mut ctx, $key, $fallback).await) })
        }
    };
}

macro_rules! module_default {
    ($name:ident, $module:literal) => {
        fn $name(mut ctx: DefaultCtx<'_>) -> Answer<'_> {
            Box::pin(async move { Ok(module_is_on(&mut ctx, $module).await) })
        }
    };
}

parameter_default!(default_max_upload, "web.max_file_upload_size", 128 * 1024 * 1024);
parameter_default!(default_active_ids, "web.active_ids_limit", 20_000);
module_default!(default_module_crm, "crm");
module_default!(default_module_project, "project");
module_default!(default_module_hr, "hr");
module_default!(default_module_maintenance, "maintenance");
module_default!(default_module_fleet, "fleet");
module_default!(default_module_lunch, "lunch");
module_default!(default_module_contacts, "contacts");
module_default!(default_module_stock, "stock");

/// `res.config.settings` — one visit to the Settings screen.
pub fn settings() -> Model {
    Model::new(
        meta("res.config.settings", "res_config_settings"),
        vec![
            m2o("company_id", "res.company"),
            // what the client reads at boot, and could not be changed
            // without recompiling until now
            Field::new("max_file_upload_size", FieldType::Integer)
                .default_from(default_max_upload),
            Field::new("active_ids_limit", FieldType::Integer).default_from(default_active_ids),
            // and the apps, which write the same decision the Apps
            // screen's Install button writes
            Field::new("module_crm", FieldType::Boolean).default_from(default_module_crm),
            Field::new("module_project", FieldType::Boolean).default_from(default_module_project),
            Field::new("module_hr", FieldType::Boolean).default_from(default_module_hr),
            Field::new("module_maintenance", FieldType::Boolean)
                .default_from(default_module_maintenance),
            Field::new("module_fleet", FieldType::Boolean).default_from(default_module_fleet),
            Field::new("module_lunch", FieldType::Boolean).default_from(default_module_lunch),
            Field::new("module_contacts", FieldType::Boolean).default_from(default_module_contacts),
            Field::new("module_stock", FieldType::Boolean).default_from(default_module_stock),
            char("note"),
        ],
    )
    // a visit, not a record: swept like every other dialog
    .transient()
    .ordered("id desc")
}

pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    methods.register(
        "res.config.settings",
        "execute",
        Operation::Write,
        execute,
    )?;
    Ok(())
}

/// `execute` — the Save button.
///
/// Reads the visit and applies it: parameters are written, modules that
/// were ticked are marked `to install`. Unticking does **not** uninstall:
/// taking a module's data back out is a slice this port has not written,
/// and a switch that silently did half of it would be worse than one
/// that says no.
fn execute<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let [visit] = ctx.ids[..] else {
            return Err(RusdooError::Validation(
                "save one settings page at a time".into(),
            ));
        };
        let wanted: Vec<&str> = PARAMETERS
            .iter()
            .map(|(field, _, _)| *field)
            .chain(MODULES.iter().map(|(field, _)| *field))
            .collect();
        let rows = ctx
            .registry
            .read(ctx.pool, "res.config.settings", &[visit], &wanted)
            .await?;
        let row = rows
            .first()
            .ok_or_else(|| RusdooError::Validation("the settings page is gone".into()))?;

        for (field, key, _) in PARAMETERS {
            if let Some(value) = row.get(field).and_then(Value::as_i64) {
                ctx.registry
                    .set_param(ctx.pool, key, &value.to_string())
                    .await?;
            }
        }

        let mut asked: Vec<String> = Vec::new();
        for (field, module) in MODULES {
            if row.get(field).and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let marked = mark_for_install(&ctx, module).await?;
            if marked {
                asked.push(module.to_string());
            }
        }

        Ok(json!({
            "type": "ir.actions.client",
            "tag": "reload",
            "modules_to_install": asked,
        }))
    })
}

/// Mark a module `to install` unless it already runs. Answers whether
/// anything changed, so the reply can name what the next boot will do.
async fn mark_for_install(ctx: &MethodCtx<'_>, module: &str) -> Result<bool, RusdooError> {
    use rusdoo_orm::crud::SearchOptions;
    let ids = ctx
        .registry
        .search(
            ctx.pool,
            "ir.module.module",
            &rusdoo_orm::domain::parse_domain(&json!([["name", "=", module]]))?,
            &SearchOptions {
                limit: Some(1),
                ..SearchOptions::default()
            },
        )
        .await?;
    let Some(id) = ids.first().copied() else {
        // a module nobody has on disk: the catalogue is written from the
        // filesystem, so this means the addon is not there at all
        return Err(RusdooError::Validation(format!(
            "module {module:?} is not in the catalogue: its addon is not on this server"
        )));
    };
    let rows = ctx
        .registry
        .read(ctx.pool, "ir.module.module", &[id], &["state"])
        .await?;
    if rows
        .first()
        .and_then(|row| row.get("state"))
        .and_then(Value::as_str)
        == Some("installed")
    {
        return Ok(false);
    }
    ctx.registry
        .write_as(
            ctx.pool,
            ctx.uid,
            "ir.module.module",
            &[id],
            vec![("state", json!("to install"))],
        )
        .await?;
    Ok(true)
}

/// Register the model.
pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    reg.register(settings())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_visit_is_swept_not_kept() {
        let model = settings();
        assert!(
            model.is_transient(),
            "uma visita à tela de configurações não é um registro"
        );
    }

    #[test]
    fn every_switch_the_save_walks_is_a_field_of_the_model() {
        let model = settings();
        for (field, _, _) in PARAMETERS {
            assert!(
                model.field(field).is_some(),
                "o save escreve {field}, que o modelo não tem"
            );
        }
        for (field, _) in MODULES {
            assert!(
                model.field(field).is_some(),
                "o save escreve {field}, que o modelo não tem"
            );
        }
    }

    #[test]
    fn every_switch_reads_the_state_it_shows() {
        // a settings field with no dynamic default opens blank and saves
        // a blank over whatever was there
        let model = settings();
        for (field, _, _) in PARAMETERS {
            assert!(
                model.field(field).expect("declared").default_fn.is_some(),
                "{field} abre sem ler o que já está valendo"
            );
        }
        for (field, _) in MODULES {
            assert!(
                model.field(field).expect("declared").default_fn.is_some(),
                "{field} abre sem ler o que já está valendo"
            );
        }
    }
}
