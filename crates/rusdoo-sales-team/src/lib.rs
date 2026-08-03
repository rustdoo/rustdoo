//! rusdoo-sales-team — port of `odoo/addons/sales_team/models/`: who
//! sells, and in which team.
//!
//! Three models and one relationship: a team, the people in it, and the
//! tags every sales module reuses to classify what it sells. The
//! membership is a record of its own (`crm.team.member`) and not a plain
//! many2many, exactly as in Odoo — a membership carries data (when it
//! started, who it is) that a link row cannot hold.
//!
//! Membership is exclusive unless `sales_team.membership_multi` says
//! otherwise, which is Odoo's own switch and its own default.
//!
//! What is deliberately *not* here: the fields `sales_team` adds to
//! `res.users`, which would need `_inherit` across modules; and
//! `crm.team.member_ids`, whose relation table in Odoo *is* the
//! membership table — declaring it here would have the boot create a
//! two-column `crm_team_member` and shadow the real one.

use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::fields::{Field, FieldType, OnDelete};
use rusdoo_orm::methods::{MethodCtx, MethodFuture, MethodRegistry};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};

/// The kanban colour picker has twelve slots, numbered 0 to 11. A record
/// coloured outside them draws no colour at all, which is a card nobody
/// can tell apart from an uncoloured one.
const LAST_COLOR: i64 = 11;

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

/// The id out of a many2one value, which reads as `[id, name]`.
fn first_id(value: &Value) -> Option<i64> {
    match value {
        Value::Array(items) => items.first().and_then(Value::as_i64),
        Value::Number(number) => number.as_i64(),
        _ => None,
    }
}

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    reg.register(team())?;
    reg.register(team_member())?;
    reg.register(tag())?;
    Ok(())
}

/// What a team lets somebody do to it from a screen.
pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    methods.register(
        "crm.team",
        "action_add_member",
        Operation::Write,
        action_add_member,
    )?;
    methods.register(
        "crm.team",
        "action_remove_member",
        Operation::Write,
        action_remove_member,
    )?;
    // pinning is a Read grant on purpose: it only touches the caller's
    // own dashboard, and a salesperson who may see a team but not edit
    // it still gets to choose what shows up on their screen. Odoo says
    // the same thing with `sudo()` inside `_inverse_is_favorite`.
    methods.register(
        "crm.team",
        "action_toggle_favorite",
        Operation::Read,
        action_toggle_favorite,
    )?;
    Ok(())
}

// ---------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------

/// The distinct users behind a team's memberships.
///
/// The dependency arrives as one value per membership, and a many2one
/// reads as `[id, name]` — so this is a projection, not a query.
fn membership_users(record: &Map<String, Value>) -> Vec<i64> {
    let mut users: Vec<i64> = Vec::new();
    for value in record
        .get("crm_team_member_ids.user_id")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(user) = first_id(value) {
            if !users.contains(&user) {
                users.push(user);
            }
        }
    }
    users
}

/// `member_count` — how many salespeople the board shows on a card.
fn member_count(record: &Map<String, Value>) -> Value {
    json!(membership_users(record).len())
}

/// A record whose name is blank: the database accepts `''` where it
/// refuses NULL, and the result is a row nobody can find or click.
fn is_named(record: &Map<String, Value>) -> Result<(), String> {
    match record.get("name").and_then(Value::as_str).map(str::trim) {
        Some(name) if !name.is_empty() => Ok(()),
        _ => Err("dê um nome à equipe ou à etiqueta: um cartão sem nome não é clicável".into()),
    }
}

/// A colour the picker cannot show.
fn is_in_palette(record: &Map<String, Value>) -> Result<(), String> {
    let color = record.get("color").and_then(Value::as_i64).unwrap_or(0);
    if !(0..=LAST_COLOR).contains(&color) {
        return Err(format!(
            "a cor {color} não existe: escolha uma das 12 do quadro, de 0 a {LAST_COLOR}"
        ));
    }
    Ok(())
}

