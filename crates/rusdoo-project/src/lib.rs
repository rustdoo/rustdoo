//! rusdoo-project — port of `odoo/addons/project/`: the work, who is
//! doing it, and whether it is done.
//!
//! A **project** is a bag of tasks with a customer and a manager. A
//! **task** is one piece of work, and it lives in two dimensions at
//! once, which is the part worth getting right: a **stage** — the column
//! it sits in, which every team names differently — and a **state**,
//! which is the handful of answers every team means the same thing by:
//! in progress, waiting on somebody, done, cancelled.
//!
//! Odoo keeps both because the stage is the team's vocabulary and the
//! state is the report's. A task in "Sprint 3" tells you nothing across
//! projects; `state = '1_done'` tells you the same thing everywhere.
//!
//! ## What is deliberately not here
//!
//! * **Sub-tasks.** `parent_id` and the rolled-up allocated hours are a
//!   tree walk with its own recursion rules and a display flag
//!   (`display_in_project`) that decides which of them show on the
//!   board. It is a slice of its own, and half of it would be worse than
//!   none: an allocated-hours total that silently ignores children is a
//!   number a manager plans with.
//! * **Timesheets**, which are `hr_timesheet`'s, and the burndown and
//!   milestone screens over them.
//! * **The portal.** Sharing a project with a customer is `portal`'s
//!   access rules over an axis this port has not started.

use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::defaults;
use rusdoo_orm::fields::{Field, FieldType, OnDelete};
use rusdoo_orm::methods::{MethodCtx, MethodFuture, MethodRegistry};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};

/// The states Odoo gives every task, with its own prefixes: they sort,
/// and the sort is the order work moves in.
const STATES: [(&str, &str); 5] = [
    ("01_in_progress", "In Progress"),
    ("02_changes_requested", "Changes Requested"),
    ("03_approved", "Approved"),
    ("1_done", "Done"),
    ("1_canceled", "Cancelled"),
];

/// The two states that mean nobody is working on it any more.
const CLOSED: [&str; 2] = ["1_done", "1_canceled"];

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    reg.register(stage())?;
    reg.register(tags())?;
    reg.register(project())?;
    reg.register(task())?;
    Ok(())
}

pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    methods.register("project.task", "action_done", Operation::Write, action_done)?;
    methods.register(
        "project.task",
        "action_cancel",
        Operation::Write,
        action_cancel,
    )?;
    Ok(())
}

fn meta(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.to_string(),
        table: table.to_string(),
        inherit: vec![],
        inherits: vec![],
    }
}

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

fn selection(name: &str, options: &[(&str, &str)]) -> Field {
    Field::new(
        name,
        FieldType::Selection(
            options
                .iter()
                .map(|(v, l)| ((*v).to_string(), (*l).to_string()))
                .collect(),
        ),
    )
}

