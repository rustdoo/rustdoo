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

use rusdoo_core::RusdooError;
use std::collections::{HashMap, HashSet};

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
            "search" | "search_read" | "search_count" | "read" | "fields_get" | "default_get"
            | "web_read" | "web_search_read" => Operation::Read,
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
