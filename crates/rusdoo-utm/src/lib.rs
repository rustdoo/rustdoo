//! rusdoo-utm — port de `odoo/addons/utm/models/`: de onde veio quem
//! chegou.
//!
//! Five small models other modules point at — campaign, medium, source,
//! stage and tag. `utm.mixin`, which pushes `campaign_id`, `source_id`
//! and `medium_id` into other modules' models, is left out: it is
//! `_inherit` across modules, which the port does not have yet. What is
//! left is what stands on its own — the records links track — and
//! os dois caminhos pelos quais um link cria um registro sem pedir nada
//! nobody.

use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::methods::{MethodCtx, MethodFuture, MethodRegistry};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};
use std::collections::HashSet;

/// Os modelos que um link rastreia (`_tracking_models` do `utm.mixin`).
/// With no mixin to gather them, each answers for itself — and it is
/// these three that get the create-by-name methods.
const TRACKING_MODELS: [&str; 3] = ["utm.campaign", "utm.medium", "utm.source"];

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

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    // stage and tag before the campaign, which points at both
    reg.register(stage())?;
    reg.register(tag())?;
    reg.register(medium())?;
    reg.register(source())?;
    reg.register(campaign())?;
    Ok(())
}

/// The two ways a link becomes a record: `name_create`, when somebody
/// digita um nome numa lista suspensa, e `find_or_create`, quando um
/// click carries a `utm_campaign=...` that may already exist.
pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    for model in TRACKING_MODELS {
        methods.register(model, "name_create", Operation::Create, name_create)?;
        methods.register(model, "find_or_create", Operation::Create, find_or_create)?;
    }
    Ok(())
}

/// A name of nothing but spaces gets past the database's `required` and
/// identifies nothing — whoever looks for the source later finds no
/// empty column to look at.
fn name_is_filled(record: &Map<String, Value>) -> Result<(), String> {
    if is_blank(record, "name") {
        return Err("the name cannot be left blank".into());
    }
    Ok(())
}

/// A campaign keeps its name in `title`; `name` is the identifier
/// derived from it, and there is no point demanding the second.
fn title_is_filled(record: &Map<String, Value>) -> Result<(), String> {
    if is_blank(record, "title") {
        return Err("give the campaign a name".into());
    }
    Ok(())
}

fn is_blank(record: &Map<String, Value>, field: &str) -> bool {
    record
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .is_empty()
}

/// `utm.stage` — where the campaign stands.
fn stage() -> Model {
    Model::new(
        meta("utm.stage", "utm_stage"),
        vec![
            char("name").required(),
            // a ordem das colunas do quadro; o Odoo ordena o modelo por ela
            Field::new("sequence", FieldType::Integer).default_value(json!(1)),
        ],
    )
    .constrained("nome preenchido", &["name"], name_is_filled)
}

/// `utm.tag` — how marketing sorts its own campaigns.
fn tag() -> Model {
    Model::new(
        meta("utm.tag", "utm_tag"),
        vec![
            char("name").required(),
            // o Odoo sorteia uma cor; aqui a etiqueta nasce sem nenhuma,
            // which is what "no colour" means on the board: nothing to
            // stand out
            Field::new("color", FieldType::Integer).default_value(json!(0)),
        ],
    )
    .constrained("nome preenchido", &["name"], name_is_filled)
}

/// `utm.medium` — por onde o visitante veio (e-mail, banner, telefone).
fn medium() -> Model {
    Model::new(
        meta("utm.medium", "utm_medium"),
        vec![
            char("name").required(),
            // arquivar em vez de apagar: um meio some das listas sem
            // take the history pointing at it along
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
        ],
    )
    .constrained("nome preenchido", &["name"], name_is_filled)
}

/// `utm.source` — de qual lugar exatamente (a newsletter de maio, o
/// buscador, o site do parceiro).
fn source() -> Model {
    Model::new(
        meta("utm.source", "utm_source"),
        vec![char("name").required()],
    )
    .constrained("nome preenchido", &["name"], name_is_filled)
}

/// `name` — the campaign's identifier, which follows the name it was
/// given.
///
/// In Odoo it also gets a counter when two titles coincide; that needs a
/// database lookup, and a computed field's function has no database.
/// What is left is the mirror: a campaign always has an identifier, and
/// it is what shows up wherever somebody references it.
fn campaign_name(record: &Map<String, Value>) -> Value {
    let title = record
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    json!(title)
}

/// `utm.campaign` — the marketing effort somebody wants to measure.
fn campaign() -> Model {
    Model::new(
        meta("utm.campaign", "utm_campaign"),
        vec![
            char("title").required(),
            // materialised: it is how the campaign shows up in every
            // reference, and a list of campaigns reads one column
            char("name").computed(&["title"], campaign_name).store(),
            m2o("user_id", "res.users").required(),
            m2o("stage_id", "utm.stage").required(),
            Field::new(
                "tag_ids",
                FieldType::Many2many {
                    comodel: "utm.tag".into(),
                    relation: "utm_tag_rel".into(),
                    // this end first, the way the framework reads it;
                    // Odoo declares the two swapped, for historical
                    // reasons
                    column1: "campaign_id".into(),
                    column2: "tag_id".into(),
                },
            ),
            // a campanha que um link criou sozinho, para o filtro separar
            // than something somebody sat down and planned
            Field::new("is_auto_campaign", FieldType::Boolean).default_value(json!(false)),
            Field::new("color", FieldType::Integer).default_value(json!(0)),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
        ],
    )
    .constrained("nome preenchido", &["title"], title_is_filled)
}