fn gathered(record: &Map<String, Value>, key: &str) -> Vec<Value> {
    record
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

// ── computes ────────────────────────────────────────────────────────

fn task_count(record: &Map<String, Value>) -> Value {
    json!(gathered(record, "tasks").len() as i64)
}

/// How much work is still on somebody's plate — the number that decides
/// whether a project needs attention today.
fn open_task_count(record: &Map<String, Value>) -> Value {
    let states = gathered(record, "tasks.state");
    json!(states
        .iter()
        .filter(|state| {
            state
                .as_str()
                .is_none_or(|state| !CLOSED.contains(&state))
        })
        .count() as i64)
}

/// `is_closed` — whether the task is one nobody is working on any more.
fn is_closed(record: &Map<String, Value>) -> Value {
    json!(record
        .get("state")
        .and_then(Value::as_str)
        .is_some_and(|state| CLOSED.contains(&state)))
}

/// Allocated time below zero is not an estimate, it is a typo that will
/// be summed into a plan.
fn allocated_time_is_not_negative(record: &Map<String, Value>) -> Result<(), String> {
    let hours = record
        .get("allocated_hours")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if hours < 0.0 {
        return Err("allocated time cannot be negative".into());
    }
    Ok(())
}

// ── models ──────────────────────────────────────────────────────────

/// `project.task.type` — a column of a project's board. Odoo shares
/// stages between projects through a many2many, which is why a stage is
/// not owned by one.
fn stage() -> Model {
    Model::new(
        meta("project.task.type", "project_task_type"),
        vec![
            char("name").required().translatable(),
            Field::new("sequence", FieldType::Integer).default_value(json!(1)),
            Field::new("fold", FieldType::Boolean).default_value(json!(false)),
            Field::new("description", FieldType::Text),
            Field::new("color", FieldType::Integer),
            Field::new(
                "project_ids",
                FieldType::Many2many {
                    comodel: "project.project".into(),
                    relation: "project_task_type_rel".into(),
                    column1: "type_id".into(),
                    column2: "project_id".into(),
                },
            ),
        ],
    )
    .ordered("sequence, id")
}

/// `project.tags` — any other way somebody slices the work.
fn tags() -> Model {
    Model::new(
        meta("project.tags", "project_tags"),
        vec![
            char("name").required().translatable(),
            Field::new("color", FieldType::Integer),
        ],
    )
    .ordered("name, id")
}

/// `project.project` — a bag of tasks with a customer and a manager.
fn project() -> Model {
    Model::new(
        meta("project.project", "project_project"),
        vec![
            char("name").required().translatable(),
            Field::new("description", FieldType::Html),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
            Field::new("sequence", FieldType::Integer).default_value(json!(10)),
            m2o("partner_id", "res.partner"),
            m2o("company_id", "res.company").default_from(defaults::USER_COMPANY),
            m2o("user_id", "res.users").default_from(defaults::CURRENT_USER),
            // Odoo lets a project name its unit of work ("Tickets",
            // "Sprints"); the screens read this rather than hardcoding
            char("label_tasks").default_value(json!("Tarefas")),
            Field::new("date_start", FieldType::Date),
            Field::new("date", FieldType::Date),
            Field::new(
                "tasks",
                FieldType::One2many {
                    comodel: "project.task".into(),
                    inverse: "project_id".into(),
                },
            ),
            Field::new(
                "type_ids",
                FieldType::Many2many {
                    comodel: "project.task.type".into(),
                    relation: "project_task_type_rel".into(),
                    column1: "project_id".into(),
                    column2: "type_id".into(),
                },
            ),
            // neither is materialised: both move when a *task* is
            // written, and a recompute only follows a write to the record
            // it is on
            Field::new("task_count", FieldType::Integer).computed(&["tasks"], task_count),
            Field::new("open_task_count", FieldType::Integer)
                .computed(&["tasks.state"], open_task_count),
            Field::new("color", FieldType::Integer),
        ],
    )
    .ordered("sequence, name, id")
}

/// `project.task` — one piece of work, in two dimensions.
fn task() -> Model {
    Model::new(
        meta("project.task", "project_task"),
        vec![
            char("name").required(),
            Field::new("description", FieldType::Html),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
            Field::new("sequence", FieldType::Integer).default_value(json!(10)),
            // the task of a deleted project goes with it: a piece of work
            // with no project is a row nobody can find again
            m2o("project_id", "project.project").ondelete(OnDelete::Cascade),
            m2o("stage_id", "project.task.type"),
            selection("state", &STATES)
                .required()
                .default_value(json!("01_in_progress")),
            Field::new("is_closed", FieldType::Boolean).computed(&["state"], is_closed),
            selection(
                "priority",
                &[("0", "Normal"), ("1", "Important")],
            )
            .default_value(json!("0")),
            m2o("user_id", "res.users").default_from(defaults::CURRENT_USER),
            m2o("partner_id", "res.partner"),
            m2o("company_id", "res.company").default_from(defaults::USER_COMPANY),
            Field::new("date_deadline", FieldType::Datetime),
            Field::new("date_end", FieldType::Datetime),
            Field::new("allocated_hours", FieldType::Float { digits: Some((16, 2)) })
                .default_value(json!(0.0)),
            Field::new(
                "tag_ids",
                FieldType::Many2many {
                    comodel: "project.tags".into(),
                    relation: "project_tags_rel".into(),
                    column1: "task_id".into(),
                    column2: "tag_id".into(),
                },
            ),
            Field::new("color", FieldType::Integer),
        ],
    )
    .constrained(
        "allocated time is not negative",
        &["allocated_hours"],
        allocated_time_is_not_negative,
    )
    // Odoo's own: what somebody flagged, then the order they arranged,
    // then what is due soonest
    .ordered("priority desc, sequence, date_deadline, id desc")
}

// ── methods ─────────────────────────────────────────────────────────

/// Move `ids` to a closed state, stamping when it happened.
async fn close(ctx: &MethodCtx<'_>, state: &str) -> Result<Value, RusdooError> {
    if ctx.ids.is_empty() {
        return Err(RusdooError::Validation(
            "say which task to close".into(),
        ));
    }
    let now = chrono::Utc::now()
        .format(defaults::DATETIME_FORMAT)
        .to_string();
    ctx.registry
        .write_as(
            ctx.pool,
            ctx.uid,
            "project.task",
            &ctx.ids,
            vec![("state", json!(state)), ("date_end", json!(now))],
        )
        .await?;
    Ok(json!(true))
}

/// The task is finished.
fn action_done<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move { close(&ctx, "1_done").await })
}

/// The task will not be done, which is a different thing from done and
/// every report needs them apart.
fn action_cancel<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move { close(&ctx, "1_canceled").await })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(pairs: Value) -> Map<String, Value> {
        pairs.as_object().expect("an object").clone()
    }

    #[test]
    fn what_is_open_is_what_nobody_has_closed() {
        let row = record(json!({
            "tasks.state": ["01_in_progress", "1_done", "1_canceled", "03_approved"],
        }));
        // done and cancelled are off the plate; approved is still work
        assert_eq!(open_task_count(&row), json!(2));
    }

    #[test]
    fn a_task_knows_whether_anybody_is_still_on_it() {
        for (state, closed) in [
            ("01_in_progress", false),
            ("03_approved", false),
            ("1_done", true),
            ("1_canceled", true),
        ] {
            let row = record(json!({"state": state}));
            assert_eq!(is_closed(&row), json!(closed), "{state}");
        }
    }

    #[test]
    fn the_two_dimensions_are_both_there() {
        let mut reg = rusdoo_base::registry().unwrap();
        extend(&mut reg).unwrap();
        let task = reg.get("project.task").expect("registered");
        // the team's vocabulary...
        assert!(task.field("stage_id").is_some());
        // ...and the report's
        let state = task.field("state").expect("declared");
        assert!(state.required);
        match &state.ty {
            FieldType::Selection(options) => assert_eq!(options.len(), STATES.len()),
            other => panic!("state is a {other:?}"),
        }
    }

    #[test]
    fn an_estimate_below_zero_is_a_typo_somebody_would_plan_with() {
        let row = record(json!({"allocated_hours": -3.0}));
        assert!(allocated_time_is_not_negative(&row).is_err());
        assert!(allocated_time_is_not_negative(&record(json!({}))).is_ok());
    }
}
