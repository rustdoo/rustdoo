//! rusdoo-hr — port of `odoo/addons/hr/`: the people the company works
//! with, and the shape of the organisation they work in.
//!
//! Four models. An **employee** is a person the company employs; a
//! **department** is where they sit and who they answer to; a **job
//! position** is the seat itself, occupied or still being recruited for;
//! and a **tag** is any other way somebody wants to slice the list.
//!
//! An employee is also a *resource*: something that can be scheduled,
//! with working hours and a timezone. That is not decoration — it is
//! what lets planning, timesheets and manufacturing ask when this person
//! is available without knowing anything about human resources. Here the
//! employee delegates to `resource.resource`
//! (`_inherits = {'resource.resource': 'resource_id'}`), so creating one
//! creates its resource and the name is one value on both.
//!
//! ## Where this differs from Odoo 19, and why
//!
//! * **The delegation is written as a delegation.** Odoo 19 spells the
//!   same relationship as `resource.mixin`: a required `resource_id`
//!   plus a `create` that makes the resource by hand, and related fields
//!   that mirror it. This ORM has no create hook to put that `create` in
//!   — so the relationship is declared instead of performed, which is
//!   how Odoo itself wrote it up to version 13 and what its mixin still
//!   simulates.
//! * **No `hr.version`.** Odoo 19 moved everything about the *terms* of
//!   employment — wage, contract type, working hours, the job on the day
//!   — into `hr.version`, a record per change, and made the employee
//!   delegate to the current one. That is a second delegation and a
//!   whole model of its own; until contracts land here, an employee
//!   carries the terms directly, which is one version: the current one.
//! * **No private information split.** Odoo hides addresses, bank
//!   accounts and identification behind `hr.group_hr_user` with
//!   `groups=` on each field. Field-level access does not exist in this
//!   ORM, and half-hidden private data is worse than none: those fields
//!   are not ported yet rather than ported into the open.

use rusdoo_core::RusdooError;
use rusdoo_orm::defaults;
use rusdoo_orm::fields::{Field, FieldType, OnDelete};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};

/// Registration order is the dependency order: a department points at an
/// employee, an employee at a department, and the ORM resolves the
/// comodel of a many2one when it is used, not when it is declared — but
/// `_inherits` is checked at registration, so `resource.resource` must
/// already be there.
pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    reg.register(department())?;
    reg.register(job())?;
    reg.register(category())?;
    reg.register(employee())?;
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

fn o2m(name: &str, comodel: &str, inverse: &str) -> Field {
    Field::new(
        name,
        FieldType::One2many {
            comodel: comodel.to_string(),
            inverse: inverse.to_string(),
        },
    )
}

fn selection(name: &str, options: &[(&str, &str)]) -> Field {
    Field::new(
        name,
        FieldType::Selection(
            options
                .iter()
                .map(|(value, label)| ((*value).to_string(), (*label).to_string()))
                .collect(),
        ),
    )
}

// ── computes ────────────────────────────────────────────────────────

