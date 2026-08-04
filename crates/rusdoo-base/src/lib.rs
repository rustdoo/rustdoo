//! rusdoo-base — port of `odoo/addons/base/models/`: the models every
//! other addon builds on.
//!
//! In Odoo these are Python classes, not data: the `base` addon ships
//! their *records* (groups, ACLs, views, menus) but the models themselves
//! are code. The split is the same here — this crate is the code, and
//! `addons/base/` is the data.

pub mod settings;

use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::fields::{Field, FieldType, OnDelete};
use rusdoo_orm::methods::{MethodCtx, MethodFuture, MethodRegistry};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};

/// The uid of the superuser (`base.user_root`), which bypasses the ACL.
pub const SUPERUSER_ID: i64 = 1;

pub(crate) fn char(name: &str) -> Field {
    Field::new(name, FieldType::Char { size: None })
}

pub(crate) fn m2o(name: &str, comodel: &str) -> Field {
    Field::new(
        name,
        FieldType::Many2one {
            comodel: comodel.to_string(),
        },
    )
}

pub(crate) fn meta(name: &str, table: &str) -> ModelMeta {
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
    settings::extend_methods(methods)?;
    methods.register(
        "ir.module.module",
        "button_immediate_install",
        Operation::Write,
        button_immediate_install,
    )?;
    Ok(())
}

/// `button_immediate_install` — the Install button of the Apps screen.
///
/// In Odoo this loads the module's Python and rebuilds the live registry
/// inside the request, so the screen comes back with the app running.
/// Here the models are compiled into the binary: a module the running
/// registry does not have cannot grow one at runtime. So the button does
/// the half that *is* honest — it records the decision, in the database,
/// where the next boot reads it — and says plainly that the server has to
/// come back for the models.
///
/// It refuses one thing and warns about another. A module whose manifest
/// says it is not installable is on disk and out of reach on purpose, and
/// that is a refusal. A module this build carries no *models* for is a
/// different matter: plenty of addons are data only — views, menus,
/// access rules over models another module declares — and those install
/// perfectly. So it is accepted with a warning, and the boot is what
/// decides: what it cannot create it skips, and the log names it.
fn button_immediate_install<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        if ctx.ids.is_empty() {
            return Err(RusdooError::Validation("say which module to install".into()));
        }
        let rows = ctx
            .registry
            .read(
                ctx.pool,
                "ir.module.module",
                &ctx.ids,
                &["name", "state", "installable", "has_code"],
            )
            .await?;
        let mut asked = Vec::new();
        let mut warned = false;
        for row in &rows {
            let name = row.get("name").and_then(Value::as_str).unwrap_or("?");
            if row.get("state").and_then(Value::as_str) == Some("installed") {
                continue;
            }
            if row.get("installable").and_then(Value::as_bool) != Some(true) {
                return Err(RusdooError::Validation(format!(
                    "module {name:?} says it is not installable"
                )));
            }
            if row.get("has_code").and_then(Value::as_bool) != Some(true) {
                // not a refusal: a data-only addon needs no models of its
                // own, and this is most of what a real tree ships
                tracing::warn!(
                    "module {name:?} has no compiled models in this build — \
                     installing it will load its data; anything it declares that \
                     needs a model this server does not have will be skipped and logged"
                );
                warned = true;
            }
            asked.push(row.get("id").and_then(Value::as_i64).unwrap_or_default());
        }
        if asked.is_empty() {
            return Ok(json!({"already_installed": true}));
        }
        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                "ir.module.module",
                &asked,
                vec![("state", json!("to install"))],
            )
            .await?;
        Ok(json!({
            "type": "ir.actions.client",
            "tag": "display_notification",
            "params": {
                "type": "info",
                "title": "Marcado para instalar",
                "message": if warned {
                    "O módulo entra no ar quando o servidor reiniciar. Este servidor \
                     não traz modelos compilados para ele: os dados serão carregados, \
                     e o que precisar de um modelo ausente é pulado e registrado no log."
                } else {
                    "O módulo entra no ar quando o servidor reiniciar: os modelos são \
                     código compilado, e é o boot que os registra."
                },
                "sticky": true,
            },
        }))
    })
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
        tracing::info!("autovacuum: {swept} transient record(s) swept");
        Ok(json!(swept))
    })
}

fn models() -> Vec<Model> {
    vec![
        lang(),
        config_parameter(),
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
        module(),
        module_dependency(),
        ir_model(),
        ir_model_fields(),
        filters(),
        model_data(),
        model_access(),
        record_rule(),
        settings::settings(),
    ]
}