/// Separa o nome do contador: `"Email [3]"` → `("Email", 3)`.
///
/// A name with no counter is worth 1 — that is how Odoo represents it,
/// and it is the
/// que faz `"Email"` e `"Email [1]"` disputarem a mesma vaga.
fn split_name_and_count(name: &str) -> (&str, u32) {
    let Some(rest) = name.trim_end().strip_suffix(']') else {
        return (name, 1);
    };
    let Some((head, digits)) = rest.rsplit_once('[') else {
        return (name, 1);
    };
    // the bracket has to come after a space: in "A[2]" the number is
    // part of the name, not a counter
    if head.trim_end() == head {
        return (name, 1);
    }
    match digits.parse::<u32>() {
        Ok(count) => (head.trim_end(), count),
        Err(_) => (name, 1),
    }
}

/// The name a record can be born with without repeating one already
/// there: `"Email"` becomes `"Email [2]"` when `"Email"` exists
/// (`_get_unique_names`).
///
/// O contador preenche buracos em vez de sempre ir ao fim — com `"Email"`
/// and `"Email [3]"` taken, the next one is `"Email [2]"`.
fn unique_name(wanted: &str, taken: &[String]) -> String {
    let (base, asked) = split_name_and_count(wanted);
    let used: HashSet<u32> = taken
        .iter()
        .filter_map(|name| {
            let (other, count) = split_name_and_count(name);
            // only the same family counts: "Email [2]" takes Email's 2,
            // "Emails" takes nothing
            (other == base).then_some(count)
        })
        .collect();
    let count = if used.contains(&asked) {
        (1u32..).find(|c| !used.contains(c)).unwrap_or(asked)
    } else {
        asked
    };
    if count > 1 {
        format!("{base} [{count}]")
    } else {
        base.to_string()
    }
}

/// `name_create` — criar digitando um nome numa lista suspensa.
///
/// Odoo numbers the repeated name inside `create`; there is no create
/// hook here, so the counter lives on this path, which is how a client
/// creates a record from a name alone.
fn name_create<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let wanted = wanted_name(&ctx.rest, kwargs)?;
        let id = create_named(&ctx, &wanted, false).await?;
        // a resposta do `name_create` do Odoo: o par que o cliente mostra
        Ok(json!([id, stored_name(&ctx, id).await?]))
    })
}

/// `find_or_create` — the record this name already names, or a new one.
///
/// É o `find_or_create_record` do `utm.mixin`: um link chega com
/// `utm_source=Newsletter` and must not create a second "Newsletter"
/// cada clique.
fn find_or_create<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let wanted = wanted_name(&ctx.rest, kwargs)?;
        if let Some(id) = named_exactly(&ctx, &wanted).await? {
            return Ok(json!({"id": id, "name": stored_name(&ctx, id).await?}));
        }
        // created by the link, not by a person: Odoo marks it this way
        let id = create_named(&ctx, &wanted, true).await?;
        Ok(json!({"id": id, "name": stored_name(&ctx, id).await?}))
    })
}

