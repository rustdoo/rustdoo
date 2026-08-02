//! Record rules, port of `ir.rule` (`odoo/addons/base/models/ir_rule.py`):
//! row-level access on top of the model-level `ir.model.access`.
//!
//! A rule is a domain that every record must satisfy for one operation.
//! Odoo combines them exactly this way, and so does this:
//! - the superuser bypasses everything;
//! - **global** rules (no groups) are AND-ed: each one restricts further;
//! - **group** rules are OR-ed among the ones the user's groups match —
//!   belonging to one more group can only widen what you see;
//! - a rule whose groups the user is not in does not apply at all;
//! - the two sets are AND-ed together.
//!
//! Rule domains are stored as raw JSON and parsed per user, because they
//! speak about the acting user: the value `"user.id"` stands for the
//! uid, which is how Odoo's `domain_force` expresses "my records".

use crate::access::Operation;
use crate::domain::{parse_domain, Domain};
use rusdoo_core::RusdooError;
use serde_json::Value;

/// The placeholder a rule domain uses for the acting user, mirroring the
/// `user.id` that Odoo's `domain_force` evaluates in Python.
pub const USER_ID_PLACEHOLDER: &str = "user.id";

#[derive(Debug, Clone)]
pub struct Rule {
    pub model: String,
    /// unparsed domain: it depends on who is asking
    pub domain: Value,
    /// groups the rule applies to; empty means a global rule
    pub groups: Vec<i64>,
    pub operations: Vec<Operation>,
}

/// In-memory `ir.rule` table.
#[derive(Debug, Default, Clone)]
pub struct RecordRules {
    rules: Vec<Rule>,
}

impl RecordRules {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Whether any rule at all constrains `model`. Callers use this to
    /// skip resolving an identity they would not need.
    pub fn covers(&self, model: &str) -> bool {
        self.rules.iter().any(|rule| rule.model == model)
    }

    pub fn add(&mut self, rule: Rule) {
        self.rules.push(rule);
    }

    /// The domain every record must satisfy for `uid` to `op` on `model`.
    /// `None` means unrestricted — the superuser, or a model no rule
    /// speaks about.
    pub fn domain_for(
        &self,
        model: &str,
        op: Operation,
        uid: i64,
        groups: &[i64],
        is_superuser: bool,
    ) -> Result<Option<Domain>, RusdooError> {
        if is_superuser {
            return Ok(None);
        }
        let mut globals = Vec::new();
        let mut group_domains = Vec::new();
        for rule in &self.rules {
            if rule.model != model || !rule.operations.contains(&op) {
                continue;
            }
            let domain = parse_domain(&substitute_user(&rule.domain, uid))?;
            if rule.groups.is_empty() {
                globals.push(domain);
            } else if rule.groups.iter().any(|g| groups.contains(g)) {
                group_domains.push(domain);
            }
            // a rule whose groups the user is not in simply does not apply
        }
        if !group_domains.is_empty() {
            globals.push(Domain::Or(group_domains));
        }
        Ok(match globals.len() {
            0 => None,
            1 => Some(globals.pop().expect("just checked")),
            _ => Some(Domain::And(globals)),
        })
    }
}

/// Replace the `"user.id"` placeholder with the acting uid — in the
/// *value* slot of a condition only. `["user.id", "=", 3]` is a dotted
/// path on a field named `user`, a legitimate domain that substitution
/// must not rewrite into nonsense.
fn substitute_user(node: &Value, uid: i64) -> Value {
    let Value::Array(items) = node else {
        return node.clone();
    };
    if is_condition(items) {
        return Value::Array(vec![
            items[0].clone(),
            items[1].clone(),
            substitute_value(&items[2], uid),
        ]);
    }
    Value::Array(
        items
            .iter()
            .map(|item| substitute_user(item, uid))
            .collect(),
    )
}

/// A `[field, operator, value]` triple, as opposed to a list of them or
/// a prefix operator.
fn is_condition(items: &[Value]) -> bool {
    items.len() == 3 && items[0].is_string() && items[1].is_string()
}

fn substitute_value(value: &Value, uid: i64) -> Value {
    match value {
        Value::String(s) if s == USER_ID_PLACEHOLDER => Value::from(uid),
        // `any`/`not any` carry a whole subdomain as their value, which is
        // a list containing conditions; a plain `in` list is scalars
        Value::Array(items) if items.iter().any(Value::is_array) => substitute_user(value, uid),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| substitute_value(item, uid))
                .collect(),
        ),
        other => other.clone(),
    }
}
