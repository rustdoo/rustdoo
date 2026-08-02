//! Access control, port of `ir.model.access` (`odoo/addons/base/models/
//! ir_model.py`): per-model CRUD permissions granted through groups.
//!
//! Semantics mirror `ir.model.access.check` and are **fail-closed**:
//! - the superuser (uid 1) bypasses every check;
//! - every other user is denied an operation on a model unless one of
//!   their groups explicitly grants it.
//!
//! An empty table therefore locks every model to the superuser — the
//! same safe default as an Odoo install whose `ir.model.access.csv`
//! rows have not been loaded, never the fail-open "open to everyone".

use crate::db::parse_commands;
use crate::fields::FieldType;
use crate::registry::Registry;
use rusdoo_core::RusdooError;
use serde_json::{Map, Value};
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};

/// How deep nested command values may reach. The tree is client-supplied,
/// so the walk is bounded like every other client-controlled recursion.
const MAX_COMMAND_DEPTH: usize = 8;

/// The four CRUD permissions of `ir.model.access`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operation {
    Read,
    Write,
    Create,
    Unlink,
}

impl Operation {
    /// The CRUD operation implied by an ORM method name, if any.
    pub fn for_method(method: &str) -> Option<Operation> {
        Some(match method {
            "search"
            | "search_read"
            | "search_count"
            | "read"
            | "fields_get"
            | "default_get"
            | "web_read"
            | "web_search_read"
            | "name_search"
            | "web_name_search"
            | "formatted_read_group"
            | "web_read_group"
            | "get_views"
            | "onchange" => Operation::Read,
            "create" => Operation::Create,
            "write" => Operation::Write,
            "unlink" => Operation::Unlink,
            _ => return None,
        })
    }

    fn label(self) -> &'static str {
        match self {
            Operation::Read => "read",
            Operation::Write => "write",
            Operation::Create => "create",
            Operation::Unlink => "unlink",
        }
    }
}

/// Where the grants live between boots. The columns are Odoo's own, so
/// a row reads the same here as in an `ir.model.access.csv`; `module` is
/// what lets a re-install replace exactly the rows it owns.
const IR_MODEL_ACCESS_DDL: &str = r#"CREATE TABLE IF NOT EXISTS "ir_model_access" ("id" SERIAL NOT NULL, "module" varchar NOT NULL, "model" varchar NOT NULL, "group_id" int4 NOT NULL, "perm_read" bool NOT NULL DEFAULT false, "perm_write" bool NOT NULL DEFAULT false, "perm_create" bool NOT NULL DEFAULT false, "perm_unlink" bool NOT NULL DEFAULT false, PRIMARY KEY("id"))"#;

/// A row of `ir.model.access` as the database holds it.
type AccessRow = (String, i32, bool, bool, bool, bool);

/// One grant: what a group may do on a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub model: String,
    pub group_id: i64,
    pub operations: Vec<Operation>,
}

fn db_err(e: sqlx::Error) -> RusdooError {
    RusdooError::Database(e.to_string())
}

/// In-memory ACL table. Rules are `(model, operation) -> {group ids}`.
#[derive(Debug, Default, Clone)]
pub struct AccessControl {
    grants: HashMap<(String, Operation), HashSet<i64>>,
}