/// `ir.model.data` — the external ids, as a model.
///
/// The rows have existed since the first install: they are how a data
/// file says "this record is `sale.action_orders`" and how the second
/// boot finds it again. What they were not was *visible* — the table was
/// written by raw SQL and no screen could open it, which is a strange
/// thing for the index of everything a module made.
///
/// The columns mirror `IR_MODEL_DATA_DDL` exactly, because the table is
/// created by that DDL on the first boot and this ORM does not yet alter
/// a table it did not make.
fn model_data() -> Model {
    Model::new(
        meta("ir.model.data", "ir_model_data"),
        vec![
            char("module").required(),
            char("name").required(),
            // the model and row this id points at — Odoo's `model` is a
            // plain string here too, because an external id may name a
            // record of a model this build does not carry
            char("model").required(),
            Field::new("res_id", FieldType::Integer).required(),
            // what `noupdate="1"` in a data file means: the user edits
            // this and a module update must not undo it
            Field::new("noupdate", FieldType::Boolean).default_value(json!(false)),
        ],
    )
    // the same unique key `IR_MODEL_DATA_DDL` declares, and not for
    // symmetry: the installer upserts external ids with `ON CONFLICT
    // ("module", "name")`, which needs a constraint to conflict *on*.
    // Now that this model creates the table, it has to carry it.
    .sql_constrained(
        "ir_model_data_module_name_uniq",
        r#"UNIQUE ("module", "name")"#,
        "an external id is one record",
    )
    .ordered("module, name, id")
}

/// `ir.model.access` — who may read, write, create and delete what.
///
/// Loaded from every module's `ir.model.access.csv` and enforced on every
/// call. It was invisible for the same reason as the external ids, and
/// this one matters more: an administrator who cannot *see* the access
/// rules is an administrator who cannot audit them.
///
/// Columns mirror `IR_MODEL_ACCESS_DDL`. `module` is this port's own —
/// it is how a re-install replaces the grants of one module without
/// touching another's.
fn model_access() -> Model {
    Model::new(
        meta("ir.model.access", "ir_model_access"),
        vec![
            char("module").required(),
            char("model").required(),
            m2o("group_id", "res.groups").required(),
            Field::new("perm_read", FieldType::Boolean).default_value(json!(false)),
            Field::new("perm_write", FieldType::Boolean).default_value(json!(false)),
            Field::new("perm_create", FieldType::Boolean).default_value(json!(false)),
            Field::new("perm_unlink", FieldType::Boolean).default_value(json!(false)),
        ],
    )
    .ordered("model, id")
}

/// `ir.rule` — which *records* of a model somebody may touch.
///
/// The domain is stored as the text a data file wrote, `user.id` and all:
/// it is substituted per call, never evaluated as code. See
/// `rusdoo-orm/src/rules.rs` for why that token is a token and not an
/// expression.
fn record_rule() -> Model {
    Model::new(
        meta("ir.rule", "ir_rule"),
        vec![
            char("module").required(),
            char("model").required(),
            Field::new("domain_force", FieldType::Text).required(),
            // the groups the rule applies to, as the JSON list the loader
            // wrote; empty means a global rule, which is the widest kind
            Field::new("groups", FieldType::Text).default_value(json!("[]")),
            Field::new("perm_read", FieldType::Boolean).default_value(json!(false)),
            Field::new("perm_write", FieldType::Boolean).default_value(json!(false)),
            Field::new("perm_create", FieldType::Boolean).default_value(json!(false)),
            Field::new("perm_unlink", FieldType::Boolean).default_value(json!(false)),
        ],
    )
    .ordered("model, id")
}

