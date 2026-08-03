//! rusdoo-base — port of `odoo/addons/base/models/`: the models every
//! other addon builds on.
//!
//! In Odoo these are Python classes, not data: the `base` addon ships
//! their *records* (groups, ACLs, views, menus) but the models themselves
//! are code. The split is the same here — this crate is the code, and
//! `addons/base/` is the data.

use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::methods::{MethodCtx, MethodFuture, MethodRegistry};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};

/// The uid of the superuser (`base.user_root`), which bypasses the ACL.
pub const SUPERUSER_ID: i64 = 1;

fn char(name: &str) -> Field {
    Field::new(name, FieldType::Char { size: None })
}

fn m2o(name: &str, comodel: &str) -> Field {
    Field::new(
        name,
        FieldType::Many2one {
            comodel: comodel.to_string(),
        },
    )
}

fn meta(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.to_string(),
        table: table.to_string(),
        inherit: vec![],
        inherits: vec![],
    }
}

/// Every model of the `base` addon, registered in dependency order (a
/// many2one may only name a model already registered).
pub fn registry() -> Result<Registry, RusdooError> {
    let mut reg = Registry::new();
    for model in models() {
        reg.register(model)?;
    }
    Ok(reg)
}

/// Register the base models into an existing registry — for a server
/// that assembles its own on top of them.
pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    for model in models() {
        reg.register(model)?;
    }
    Ok(())
}

/// How old a dialog has to be before the vacuum sweeps it. Long enough
/// that nobody loses a wizard they left open over lunch.
const TRANSIENT_MAX_HOURS: i64 = 12;

/// The methods the framework itself schedules.
pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    methods.register("ir.autovacuum", "power_on", Operation::Unlink, power_on)?;
    Ok(())
}

/// `power_on` — delete the transient rows nobody came back to.
///
/// Only transient models are touched, and only rows older than the
/// window: a vacuum that could reach stored data would be a delete
/// nobody asked for, scheduled.
fn power_on<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let mut swept = 0u64;
        for model in ctx.registry.models() {
            if !model.is_transient() {
                continue;
            }
            let table = rusdoo_orm::sql::quote_ident(&model.meta.table)?;
            let sql = format!(
                r#"DELETE FROM {table}
                   WHERE "create_date" IS NOT NULL
                     AND "create_date" < (CURRENT_TIMESTAMP - ($1 || ' hours')::interval)"#
            );
            let done = sqlx::query(&sql)
                .bind(TRANSIENT_MAX_HOURS.to_string())
                .execute(ctx.pool)
                .await
                .map_err(|error| RusdooError::Database(error.to_string()))?;
            swept += done.rows_affected();
        }
        tracing::info!("autovacuum: {swept} registro(s) transientes removidos");
        Ok(json!(swept))
    })
}

fn models() -> Vec<Model> {
    vec![
        sequence(),
        country(),
        company(),
        partner(),
        groups(),
        users(),
        ui_view(),
        act_window(),
        report(),
        attachment(),
        cron(),
        autovacuum(),
        ui_menu(),
    ]
}

/// `ir.sequence` — the numbers documents carry.
fn sequence() -> Model {
    Model::new(
        meta("ir.sequence", "ir_sequence"),
        vec![
            char("name").required(),
            // what a field asks for by name
            char("code").required(),
            char("prefix"),
            char("suffix"),
            Field::new("padding", FieldType::Integer).default_value(json!(0)),
            Field::new("number_next", FieldType::Integer).default_value(json!(1)),
            Field::new("number_increment", FieldType::Integer).default_value(json!(1)),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
        ],
    )
}

/// `res.country` — the country of an address.
fn country() -> Model {
    Model::new(
        meta("res.country", "res_country"),
        vec![
            char("name").required(),
            // ISO 3166-1 alpha-2, what addresses and reports print
            char("code"),
            char("phone_code"),
        ],
    )
}

/// `res.company` — the company records belong to.
fn company() -> Model {
    Model::new(
        meta("res.company", "res_company"),
        vec![
            char("name").required(),
            char("email"),
            char("phone"),
            char("website"),
            char("vat"),
            m2o("parent_id", "res.company"),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
        ],
    )
}

/// `res.partner` — every person or organization the system knows.
fn partner() -> Model {
    Model::new(
        meta("res.partner", "res_partner"),
        vec![
            char("name").required(),
            char("email"),
            char("phone"),
            char("mobile"),
            char("website"),
            char("vat"),
            char("function"),
            char("street"),
            char("street2"),
            char("city"),
            char("zip"),
            m2o("country_id", "res.country"),
            m2o("parent_id", "res.partner"),
            m2o("company_id", "res.company"),
            Field::new("is_company", FieldType::Boolean).default_value(json!(false)),
            Field::new(
                "type",
                FieldType::Selection(vec![
                    ("contact".into(), "Contato".into()),
                    ("invoice".into(), "Endereço de faturamento".into()),
                    ("delivery".into(), "Endereço de entrega".into()),
                    ("other".into(), "Outro".into()),
                ]),
            )
            .default_value(json!("contact")),
            Field::new("comment", FieldType::Text),
            // like every archivable Odoo model: born active
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
        ],
    )
}

/// `res.groups` — what an ACL grant and a record rule are addressed to.
fn groups() -> Model {
    Model::new(
        meta("res.groups", "res_groups"),
        vec![
            char("name").required(),
            char("category"),
            Field::new("comment", FieldType::Text),
        ],
    )
}

