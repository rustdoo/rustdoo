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
            | "web_read_group" => Operation::Read,
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