/// `ir.filters` — the searches somebody saved.
///
/// Port of `odoo/addons/base/models/ir_filters.py`. A saved filter is
/// the smallest piece of an ERP that belongs to a *person*: a domain, the
/// context it was built in, and the order it read best in. Losing them
/// between sessions is what makes a system feel like somebody else's.
///
/// `model_id` is the model's technical name, as in Odoo — a `Selection`
/// there, filled from the registry; a plain string here, which is what
/// the client sends and what `ir.model` now describes.
///
/// Sharing follows Odoo's rule and it is worth stating, because the empty
/// case is the surprising one: a filter with no users is shared with
/// **everyone**, and one that names users belongs to them.
fn filters() -> Model {
    Model::new(
        meta("ir.filters", "ir_filters"),
        vec![
            char("name").required(),
            char("model_id").required(),
            // the three pieces a search is made of, stored as the client
            // sends them: JSON text, not columns this port would have to
            // re-render on the way out
            Field::new("domain", FieldType::Text)
                .required()
                .default_value(json!("[]")),
            Field::new("context", FieldType::Text)
                .required()
                .default_value(json!("{}")),
            char("sort").required().default_value(json!("[]")),
            Field::new(
                "user_ids",
                FieldType::Many2many {
                    comodel: "res.users".into(),
                    relation: "ir_filters_users_rel".into(),
                    column1: "filter_id".into(),
                    column2: "user_id".into(),
                },
            ),
            // the one that opens with the screen
            Field::new("is_default", FieldType::Boolean).default_value(json!(false)),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
        ],
    )
    .constrained(
        "a saved search is a search",
        &["domain", "context", "sort"],
        a_filter_holds_json,
    )
    .ordered("model_id, name, id desc")
}

/// The three pieces are JSON the client sends and the client reads back.
/// Storing text nobody parsed is how a saved filter becomes a screen that
/// will not open, months later, for one person.
fn a_filter_holds_json(record: &Map<String, Value>) -> Result<(), String> {
    for (field, empty) in [("domain", "[]"), ("context", "{}"), ("sort", "[]")] {
        let text = record
            .get(field)
            .and_then(Value::as_str)
            .unwrap_or(empty)
            .trim()
            .to_string();
        if text.is_empty() {
            continue;
        }
        if serde_json::from_str::<Value>(&text).is_err() {
            return Err(format!("the {field} of a saved filter is not JSON: {text:?}"));
        }
    }
    Ok(())
}

/// `ir.model` — every model this server runs, as a row.
///
/// Port of `odoo/addons/base/models/ir_model.py`. Odoo calls writing
/// these rows *reflection*: the registry is built from code, and then it
/// describes itself into the database, because half of what an ERP does
/// is talk about its own shape. A saved filter names a model; a server
/// action names a model; `ir.rule` and `ir.model.access` in Odoo's own
/// data files point at `model_id`, not at a string.
///
/// The rows are not data anybody types. Every boot rewrites them from
/// the registry, and a model that stopped existing loses its row.
fn ir_model() -> Model {
    Model::new(
        meta("ir.model", "ir_model"),
        vec![
            // the technical name (`res.partner`), which is what every
            // other row means when it says "model"
            char("model").required(),
            // what a person reads
            char("name").translatable(),
            Field::new("info", FieldType::Text),
            // Odoo tells apart the models that came with the code from
            // the ones somebody built through the interface. This port
            // has no interface for building one, so every row is `base`
            // until it does.
            Field::new(
                "state",
                FieldType::Selection(vec![
                    ("manual".into(), "Custom Object".into()),
                    ("base".into(), "Base Object".into()),
                ]),
            )
            .required()
            .default_value(json!("base")),
            // a wizard is a model whose rows are swept: a screen that
            // lists models has to say which are which
            Field::new("transient", FieldType::Boolean).default_value(json!(false)),
            Field::new(
                "field_id",
                FieldType::One2many {
                    comodel: "ir.model.fields".into(),
                    inverse: "model_id".into(),
                },
            ),
        ],
    )
    .sql_constrained(
        "ir_model_model_uniq",
        r#"UNIQUE ("model")"#,
        "a model is described once",
    )
    .ordered("model, id")
}

/// `ir.model.fields` — every field of every model, as a row.
///
/// The column names are Odoo's, because its own data files and every
/// module that reads metadata use them: `ttype` for the kind, `relation`
/// for the comodel a relational field points at, `relation_field` for
/// the column a one2many is inverse of.
fn ir_model_fields() -> Model {
    Model::new(
        meta("ir.model.fields", "ir_model_fields"),
        vec![
            char("name").required(),
            // both, like Odoo: the link for a screen, the string for
            // everything that only knows the technical name
            char("model").required(),
            m2o("model_id", "ir.model").ondelete(OnDelete::Cascade),
            char("field_description").translatable(),
            char("ttype").required(),
            char("relation"),
            char("relation_field"),
            Field::new("required", FieldType::Boolean).default_value(json!(false)),
            Field::new("readonly", FieldType::Boolean).default_value(json!(false)),
            Field::new("store", FieldType::Boolean).default_value(json!(true)),
            Field::new("translate", FieldType::Boolean).default_value(json!(false)),
            // what a field is derived from, when it is derived: the path
            // of a related field, and whether a compute stands behind it
            char("related"),
            Field::new("is_computed", FieldType::Boolean).default_value(json!(false)),
            Field::new(
                "state",
                FieldType::Selection(vec![
                    ("manual".into(), "Custom Field".into()),
                    ("base".into(), "Base Field".into()),
                ]),
            )
            .required()
            .default_value(json!("base")),
        ],
    )
    .sql_constrained(
        "ir_model_fields_name_uniq",
        r#"UNIQUE ("model", "name")"#,
        "a field is described once per model",
    )
    .ordered("model, name, id")
}