/// `res.users` — who logs in. `groups_id` is the membership every access
/// check reads.
fn users() -> Model {
    Model::new(
        meta("res.users", "res_users"),
        vec![
            char("login").required(),
            // never leaves the server: the hash is not a readable field
            char("password").private(),
            char("name"),
            char("email"),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
            // the context every call of this user inherits
            char("lang").default_value(json!("en_US")),
            char("tz"),
            m2o("partner_id", "res.partner"),
            m2o("company_id", "res.company"),
            Field::new(
                "groups_id",
                FieldType::Many2many {
                    comodel: "res.groups".into(),
                    relation: "res_groups_users_rel".into(),
                    column1: "uid".into(),
                    column2: "gid".into(),
                },
            ),
        ],
    )
}

/// `ir.ui.view` — the arch a client renders.
fn ui_view() -> Model {
    Model::new(
        meta("ir.ui.view", "ir_ui_view"),
        vec![
            char("name"),
            char("model"),
            // list/form/kanban/qweb: which view the client asks for by type
            char("type").default_value(json!("form")),
            // lowest wins when several views share a model and type
            Field::new("priority", FieldType::Integer).default_value(json!(16)),
            Field::new("arch", FieldType::Text),
        ],
    )
}

/// `ir.actions.act_window` — opening a model in its views.
fn act_window() -> Model {
    Model::new(
        meta("ir.actions.act_window", "ir_act_window"),
        vec![
            char("name"),
            char("res_model").required(),
            char("view_mode").default_value(json!("list,form")),
            Field::new("domain", FieldType::Text),
        ],
    )
}

/// `ir.actions.report` — printing a record through a QWeb template.
fn report() -> Model {
    Model::new(
        meta("ir.actions.report", "ir_act_report"),
        vec![
            char("name").required(),
            char("model").required(),
            // the external id of the ir.ui.view holding the template
            char("report_name").required(),
            char("report_type").default_value(json!("qweb-html")),
        ],
    )
}

/// `ir.attachment` — a file kept next to a record.
///
/// The bytes live in the filestore on disk, not in the row: a database
/// dump should not carry every PDF anyone ever uploaded. The row keeps
/// what the file is and where it went.
fn attachment() -> Model {
    Model::new(
        meta("ir.attachment", "ir_attachment"),
        vec![
            char("name").required(),
            // which record it belongs to (Odoo's model/res_id pair)
            char("res_model"),
            Field::new("res_id", FieldType::Integer),
            char("mimetype"),
            Field::new("file_size", FieldType::Integer).default_value(json!(0)),
            // the name the bytes have inside the filestore
            char("store_fname"),
            char("description"),
        ],
    )
}

/// `ir.cron` — work the server does on its own, on a clock.
///
/// A job names a model and one of its methods: the same methods a client
/// calls by name, run by the server with nobody watching. There is no
/// second kind of code to write and no second place for it to live.
fn cron() -> Model {
    Model::new(
        meta("ir.cron", "ir_cron"),
        vec![
            char("name").required(),
            // what to run: a registered model method
            char("model").required(),
            char("code").required(),
            Field::new("interval_number", FieldType::Integer).default_value(json!(1)),
            Field::new(
                "interval_type",
                FieldType::Selection(vec![
                    ("minutes".into(), "Minutos".into()),
                    ("hours".into(), "Horas".into()),
                    ("days".into(), "Dias".into()),
                    ("weeks".into(), "Semanas".into()),
                ]),
            )
            .default_value(json!("days")),
            Field::new("nextcall", FieldType::Datetime),
            Field::new("lastcall", FieldType::Datetime),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
        ],
    )
}

/// `ir.autovacuum` — the job that sweeps what nobody kept.
///
/// Odoo has this model for the same reason: transient rows are dialogs
/// somebody had open, and a database that keeps every dialog forever is
/// a database nobody can explain the size of.
fn autovacuum() -> Model {
    Model::new(meta("ir.autovacuum", "ir_autovacuum"), vec![char("name")]).transient()
}

/// `ir.ui.menu` — the navigation tree.
fn ui_menu() -> Model {
    Model::new(
        meta("ir.ui.menu", "ir_ui_menu"),
        vec![
            char("name"),
            m2o("parent_id", "ir.ui.menu"),
            Field::new("sequence", FieldType::Integer).default_value(json!(10)),
            // external id of the act_window this menu opens (Odoo uses a
            // reference field; we bridge with the xml_id string for now)
            char("action"),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_base_model_registers() {
        let reg = registry().expect("base models register");
        for name in [
            "ir.sequence",
            "res.country",
            "res.company",
            "res.partner",
            "res.groups",
            "res.users",
            "ir.ui.view",
            "ir.actions.act_window",
            "ir.actions.report",
            "ir.attachment",
            "ir.cron",
            "ir.autovacuum",
            "ir.ui.menu",
        ] {
            assert!(reg.get(name).is_some(), "{name} must be registered");
        }
    }

    #[test]
    fn the_password_hash_is_not_a_readable_field() {
        let reg = registry().unwrap();
        let password = reg.get("res.users").unwrap().field("password").unwrap();
        assert!(!password.exposed, "the hash must never reach a client");
    }

    #[test]
    fn users_carry_their_group_membership() {
        let reg = registry().unwrap();
        let groups = reg.get("res.users").unwrap().field("groups_id").unwrap();
        assert!(matches!(
            groups.ty,
            FieldType::Many2many { ref comodel, .. } if comodel == "res.groups"
        ));
    }
}