/// `crm.team` — a sales team.
fn team() -> Model {
    Model::new(
        meta("crm.team", "crm_team"),
        vec![
            char("name").required(),
            Field::new("sequence", FieldType::Integer).default_value(json!(10)),
            // a team that stopped selling is archived, never deleted: the
            // orders it signed still point at it
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
            m2o("company_id", "res.company"),
            m2o("user_id", "res.users"),
            // Odoo draws a random colour on create; a random default is
            // not something a data file can reproduce, so a new team is
            // uncoloured until somebody picks
            Field::new("color", FieldType::Integer).default_value(json!(0)),
            Field::new(
                "crm_team_member_ids",
                FieldType::One2many {
                    comodel: "crm.team.member".into(),
                    inverse: "crm_team_id".into(),
                },
            ),
            // not materialized: the number changes when a membership is
            // created, and the framework only recomputes on a write to
            // the team itself — a stored counter would go stale and lie
            Field::new("member_count", FieldType::Integer)
                .computed(&["crm_team_member_ids.user_id"], member_count),
            // who pinned this team to their dashboard. A real relation
            // table, like Odoo's: the link belongs to neither side.
            Field::new(
                "favorite_user_ids",
                FieldType::Many2many {
                    comodel: "res.users".into(),
                    relation: "team_favorite_user_rel".into(),
                    column1: "team_id".into(),
                    column2: "user_id".into(),
                },
            ),
        ],
    )
    .constrained("equipe com nome", &["name"], is_named)
    .constrained("cor da paleta", &["color"], is_in_palette)
}

/// `crm.team.member` — one salesperson in one team.
///
/// Odoo keeps this as a model rather than a link row because a
/// membership is something you look at: it has an owner, a date and a
/// screen of its own.
fn team_member() -> Model {
    Model::new(
        meta("crm.team.member", "crm_team_member"),
        vec![
            m2o("crm_team_id", "crm.team")
                .required()
                .ondelete(OnDelete::Cascade),
            m2o("user_id", "res.users").required(),
            // what the member list shows without a second lookup
            char("name").related("user_id.name"),
            char("email").related("user_id.email"),
            m2o("company_id", "res.company").related("user_id.company_id"),
        ],
    )
}

/// `crm.tag` — the labels every sales module reuses.
fn tag() -> Model {
    Model::new(
        meta("crm.tag", "crm_tag"),
        vec![
            char("name").required(),
            Field::new("color", FieldType::Integer).default_value(json!(0)),
        ],
    )
    .constrained("etiqueta com nome", &["name"], is_named)
    .constrained("cor da paleta", &["color"], is_in_palette)
}

// ---------------------------------------------------------------------
// Methods
// ---------------------------------------------------------------------

/// The single record a team button was pressed on.
fn only_one(ids: &[i64], complaint: &str) -> Result<i64, RusdooError> {
    match ids {
        [id] => Ok(*id),
        _ => Err(RusdooError::Validation(complaint.to_string())),
    }
}

/// Which user the caller means: `user_id` in the kwargs, or the first
/// positional argument — the two shapes a client sends.
fn wanted_user(args: &[Value], kwargs: &Map<String, Value>) -> Result<i64, RusdooError> {
    kwargs
        .get("user_id")
        .or_else(|| args.first())
        .and_then(Value::as_i64)
        .ok_or_else(|| RusdooError::Validation("diga quem: passe user_id com o vendedor".into()))
}

/// A record's name, and a clear complaint when the record is not there.
async fn name_of(ctx: &MethodCtx<'_>, model: &str, id: i64) -> Result<String, RusdooError> {
    let rows = ctx.registry.read(ctx.pool, model, &[id], &["name"]).await?;
    let name = rows
        .first()
        .ok_or_else(|| RusdooError::Validation(format!("{model} {id} não existe")))?
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok(name)
}