impl AccessControl {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether any rule has been loaded at all (used to warn operators
    /// that non-superusers are locked out until ACL data is present).
    pub fn is_empty(&self) -> bool {
        self.grants.is_empty()
    }

    /// Grant `group_id` the given operations on `model`.
    pub fn grant(&mut self, model: &str, group_id: i64, operations: &[Operation]) {
        for op in operations {
            self.grants
                .entry((model.to_string(), *op))
                .or_default()
                .insert(group_id);
        }
    }

    /// The grants of this table, one row per (model, group).
    pub fn rows(&self) -> Vec<Grant> {
        let mut by_target: HashMap<(&str, i64), Vec<Operation>> = HashMap::new();
        for ((model, op), groups) in &self.grants {
            for group in groups {
                by_target
                    .entry((model.as_str(), *group))
                    .or_default()
                    .push(*op);
            }
        }
        by_target
            .into_iter()
            .map(|((model, group_id), operations)| Grant {
                model: model.to_string(),
                group_id,
                operations,
            })
            .collect()
    }

    /// Load the table from the database, creating it if this is the
    /// first boot. This is what makes the ACL survive a restart: a
    /// server that comes up without re-installing its addons still knows
    /// who may do what.
    pub async fn load(pool: &PgPool) -> Result<Self, RusdooError> {
        sqlx::query(IR_MODEL_ACCESS_DDL)
            .execute(pool)
            .await
            .map_err(db_err)?;
        let rows: Vec<AccessRow> = sqlx::query_as(
            r#"SELECT "model", "group_id", "perm_read", "perm_write", "perm_create", "perm_unlink"
               FROM "ir_model_access""#,
        )
        .fetch_all(pool)
        .await
        .map_err(db_err)?;
        let mut access = AccessControl::new();
        for (model, group_id, read, write, create, unlink) in rows {
            let mut ops = Vec::new();
            for (granted, op) in [
                (read, Operation::Read),
                (write, Operation::Write),
                (create, Operation::Create),
                (unlink, Operation::Unlink),
            ] {
                if granted {
                    ops.push(op);
                }
            }
            access.grant(&model, i64::from(group_id), &ops);
        }
        Ok(access)
    }

    /// Replace the grants a module owns with `grants`, in one
    /// transaction: an install that fails halfway must not leave a
    /// half-open ACL behind.
    pub async fn persist_module(
        pool: &PgPool,
        module: &str,
        grants: &[Grant],
    ) -> Result<(), RusdooError> {
        sqlx::query(IR_MODEL_ACCESS_DDL)
            .execute(pool)
            .await
            .map_err(db_err)?;
        let mut tx = pool.begin().await.map_err(db_err)?;
        sqlx::query(r#"DELETE FROM "ir_model_access" WHERE "module" = $1"#)
            .bind(module)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        for grant in grants {
            sqlx::query(
                r#"INSERT INTO "ir_model_access"
                   ("module", "model", "group_id", "perm_read", "perm_write",
                    "perm_create", "perm_unlink")
                   VALUES ($1, $2, $3, $4, $5, $6, $7)"#,
            )
            .bind(module)
            .bind(&grant.model)
            .bind(grant.group_id as i32)
            .bind(grant.operations.contains(&Operation::Read))
            .bind(grant.operations.contains(&Operation::Write))
            .bind(grant.operations.contains(&Operation::Create))
            .bind(grant.operations.contains(&Operation::Unlink))
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)
    }

    /// Check whether a user in `groups` may perform `op` on `model`.
    /// Fail-closed: denied unless a group grants it (superuser bypasses).
    pub fn check(
        &self,
        model: &str,
        op: Operation,
        groups: &[i64],
        is_superuser: bool,
    ) -> Result<(), RusdooError> {
        if is_superuser {
            return Ok(());
        }
        let allowed = self
            .grants
            .get(&(model.to_string(), op))
            .is_some_and(|granted| groups.iter().any(|g| granted.contains(g)));
        if allowed {
            Ok(())
        } else {
            Err(RusdooError::Access(format!(
                "you are not allowed to {} on {model}",
                op.label()
            )))
        }
    }
}

/// The `(comodel, operation)` pairs implied by the x2many command tuples
/// inside create/write values, nested values included.
///
/// A relational write reaches records in another model: Odoo performs
/// those creates/writes/unlinks on the comodel and `ir.model.access`
/// checks them there. Checking only the called model would let a user who
/// may write on an order create, rewrite or delete rows of a line model
/// they have no rights on at all.
///
/// Unknown field names are skipped: the write path itself rejects them,
/// and an ACL walk must not be the thing that decides what a field is.
pub fn x2many_operations(
    registry: &Registry,
    model_name: &str,
    values: &Map<String, Value>,
) -> Result<Vec<(String, Operation)>, RusdooError> {
    let mut found = HashSet::new();
    collect_x2many_operations(registry, model_name, values, 0, &mut found)?;
    Ok(found.into_iter().collect())
}

fn collect_x2many_operations(
    registry: &Registry,
    model_name: &str,
    values: &Map<String, Value>,
    depth: usize,
    found: &mut HashSet<(String, Operation)>,
) -> Result<(), RusdooError> {
    if depth > MAX_COMMAND_DEPTH {
        return Err(RusdooError::Validation(format!(
            "x2many command values nest deeper than {MAX_COMMAND_DEPTH} levels"
        )));
    }
    let model = registry
        .get(model_name)
        .ok_or_else(|| RusdooError::Validation(format!("unknown model: {model_name}")))?;
    for (name, value) in values {
        let Some(field) = model.field(name) else {
            continue;
        };
        let (comodel, is_one2many) = match &field.ty {
            FieldType::One2many { comodel, .. } => (comodel, true),
            FieldType::Many2many { comodel, .. } => (comodel, false),
            _ => continue,
        };
        for command in parse_commands(value)? {
            let Some(arr) = command.as_array() else {
                continue;
            };
            let Some(code) = arr.first().and_then(Value::as_i64) else {
                continue;
            };
            let operation = match code {
                0 => Some(Operation::Create),
                1 => Some(Operation::Write),
                2 => Some(Operation::Unlink),
                // link/unlink/clear/set rewrite the child's inverse column
                // on a one2many, which is a write on the comodel; on a
                // many2many only relation rows move, and those belong to
                // the field — covered by write access on this model
                3..=6 if is_one2many => Some(Operation::Write),
                _ => None,
            };
            if let Some(operation) = operation {
                found.insert((comodel.clone(), operation));
            }
            // create/update carry values that may themselves hold commands
            if matches!(code, 0 | 1) {
                if let Some(Value::Object(nested)) = arr.get(2) {
                    collect_x2many_operations(registry, comodel, nested, depth + 1, found)?;
                }
            }
        }
    }
    Ok(())
}
