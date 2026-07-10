//! Domain -> SQL WHERE translation with `$n` placeholders.
//!
//! Mirrors the NULL semantics of Odoo's condition operators: a False/None
//! value means *not set*, and negative operators also match unset records.
//! When a [`Model`] is provided, "not set" additionally matches the field's
//! falsy value (0, '', false), like `falsy_value` on Odoo field classes.

use crate::domain::{Domain, Operator, Term};
use crate::model::Model;
use rodoo_core::RodooError;
use serde_json::Value;

/// Hard cap mirroring the parser's limit: `Domain` values can also be built
/// programmatically, so rendering must bound its own recursion.
const MAX_RENDER_DEPTH: usize = 100;

/// Validate and double-quote a SQL identifier. Dotted paths (joins) are
/// intentionally rejected until relational traversal is implemented.
pub fn quote_ident(name: &str) -> Result<String, RodooError> {
    let mut chars = name.chars();
    let valid = matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if valid {
        Ok(format!("\"{name}\""))
    } else {
        Err(RodooError::Validation(format!(
            "invalid SQL identifier: {name:?}"
        )))
    }
}

/// Type-blind rendering; prefer [`render`] with a model when one is available.
pub fn where_clause(domain: &Domain) -> Result<(String, Vec<Value>), RodooError> {
    let mut params = Vec::new();
    let sql = render(domain, &mut params, None)?;
    Ok((sql, params))
}

/// Render a domain, appending its bind values to `params` (placeholders
/// continue from the current length, so callers can prepend their own).
pub fn render(
    domain: &Domain,
    params: &mut Vec<Value>,
    model: Option<&Model>,
) -> Result<String, RodooError> {
    render_at(domain, params, model, 0)
}

pub(crate) fn bind(params: &mut Vec<Value>, value: Value) -> String {
    params.push(value);
    format!("${}", params.len())
}

fn render_at(
    domain: &Domain,
    params: &mut Vec<Value>,
    model: Option<&Model>,
    depth: usize,
) -> Result<String, RodooError> {
    if depth > MAX_RENDER_DEPTH {
        return Err(RodooError::Validation(format!(
            "domain nesting exceeds {MAX_RENDER_DEPTH} levels"
        )));
    }
    match domain {
        Domain::True => Ok("TRUE".into()),
        Domain::False => Ok("FALSE".into()),
        Domain::And(children) => render_nary("AND", children, params, model, depth),
        Domain::Or(children) => render_nary("OR", children, params, model, depth),
        Domain::Not(child) => Ok(format!(
            "NOT ({})",
            render_at(child, params, model, depth + 1)?
        )),
        Domain::Term(term) => render_term(term, params, model),
    }
}

fn render_nary(
    op: &str,
    children: &[Domain],
    params: &mut Vec<Value>,
    model: Option<&Model>,
    depth: usize,
) -> Result<String, RodooError> {
    match children {
        [] => Ok("TRUE".into()),
        [only] => render_at(only, params, model, depth + 1),
        _ => {
            let parts: Vec<String> = children
                .iter()
                .map(|c| render_at(c, params, model, depth + 1))
                .collect::<Result<_, _>>()?;
            Ok(format!("({})", parts.join(&format!(" {op} "))))
        }
    }
}

/// Odoo semantics: False/None as a condition value means "not set".
fn is_unset(value: &Value) -> bool {
    value.is_null() || *value == Value::Bool(false)
}

/// The field's stored falsy value, when the model is known and the type
/// has one (0 for numeric, '' for text, false for boolean).
fn falsy_value(field: &str, model: Option<&Model>) -> Option<Value> {
    model?.field(field)?.falsy_value()
}

