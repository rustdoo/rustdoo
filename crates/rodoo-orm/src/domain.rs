//! Port of Odoo domain expressions (`odoo/orm/domains.py`).
//!
//! Domains are lists in second-order prefix notation: `'&'`/`'|'` are binary,
//! `'!'` is unary, and consecutive expressions without an operator are
//! implicitly AND-ed. `(1, '=', 1)` and `(0, '=', 1)` are the TRUE/FALSE leaves.

use rodoo_core::RodooError;
use serde_json::Value;

/// Hard cap on prefix-operator nesting; parsing is recursive, so an
/// unbounded domain from an untrusted caller could overflow the stack.
const MAX_DOMAIN_DEPTH: usize = 100;

#[derive(Debug, Clone, PartialEq)]
pub enum Operator {
    Eq,
    Neq,
    /// `=?`: equals, or TRUE when the value is unset
    EqMaybe,
    Lt,
    Lte,
    Gt,
    Gte,
    In,
    NotIn,
    Like,
    NotLike,
    ILike,
    NotILike,
    /// `=like`: pattern taken verbatim (no wildcard wrapping)
    EqLike,
    NotEqLike,
    EqILike,
    NotEqILike,
    ChildOf,
    ParentOf,
    Any,
    NotAny,
}

impl Operator {
    pub fn parse(s: &str) -> Option<Self> {
        use Operator::*;
        Some(match s {
            // '==' and '<>' are legacy aliases still normalized by Odoo
            "=" | "==" => Eq,
            "!=" | "<>" => Neq,
            "=?" => EqMaybe,
            "<" => Lt,
            "<=" => Lte,
            ">" => Gt,
            ">=" => Gte,
            "in" => In,
            "not in" => NotIn,
            "like" => Like,
            "not like" => NotLike,
            "ilike" => ILike,
            "not ilike" => NotILike,
            "=like" => EqLike,
            "not =like" => NotEqLike,
            "=ilike" => EqILike,
            "not =ilike" => NotEqILike,
            "child_of" => ChildOf,
            "parent_of" => ParentOf,
            "any" => Any,
            "not any" => NotAny,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Term {
    pub field: String,
    pub op: Operator,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Domain {
    True,
    False,
    And(Vec<Domain>),
    Or(Vec<Domain>),
    Not(Box<Domain>),
    Term(Term),
}

pub fn parse_domain(raw: &Value) -> Result<Domain, RodooError> {
    let items = raw
        .as_array()
        .ok_or_else(|| RodooError::Validation("domain must be a list".into()))?;
    let mut parts = Vec::new();
    let mut pos = 0;
    while pos < items.len() {
        let (dom, next) = parse_expr(items, pos, 0)?;
        parts.push(dom);
        pos = next;
    }
    Ok(match parts.len() {
        0 => Domain::True,
        1 => parts.remove(0),
        _ => Domain::And(parts),
    })
}

fn parse_expr(items: &[Value], pos: usize, depth: usize) -> Result<(Domain, usize), RodooError> {
    if depth > MAX_DOMAIN_DEPTH {
        return Err(RodooError::Validation(format!(
            "domain nesting exceeds {MAX_DOMAIN_DEPTH} levels"
        )));
    }
    match items.get(pos) {
        None => Err(RodooError::Validation(
            "domain ended while an operator still expected operands".into(),
        )),
        Some(Value::String(s)) => match s.as_str() {
            "&" => {
                let (a, p) = parse_expr(items, pos + 1, depth + 1)?;
                let (b, p) = parse_expr(items, p, depth + 1)?;
                Ok((Domain::And(vec![a, b]), p))
            }
            "|" => {
                let (a, p) = parse_expr(items, pos + 1, depth + 1)?;
                let (b, p) = parse_expr(items, p, depth + 1)?;
                Ok((Domain::Or(vec![a, b]), p))
            }
            "!" => {
                let (a, p) = parse_expr(items, pos + 1, depth + 1)?;
                Ok((Domain::Not(Box::new(a)), p))
            }
            other => Err(RodooError::Validation(format!(
                "invalid domain operator: {other:?}"
            ))),
        },
        Some(Value::Array(term)) => Ok((parse_term(term)?, pos + 1)),
        Some(other) => Err(RodooError::Validation(format!(
            "invalid domain element: {other}"
        ))),
    }
}

fn parse_term(term: &[Value]) -> Result<Domain, RodooError> {
    let [field, op, value] = term else {
        return Err(RodooError::Validation(format!(
            "domain term must have 3 elements, got {}",
            term.len()
        )));
    };
    // Odoo's _TRUE_LEAF (1,'=',1) / _FALSE_LEAF (0,'=',1)
    if op == "=" && value.as_i64() == Some(1) {
        match field.as_i64() {
            Some(1) => return Ok(Domain::True),
            Some(0) => return Ok(Domain::False),
            _ => {}
        }
    }
    let field = field
        .as_str()
        .ok_or_else(|| RodooError::Validation(format!("field name must be a string: {field}")))?;
    let op_str = op
        .as_str()
        .ok_or_else(|| RodooError::Validation(format!("operator must be a string: {op}")))?;
    let op = Operator::parse(op_str)
        .ok_or_else(|| RodooError::Validation(format!("unknown operator: {op_str:?}")))?;
    Ok(Domain::Term(Term {
        field: field.to_string(),
        op,
        value: value.clone(),
    }))
}