/// The states a module is in, and the one that makes the screen useful.
///
/// Odoo's list, minus what this port cannot honour: `to remove`, because
/// taking a module's data back out is a slice of its own, and
/// `to upgrade`, which needs a version comparison and a data reload that
/// knows what to overwrite.
///
/// **`to install` is not a formality here, it is the mechanism.** In
/// Odoo, installing loads the module's Python and rebuilds the live
/// registry in the same request. In this port the models are compiled
/// into the binary, so a module that is not in the running registry
/// cannot grow one at runtime: the button records the decision, and the
/// next boot honours it — registering the models, creating the tables
/// and loading the data. The database is what remembers, which is also
/// what this port was missing: until now the installed set lived in an
/// environment variable and nothing wrote it down.
fn module_states() -> FieldType {
    FieldType::Selection(vec![
        ("uninstalled".into(), "Not Installed".into()),
        ("to install".into(), "To be installed".into()),
        ("installed".into(), "Installed".into()),
    ])
}

/// `ir.module.module` — one row per addon found on disk.
///
/// Port of `odoo/addons/base/models/ir_module.py`, the part a person
/// uses: what the module is called, what it does, whether it is on, and
/// what it needs. The rows are not data anybody types — every boot
/// rewrites them from the manifests on disk, because the disk is the
/// truth about what exists and the database is the truth about what is
/// running.
fn module() -> Model {
    Model::new(
        meta("ir.module.module", "ir_module_module"),
        vec![
            // the directory name, which is what every dependency names
            char("name").required(),
            // the manifest's `name`, which is what a person reads
            char("shortdesc"),
            Field::new("state", module_states())
                .required()
                .default_value(json!("uninstalled")),
            char("latest_version"),
            char("author"),
            char("website"),
            char("category"),
            char("summary"),
            Field::new("description", FieldType::Text),
            // Odoo shows applications apart from the technical modules
            // they pull in, and so should any screen over this
            Field::new("application", FieldType::Boolean).default_value(json!(false)),
            Field::new("auto_install", FieldType::Boolean).default_value(json!(false)),
            // a module whose manifest says it is not installable is on
            // disk and out of reach: the screen has to say why
            Field::new("installable", FieldType::Boolean).default_value(json!(true)),
            // whether this build has the module's code compiled in. The
            // honest column: an addon can be on disk, with data files and
            // a manifest, and still have no models here.
            Field::new("has_code", FieldType::Boolean).default_value(json!(false)),
            Field::new(
                "dependencies_id",
                FieldType::One2many {
                    comodel: "ir.module.module.dependency".into(),
                    inverse: "module_id".into(),
                },
            ),
        ],
    )
    .sql_constrained(
        "ir_module_module_name_uniq",
        r#"UNIQUE ("name")"#,
        "a module is named once",
    )
    // applications first, then by name: the same order the Apps screen
    // reads in Odoo
    .ordered("application desc, shortdesc, name, id")
}

/// `ir.module.module.dependency` — what a module needs before it can
/// run. A model of its own, as in Odoo, because a dependency is a row a
/// screen shows and a search filters on, not a string somebody parses.
fn module_dependency() -> Model {
    Model::new(
        meta("ir.module.module.dependency", "ir_module_module_dependency"),
        vec![
            char("name").required(),
            m2o("module_id", "ir.module.module").ondelete(OnDelete::Cascade),
        ],
    )
    .ordered("name, id")
}