/// Take a membership out of the team it belongs to.
///
/// Leaving is a write on the team, not a delete on the membership: the
/// one2many command checks the membership really is that team's before
/// removing it, which a bare unlink by id would not.
async fn leave_team(ctx: &MethodCtx<'_>, team: i64, membership: i64) -> Result<(), RusdooError> {
    ctx.registry
        .write_as(
            ctx.pool,
            ctx.uid,
            "crm.team",
            &[team],
            vec![("crm_team_member_ids", json!([[2, membership, 0]]))],
        )
        .await
}

/// Every membership a user currently has, as `(membership, team)`.
async fn memberships_of(ctx: &MethodCtx<'_>, user: i64) -> Result<Vec<(i64, i64)>, RusdooError> {
    let ids = ctx
        .registry
        .search(
            ctx.pool,
            "crm.team.member",
            &parse_domain(&json!([["user_id", "=", user]]))?,
            &SearchOptions::default(),
        )
        .await?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = ctx
        .registry
        .read(ctx.pool, "crm.team.member", &ids, &["crm_team_id"])
        .await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let membership = row.get("id").and_then(Value::as_i64)?;
            let team = row.get("crm_team_id").and_then(first_id)?;
            Some((membership, team))
        })
        .collect())
}

/// The switch that says whether a salesperson may be in two teams at
/// once (`sales_team.membership_multi`, `res_config_settings.py`).
/// Off out of the box, in Odoo and here.
const MEMBERSHIP_MULTI: &str = "sales_team.membership_multi";

/// `action_add_member` — put a salesperson in this team.
///
/// This is Odoo's `_inverse_member_ids` plus `_synchronize_memberships`,
/// which the port cannot hang off a field write: joining a team means
/// leaving the previous one, unless the database was told otherwise —
/// because a salesperson whose documents could land in two pipelines is,
/// by default, a salesperson nobody can report on.
fn action_add_member<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let team = only_one(&ctx.ids, "adicione membros a uma equipe de cada vez")?;
        let user = wanted_user(&ctx.rest, kwargs)?;
        let team_name = name_of(&ctx, "crm.team", team).await?;
        let user_name = name_of(&ctx, "res.users", user).await?;

        // the refusal comes before the first departure: finding the
        // duplicate halfway through would leave the salesperson out of
        // the team they were in and out of this one too
        let memberships = memberships_of(&ctx, user).await?;
        if memberships.iter().any(|(_, current)| *current == team) {
            return Err(RusdooError::Validation(format!(
                "{user_name} já está na equipe {team_name}"
            )));
        }
        // leaving the previous team is what the switch turns off: with
        // it on, a salesperson serves two pipelines on purpose
        if !ctx
            .registry
            .param_flag(ctx.pool, MEMBERSHIP_MULTI, false)
            .await
        {
            for (membership, current) in memberships {
                leave_team(&ctx, current, membership).await?;
            }
        }

        let membership = ctx
            .registry
            .create_as(
                ctx.pool,
                ctx.uid,
                "crm.team.member",
                vec![("crm_team_id", json!(team)), ("user_id", json!(user))],
            )
            .await?;
        Ok(json!(membership))
    })
}

/// `action_remove_member` — take a salesperson out of this team.
fn action_remove_member<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let team = only_one(&ctx.ids, "remova membros de uma equipe de cada vez")?;
        let user = wanted_user(&ctx.rest, kwargs)?;
        let team_name = name_of(&ctx, "crm.team", team).await?;
        let user_name = name_of(&ctx, "res.users", user).await?;

        let membership = memberships_of(&ctx, user)
            .await?
            .into_iter()
            .find(|(_, current)| *current == team)
            .map(|(membership, _)| membership)
            .ok_or_else(|| {
                RusdooError::Validation(format!("{user_name} não está na equipe {team_name}"))
            })?;
        leave_team(&ctx, team, membership).await?;
        Ok(json!(true))
    })
}

