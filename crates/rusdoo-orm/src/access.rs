//! Access control, port of `ir.model.access` (`odoo/addons/base/models/
//! ir_model.py`): per-model CRUD permissions granted through groups.
//!
//! Semantics mirror `ir.model.access.check`:
//! - the superuser (uid 1) bypasses every check;
//! - a model with no ACL rules at all is unrestricted;
//! - once a model has any rule, an operation is allowed only if one of
//!   the user's groups grants that operation.

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
            "search" | "search_read" | "search_count" | "read" => Operation::Read,
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
    /// models that have at least one rule (so absence means unrestricted)
    restricted: HashSet<String>,
    /// (model, op) -> groups granting it
    grants: HashMap<(String, Operation), HashSet<i64>>,
}

impl AccessControl {
    pub fn new() -> Self {
        Self::default()
    }

    /// Grant `group_id` the given operations on `model`. Marks the model
    /// as restricted, so every operation on it now needs an explicit
    /// grant.
    pub fn grant(&mut self, model: &str, group_id: i64, operations: &[Operation]) {
        self.restricted.insert(model.to_string());
        for op in operations {
            self.grants
                .entry((model.to_string(), *op))
                .or_default()
                .insert(group_id);
        }
    }

    /// Check whether a user in `groups` may perform `op` on `model`.
    pub fn check(
        &self,
        model: &str,
        op: Operation,
        groups: &[i64],
        is_superuser: bool,
    ) -> Result<(), RusdooError> {
        if is_superuser || !self.restricted.contains(model) {
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