fn render_term(
    term: &Term,
    params: &mut Vec<Value>,
    model: Option<&Model>,
) -> Result<String, RodooError> {
    let col = quote_ident(&term.field)?;
    let value = &term.value;
    use Operator::*;

    // Odoo rewrites '='/'!=' with a collection value into 'in'/'not in'
    if value.is_array() && matches!(term.op, Eq | Neq) {
        return render_in(&col, value, term.op == Neq, params);
    }

    match &term.op {
        Eq => Ok(if is_unset(value) {
            match falsy_value(&term.field, model) {
                Some(fv) => format!("({col} = {p} OR {col} IS NULL)", p = bind(params, fv)),
                None => format!("{col} IS NULL"),
            }
        } else {
            format!("{col} = {}", bind(params, value.clone()))
        }),
        Neq => Ok(if is_unset(value) {
            match falsy_value(&term.field, model) {
                Some(fv) => format!("({col} != {p} AND {col} IS NOT NULL)", p = bind(params, fv)),
                None => format!("{col} IS NOT NULL"),
            }
        } else {
            // negative operators also match unset records
            format!(
                "({col} != {p} OR {col} IS NULL)",
                p = bind(params, value.clone())
            )
        }),
        EqMaybe => Ok(if is_unset(value) {
            "TRUE".into()
        } else {
            format!("{col} = {}", bind(params, value.clone()))
        }),
        Lt => render_cmp(&col, "<", value, params),
        Lte => render_cmp(&col, "<=", value, params),
        Gt => render_cmp(&col, ">", value, params),
        Gte => render_cmp(&col, ">=", value, params),
        In | NotIn => render_in(&col, value, term.op == NotIn, params),
        Like => render_like(&col, "LIKE", value, true, false, params),
        ILike => render_like(&col, "ILIKE", value, true, false, params),
        NotLike => render_like(&col, "LIKE", value, true, true, params),
        NotILike => render_like(&col, "ILIKE", value, true, true, params),
        EqLike => render_like(&col, "LIKE", value, false, false, params),
        EqILike => render_like(&col, "ILIKE", value, false, false, params),
        NotEqLike => render_like(&col, "LIKE", value, false, true, params),
        NotEqILike => render_like(&col, "ILIKE", value, false, true, params),
        ChildOf | ParentOf | Any | NotAny => Err(RodooError::Validation(format!(
            "operator not yet supported: {:?}",
            term.op
        ))),
    }
}

fn render_cmp(
    col: &str,
    sql_op: &str,
    value: &Value,
    params: &mut Vec<Value>,
) -> Result<String, RodooError> {
    Ok(format!("{col} {sql_op} {}", bind(params, value.clone())))
}

fn render_in(
    col: &str,
    value: &Value,
    negative: bool,
    params: &mut Vec<Value>,
) -> Result<String, RodooError> {
    let items = value.as_array().ok_or_else(|| {
        RodooError::Validation(format!("'in' operator expects a list, got {value}"))
    })?;
    let has_unset = items.iter().any(is_unset);
    let set_values: Vec<&Value> = items.iter().filter(|v| !is_unset(v)).collect();

    if set_values.is_empty() {
        return Ok(match (negative, has_unset) {
            (false, true) => format!("{col} IS NULL"),
            (false, false) => "FALSE".into(),
            (true, true) => format!("{col} IS NOT NULL"),
            (true, false) => "TRUE".into(),
        });
    }
    let placeholders: Vec<String> = set_values
        .into_iter()
        .map(|v| bind(params, v.clone()))
        .collect();
    let list = placeholders.join(", ");
    Ok(match (negative, has_unset) {
        (false, false) => format!("{col} IN ({list})"),
        (false, true) => format!("({col} IN ({list}) OR {col} IS NULL)"),
        // "not set" excluded explicitly: value must exist and differ
        (true, true) => format!("({col} NOT IN ({list}) AND {col} IS NOT NULL)"),
        (true, false) => format!("({col} NOT IN ({list}) OR {col} IS NULL)"),
    })
}

fn render_like(
    col: &str,
    sql_op: &str,
    value: &Value,
    wrap: bool,
    negative: bool,
    params: &mut Vec<Value>,
) -> Result<String, RodooError> {
    let text = value.as_str().ok_or_else(|| {
        RodooError::Validation(format!("pattern operator expects a string, got {value}"))
    })?;
    let pattern = if wrap {
        // the user searched a literal string: escape LIKE wildcards, then wrap
        let escaped = text
            .replace('\\', r"\\")
            .replace('%', r"\%")
            .replace('_', r"\_");
        format!("%{escaped}%")
    } else {
        text.to_string()
    };
    let p = bind(params, Value::String(pattern));
    Ok(if negative {
        format!("({col} NOT {sql_op} {p} OR {col} IS NULL)")
    } else {
        format!("{col} {sql_op} {p}")
    })
}