/// `action_toggle_favorite` — pin this team to the caller's dashboard,
/// or unpin it.
///
/// Port of `_inverse_is_favorite`. Whose favourite it is comes from who
/// is calling and never from the arguments: a client that could name the
/// user would be a client that reorganizes somebody else's dashboard.
fn action_toggle_favorite<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let team = only_one(&ctx.ids, "marque uma equipe de cada vez")?;
        let rows = ctx
            .registry
            .read(ctx.pool, "crm.team", &[team], &["favorite_user_ids"])
            .await?;
        let favorites = rows
            .first()
            .ok_or_else(|| RusdooError::Validation(format!("equipe {team} não existe")))?
            .get("favorite_user_ids")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let pinned = favorites
            .iter()
            .filter_map(Value::as_i64)
            .any(|id| id == ctx.uid);
        // command 3 unlinks the pair, command 4 links it
        let command = if pinned { 3 } else { 4 };
        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                "crm.team",
                &[team],
                vec![("favorite_user_ids", json!([[command, ctx.uid, 0]]))],
            )
            .await?;
        Ok(json!(!pinned))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dependency as the ORM hands it over: one many2one pair per
    /// membership.
    fn with_memberships(pairs: Value) -> Map<String, Value> {
        let mut record = Map::new();
        record.insert("crm_team_member_ids.user_id".into(), pairs);
        record
    }

    #[test]
    fn a_team_counts_the_users_of_its_memberships() {
        let record = with_memberships(json!([[7, "Ana"], [9, "Beto"]]));
        assert_eq!(membership_users(&record), vec![7, 9]);
        assert_eq!(member_count(&record), json!(2));
        // a team nobody joined yet counts zero, not null
        assert_eq!(member_count(&Map::new()), json!(0));
    }

    #[test]
    fn the_same_user_twice_is_one_member() {
        // two membership rows for one person (a direct create can leave
        // them behind) must not count as two salespeople
        let record = with_memberships(json!([[7, "Ana"], [7, "Ana"]]));
        assert_eq!(member_count(&record), json!(1));
    }

    #[test]
    fn a_blank_name_is_refused() {
        let mut record = Map::new();
        record.insert("name".into(), json!("   "));
        assert!(is_named(&record).is_err());
        record.insert("name".into(), json!("Vendas Sul"));
        assert!(is_named(&record).is_ok());
    }

    #[test]
    fn a_colour_outside_the_picker_is_refused() {
        let mut record = Map::new();
        for color in [0, 11] {
            record.insert("color".into(), json!(color));
            assert!(is_in_palette(&record).is_ok(), "{color} is on the board");
        }
        record.insert("color".into(), json!(57));
        let error = is_in_palette(&record).expect_err("57 is not a colour");
        assert!(error.contains("de 0 a 11"), "{error}");
    }

    #[test]
    fn the_models_register_on_top_of_base() {
        let mut reg = rusdoo_base::registry().unwrap();
        extend(&mut reg).unwrap();
        for name in ["crm.team", "crm.team.member", "crm.tag"] {
            assert!(reg.get(name).is_some(), "{name} must be registered");
        }
        // a membership without a team or a user is not a membership
        let member = reg.get("crm.team.member").unwrap();
        assert!(member.field("crm_team_id").unwrap().required);
        assert!(member.field("user_id").unwrap().required);
        // the dashboard link is a real relation table, not a compute
        let favorites = reg
            .get("crm.team")
            .unwrap()
            .field("favorite_user_ids")
            .unwrap();
        assert!(matches!(
            favorites.ty,
            FieldType::Many2many { ref relation, .. } if relation == "team_favorite_user_rel"
        ));
        assert!(favorites.compute.is_none());
    }

    #[test]
    fn a_team_has_its_three_buttons() {
        let mut methods = MethodRegistry::new();
        extend_methods(&mut methods).unwrap();
        assert_eq!(
            methods.names_for("crm.team"),
            vec![
                "action_add_member",
                "action_remove_member",
                "action_toggle_favorite"
            ]
        );
    }
}
