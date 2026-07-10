//! Port of Odoo domain expressions (`odoo/orm/domains.py`),
//! e.g. `[("name", "=", "x"), ("age", ">", 18)]`.

use serde_json::Value;

#[derive(Debug, Clone)]
pub enum Domain {
    /// `&` — conjunction (implicit between terms in Odoo)
    And(Vec<Domain>),
    /// `|`
    Or(Box<Domain>, Box<Domain>),
    /// `!`
    Not(Box<Domain>),
    /// `(field, operator, value)`
    Term { field: String, op: Operator, value: Value },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Operator {
    Eq, Neq, Gt, Gte, Lt, Lte,
    Like, ILike, NotLike, NotILike,
    In, NotIn, ChildOf, ParentOf,
}