/// The name the call asked for, without the surrounding spaces.
fn wanted_name(args: &[Value], kwargs: &Map<String, Value>) -> Result<String, RusdooError> {
    let raw = args
        .first()
        .or_else(|| kwargs.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if raw.is_empty() {
        return Err(RusdooError::Validation(
            "say the name of the record to create".into(),
        ));
    }
    Ok(raw.to_string())
}

/// The record whose name is exactly this one, case-insensitively — and
/// contando os arquivados, que continuam ocupando o nome.
async fn named_exactly(ctx: &MethodCtx<'_>, name: &str) -> Result<Option<i64>, RusdooError> {
    let found = ctx
        .registry
        .search(
            ctx.pool,
            ctx.model,
            &parse_domain(&json!([["name", "=ilike", name]]))?,
            &SearchOptions {
                limit: Some(1),
                active_test: false,
                ..SearchOptions::default()
            },
        )
        .await?;
    Ok(found.first().copied())
}

/// Create the record from a name alone, filling in what the model
/// demands and the caller has no way of knowing.
async fn create_named(
    ctx: &MethodCtx<'_>,
    wanted: &str,
    from_link: bool,
) -> Result<i64, RusdooError> {
    let model = ctx.registry.get(ctx.model).ok_or_else(|| {
        RusdooError::Validation(format!("unknown model: {model}", model = ctx.model))
    })?;
    let mut values: Vec<(&str, Value)> = Vec::new();
    if model.field("title").is_some() {
        // a campanha guarda o nome em `title`; o identificador vem dele
        values.push(("title", json!(wanted)));
    } else {
        values.push(("name", json!(available_name(ctx, wanted).await?)));
    }
    if model.field("user_id").is_some() {
        // the owner is whoever is creating it
        // (`default=lambda: self.env.uid`)
        values.push(("user_id", json!(ctx.uid)));
    }
    if model.field("stage_id").is_some() {
        values.push(("stage_id", json!(first_stage(ctx).await?)));
    }
    if from_link && model.field("is_auto_campaign").is_some() {
        values.push(("is_auto_campaign", json!(true)));
    }
    ctx.registry
        .create_as(ctx.pool, ctx.uid, ctx.model, values)
        .await
}

/// The free name closest to the one asked for, given what exists.
async fn available_name(ctx: &MethodCtx<'_>, wanted: &str) -> Result<String, RusdooError> {
    let (base, _) = split_name_and_count(wanted);
    let ids = ctx
        .registry
        .search(
            ctx.pool,
            ctx.model,
            &parse_domain(&json!([["name", "ilike", base]]))?,
            &SearchOptions {
                active_test: false,
                ..SearchOptions::default()
            },
        )
        .await?;
    if ids.is_empty() {
        return Ok(unique_name(wanted, &[]));
    }
    let taken: Vec<String> = ctx
        .registry
        .read(ctx.pool, ctx.model, &ids, &["name"])
        .await?
        .iter()
        .filter_map(|row| row.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    Ok(unique_name(wanted, &taken))
}

/// The first stage, which is the stage a campaign starts in.
///
/// Odoo settles this in a `default`; the defaults here were constants
/// when this was written, and an id is not a constant — so the lookup
/// happens here.
async fn first_stage(ctx: &MethodCtx<'_>) -> Result<i64, RusdooError> {
    let found = ctx
        .registry
        .search(
            ctx.pool,
            "utm.stage",
            &parse_domain(&json!([]))?,
            &SearchOptions {
                order: Some("sequence, id".into()),
                limit: Some(1),
                ..SearchOptions::default()
            },
        )
        .await?;
    found.first().copied().ok_or_else(|| {
        RusdooError::Validation("there is no stage at all: create one before creating campaigns".into())
    })
}

/// The name that was stored, which is not necessarily the one asked
/// for: the counter may have come in, and a campaign's identifier is
/// computed.
async fn stored_name(ctx: &MethodCtx<'_>, id: i64) -> Result<String, RusdooError> {
    let rows = ctx
        .registry
        .read(ctx.pool, ctx.model, &[id], &["name"])
        .await?;
    Ok(rows
        .first()
        .and_then(|row| row.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_counter_is_read_off_the_end_of_the_name() {
        assert_eq!(split_name_and_count("Email"), ("Email", 1));
        assert_eq!(split_name_and_count("Email [3]"), ("Email", 3));
        // with no space before the bracket the number is part of the name
        assert_eq!(split_name_and_count("Email[3]"), ("Email[3]", 1));
        assert_eq!(split_name_and_count("Email [x]"), ("Email [x]", 1));
        assert_eq!(split_name_and_count("[3]"), ("[3]", 1));
    }

    #[test]
    fn a_free_name_is_kept_as_it_was_asked() {
        assert_eq!(unique_name("Email", &[]), "Email");
        // another family takes no slot at all
        assert_eq!(unique_name("Email", &["Emails".into()]), "Email");
    }

    #[test]
    fn a_taken_name_gets_the_first_free_counter() {
        let taken = vec!["Email".to_string(), "Email [3]".to_string()];
        assert_eq!(unique_name("Email", &taken), "Email [2]");
        // asking for 3, which is taken, falls into the first gap
        assert_eq!(unique_name("Email [3]", &taken), "Email [2]");
        // and asking for a free counter is answered as asked
        assert_eq!(unique_name("Email [9]", &taken), "Email [9]");
    }

    #[test]
    fn the_models_register_on_top_of_base() {
        let mut reg = rusdoo_base::registry().expect("base registra");
        extend(&mut reg).expect("utm registra");
        for name in [
            "utm.stage",
            "utm.tag",
            "utm.medium",
            "utm.source",
            "utm.campaign",
        ] {
            assert!(reg.get(name).is_some(), "{name} must be registered");
        }
        // the identifier is materialised: a list of campaigns reads a column
        let campaign = reg.get("utm.campaign").expect("campanha registrada");
        let name = campaign.field("name").expect("campanha tem identificador");
        assert!(name.stored && name.compute.is_some());
        // and `title` is what a person types, so it is the required one
        assert!(campaign.field("title").expect("has a title").required);
        assert!(!name.required, "the identifier is filled by the recompute");
    }

    #[test]
    fn the_three_tracked_models_can_be_created_from_a_name() {
        let mut methods = MethodRegistry::new();
        extend_methods(&mut methods).expect("methods register");
        for model in TRACKING_MODELS {
            assert_eq!(
                methods.names_for(model),
                vec!["find_or_create", "name_create"],
                "{model}"
            );
        }
        // the tag and the stage are tracked by no link
        assert!(methods.names_for("utm.tag").is_empty());
    }
}