/// The ids a one2many dependency arrives as.
fn gathered(record: &Map<String, Value>, key: &str) -> Vec<Value> {
    record
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// `complete_name` — "Board / Sales / Field", the name a department is
/// known by once there is more than one level of them.
///
/// A dependency through a link arrives as a one-element list, and the
/// parent's own `complete_name` is computed the same way — so the whole
/// chain resolves by the read walking it, one hop per level.
fn complete_name(record: &Map<String, Value>) -> Value {
    let name = record.get("name").and_then(Value::as_str).unwrap_or_default();
    let parent = record
        .get("parent_id.complete_name")
        .and_then(Value::as_array)
        .and_then(|found| found.first())
        .and_then(Value::as_str)
        .filter(|parent| !parent.is_empty());
    match parent {
        Some(parent) => json!(format!("{parent} / {name}")),
        None => json!(name),
    }
}

/// How many people the department has, which is the number a manager
/// looks at before anything else on the screen.
fn total_employee(record: &Map<String, Value>) -> Value {
    json!(gathered(record, "member_ids").len() as i64)
}

/// How many people hold this job today.
fn no_of_employee(record: &Map<String, Value>) -> Value {
    json!(gathered(record, "employee_ids").len() as i64)
}

/// What the job will hold once recruitment is done: the people in it now
/// plus the ones still being looked for. Odoo's `expected_employees`.
fn expected_employees(record: &Map<String, Value>) -> Value {
    let current = gathered(record, "employee_ids").len() as i64;
    let wanted = record
        .get("no_of_recruitment")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    json!(current + wanted)
}

// ── constraints ─────────────────────────────────────────────────────

/// Odoo's `_check_parent_id`: a department cannot be its own ancestor.
///
/// Only the first link is checked here, which is the case somebody
/// actually creates by hand; a longer loop needs the walk that
/// `parent_path` exists for in Odoo, and the port has no such column
/// yet. What this does catch is the mistake a form makes possible.
fn a_department_is_not_its_own_parent(record: &Map<String, Value>) -> Result<(), String> {
    let id = record.get("id").and_then(Value::as_i64);
    let parent = record
        .get("parent_id")
        .and_then(|value| match value {
            Value::Array(pair) => pair.first().and_then(Value::as_i64),
            other => other.as_i64(),
        });
    match (id, parent) {
        (Some(id), Some(parent)) if id == parent => {
            Err("a department cannot report to itself".into())
        }
        _ => Ok(()),
    }
}

/// The same for the reporting line: nobody is their own manager.
fn nobody_manages_themselves(record: &Map<String, Value>) -> Result<(), String> {
    let id = record.get("id").and_then(Value::as_i64);
    let manager = record
        .get("parent_id")
        .and_then(|value| match value {
            Value::Array(pair) => pair.first().and_then(Value::as_i64),
            other => other.as_i64(),
        });
    match (id, manager) {
        (Some(id), Some(manager)) if id == manager => {
            Err("an employee cannot be their own manager".into())
        }
        _ => Ok(()),
    }
}

// ── models ──────────────────────────────────────────────────────────

/// `hr.department` — where somebody sits, and who they answer to.
fn department() -> Model {
    Model::new(
        meta("hr.department", "hr_department"),
        vec![
            char("name").required().translatable(),
            char("complete_name").computed(&["name", "parent_id.complete_name"], complete_name),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
            m2o("company_id", "res.company").default_from(defaults::USER_COMPANY),
            // a department whose parent goes does not go with it: the
            // people in it are still employed, and Odoo says the same by
            // leaving the reference to be emptied
            m2o("parent_id", "hr.department").ondelete(OnDelete::SetNull),
            o2m("child_ids", "hr.department", "parent_id"),
            m2o("manager_id", "hr.employee"),
            o2m("member_ids", "hr.employee", "department_id"),
            o2m("jobs_ids", "hr.job", "department_id"),
            // not materialised: the count changes when an *employee* is
            // written, and a recompute only follows a write to the
            // department itself
            Field::new("total_employee", FieldType::Integer)
                .computed(&["member_ids"], total_employee),
            Field::new("note", FieldType::Text),
            Field::new("color", FieldType::Integer),
        ],
    )
    .constrained(
        "a department is not its own parent",
        &["parent_id"],
        a_department_is_not_its_own_parent,
    )
    .ordered("name, id")
}

/// `hr.job` — the seat, whether or not anybody is in it.
fn job() -> Model {
    Model::new(
        meta("hr.job", "hr_job"),
        vec![
            char("name").required().translatable(),
            Field::new("sequence", FieldType::Integer).default_value(json!(10)),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
            m2o("department_id", "hr.department"),
            m2o("company_id", "res.company").default_from(defaults::USER_COMPANY),
            Field::new("description", FieldType::Html),
            Field::new("requirements", FieldType::Text),
            // Odoo's `no_of_recruitment`: how many more are wanted
            Field::new("no_of_recruitment", FieldType::Integer).default_value(json!(0)),
            o2m("employee_ids", "hr.employee", "job_id"),
            Field::new("no_of_employee", FieldType::Integer)
                .computed(&["employee_ids"], no_of_employee),
            Field::new("expected_employees", FieldType::Integer)
                .computed(&["employee_ids", "no_of_recruitment"], expected_employees),
        ],
    )
    .ordered("sequence, name, id")
}

/// `hr.employee.category` — any other way to slice the list.
fn category() -> Model {
    Model::new(
        meta("hr.employee.category", "hr_employee_category"),
        vec![
            char("name").required().translatable(),
            Field::new("color", FieldType::Integer),
            Field::new(
                "employee_ids",
                FieldType::Many2many {
                    comodel: "hr.employee".into(),
                    relation: "employee_category_rel".into(),
                    column1: "category_id".into(),
                    column2: "emp_id".into(),
                },
            ),
        ],
    )
    .ordered("name, id")
}

/// `hr.employee` — a person the company employs, and a resource that can
/// be scheduled.
///
/// The name, the company, the timezone and the working hours belong to
/// the resource and are reached through the delegation: an employee
/// renamed is a resource renamed, and a planning module that only knows
/// about resources sees the change.
fn employee() -> Model {
    Model::new(
        ModelMeta {
            name: "hr.employee".into(),
            table: "hr_employee".into(),
            inherit: vec![],
            inherits: vec![("resource.resource".into(), "resource_id".into())],
        },
        vec![
            // `ondelete='restrict'`, like Odoo's mixin: a resource that
            // still has an employee is not a resource anybody may delete
            m2o("resource_id", "resource.resource")
                .required()
                .ondelete(OnDelete::Restrict),
            // what they are called at work, which is not always the job
            // position they hold
            char("job_title"),
            m2o("job_id", "hr.job"),
            m2o("department_id", "hr.department"),
            // Odoo's `parent_id` on an employee is the manager, not a
            // parent record — the name is theirs and worth keeping
            m2o("parent_id", "hr.employee"),
            m2o("coach_id", "hr.employee"),
            char("work_email"),
            char("work_phone"),
            char("mobile_phone"),
            char("work_location"),
            m2o("address_id", "res.partner"),
            selection(
                "employee_type",
                &[
                    ("employee", "Employee"),
                    ("student", "Trainee"),
                    ("trainee", "Intern"),
                    ("contractor", "Contractor"),
                    ("freelance", "Freelancer"),
                ],
            )
            .required()
            .default_value(json!("employee")),
            char("barcode"),
            Field::new(
                "category_ids",
                FieldType::Many2many {
                    comodel: "hr.employee.category".into(),
                    relation: "employee_category_rel".into(),
                    column1: "emp_id".into(),
                    column2: "category_id".into(),
                },
            ),
            Field::new("image_1920", FieldType::Binary),
            Field::new("note", FieldType::Text),
        ],
    )
    .constrained(
        "nobody manages themselves",
        &["parent_id"],
        nobody_manages_themselves,
    )
    // Odoo orders employees by name, which is the resource's — and a
    // delegated column is one the query can reach
    .ordered("name, id")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Registry {
        let mut reg = rusdoo_base::registry().unwrap();
        rusdoo_resource::extend(&mut reg).unwrap();
        extend(&mut reg).unwrap();
        reg
    }

    #[test]
    fn an_employee_is_a_resource_and_says_so() {
        let reg = registry();
        let employee = reg.get("hr.employee").expect("registered");
        assert_eq!(
            employee.meta.inherits,
            vec![("resource.resource".to_string(), "resource_id".to_string())]
        );
        // the name is not the employee's own column: it is the
        // resource's, reached through the link
        assert!(employee.field("name").is_none());
        assert!(reg.field_of(employee, "name").is_some());
        // and so are the working hours a planning module would ask about
        assert!(reg.field_of(employee, "calendar_id").is_some());
        assert!(reg.field_of(employee, "tz").is_some());
    }

    #[test]
    fn a_department_reads_its_way_up_the_tree() {
        let mut record = Map::new();
        record.insert("name".into(), json!("Vendas"));
        assert_eq!(complete_name(&record), json!("Vendas"));
        record.insert("parent_id.complete_name".into(), json!(["Diretoria"]));
        assert_eq!(complete_name(&record), json!("Diretoria / Vendas"));
        // a parent with no name of its own does not leave a dangling
        // separator on the screen
        record.insert("parent_id.complete_name".into(), json!([""]));
        assert_eq!(complete_name(&record), json!("Vendas"));
    }

    #[test]
    fn a_job_counts_who_holds_it_and_who_is_still_wanted() {
        let mut record = Map::new();
        record.insert("employee_ids".into(), json!([1, 2]));
        record.insert("no_of_recruitment".into(), json!(3));
        assert_eq!(no_of_employee(&record), json!(2));
        assert_eq!(expected_employees(&record), json!(5));
        // an empty seat is still a seat somebody is looking to fill
        let mut empty = Map::new();
        empty.insert("no_of_recruitment".into(), json!(1));
        assert_eq!(no_of_employee(&empty), json!(0));
        assert_eq!(expected_employees(&empty), json!(1));
    }

    #[test]
    fn the_loops_a_form_makes_possible_are_refused() {
        let mut record = Map::new();
        record.insert("id".into(), json!(7));
        record.insert("parent_id".into(), json!([7, "Vendas"]));
        assert!(a_department_is_not_its_own_parent(&record).is_err());
        assert!(nobody_manages_themselves(&record).is_err());
        record.insert("parent_id".into(), json!([9, "Diretoria"]));
        assert!(a_department_is_not_its_own_parent(&record).is_ok());
        assert!(nobody_manages_themselves(&record).is_ok());
    }
}