/// `res.lang` — the languages a database answers in.
///
/// Port of `odoo/addons/base/models/res_lang.py`, the fields a client
/// actually needs: what the language is called, its code, and how it
/// writes a date, a time and a number. A language is a row and not a
/// constant because installing one is a decision an administrator makes,
/// and because the formats belong to the language, not to the code.
fn lang() -> Model {
    Model::new(
        meta("res.lang", "res_lang"),
        vec![
            char("name").required(),
            // `en_US`, `pt_BR` — the key every translated column uses
            char("code").required(),
            // the name of the `.po` file the translations come from
            char("iso_code"),
            // Odoo declares it required and fills it from `install_lang`,
            // a `<function>` in `res_lang_data.xml` that this port does
            // not run yet. Requiring it here turns Odoo's own data into
            // an error, which is being stricter than Odoo about Odoo.
            char("url_code"),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
            Field::new(
                "direction",
                FieldType::Selection(vec![
                    ("ltr".into(), "Left-to-Right".into()),
                    ("rtl".into(), "Right-to-Left".into()),
                ]),
            )
            .default_value(json!("ltr")),
            char("date_format").default_value(json!("%m/%d/%Y")),
            char("time_format").default_value(json!("%H:%M:%S")),
            char("decimal_point").default_value(json!(".")),
            char("thousands_sep").default_value(json!(",")),
            // how a number is grouped, as the client reads it: the text
            // of a list, `[3,0]` for 123,456,789 and `[3,2,0]` for the
            // Indian 12,34,56,789. A string and not a list because that
            // is what `res.lang` stores and what the client parses.
            Field::new(
                "grouping",
                FieldType::Selection(vec![
                    ("[3,0]".into(), "International Grouping".into()),
                    ("[3,2,0]".into(), "Indian Grouping".into()),
                ]),
            )
            .default_value(json!("[3,0]")),
            // which day a week starts on, 1 = Monday .. 7 = Sunday. The
            // calendar and the date pickers read it, so a wrong default
            // is a week drawn wrong.
            Field::new(
                "week_start",
                FieldType::Selection(vec![
                    ("1".into(), "Monday".into()),
                    ("2".into(), "Tuesday".into()),
                    ("3".into(), "Wednesday".into()),
                    ("4".into(), "Thursday".into()),
                    ("5".into(), "Friday".into()),
                    ("6".into(), "Saturday".into()),
                    ("7".into(), "Sunday".into()),
                ]),
            )
            .default_value(json!("7")),
        ],
    )
    .sql_constrained(
        "res_lang_code_uniq",
        r#"UNIQUE ("code")"#,
        "a language with that code already exists",
    )
    // Odoo's `_order`: the installed ones first, then by name
    .ordered("active desc, name, id")
}

/// `ir.config_parameter` — the switches a database is set with.
///
/// Port of `odoo/addons/base/models/ir_config_parameter.py`. A module
/// that can be configured stops having to pick one behaviour and be
/// wrong for half its users.
fn config_parameter() -> Model {
    Model::new(
        meta("ir.config_parameter", "ir_config_parameter"),
        vec![
            char("key").required(),
            Field::new("value", FieldType::Text).required(),
        ],
    )
    // the uniqueness is the database's, not a check before the insert:
    // `set_param` is an upsert, and an upsert needs a key to conflict on
    .sql_constrained(
        "ir_config_parameter_key_uniq",
        r#"UNIQUE ("key")"#,
        "a parameter with that key already exists",
    )
    .ordered("key, id")
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
    // two sequences with the same code are two numberings for the
    // mesmo documento: o `next_by_code` pegaria uma delas, e qual
    // depende do dia
    .sql_constrained(
        "ir_sequence_code_uniq",
        r#"UNIQUE ("code")"#,
        "a sequence with that code already exists",
    )
    .ordered("name, id")
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
                    ("contact".into(), "Contact".into()),
                    ("invoice".into(), "Invoice address".into()),
                    ("delivery".into(), "Delivery address".into()),
                    ("other".into(), "Other".into()),
                ]),
            )
            .default_value(json!("contact")),
            Field::new("comment", FieldType::Text),
            // like every archivable Odoo model: born active
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
        ],
    )
    .ordered("name, id")
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
    .sql_constrained(
        "res_users_login_uniq",
        r#"UNIQUE ("login")"#,
        "a user with that login already exists",
    )
    .ordered("login, id")
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
            // set when this view is a patch of another one instead of a
            // view in its own right: a module that adds a field to
            // somebody else's form publishes the difference, not a copy
            m2o("inherit_id", "ir.ui.view"),
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
                    ("minutes".into(), "Minutes".into()),
                    ("hours".into(), "Hours".into()),
                    ("days".into(), "Days".into()),
                    ("weeks".into(), "Weeks".into()),
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
