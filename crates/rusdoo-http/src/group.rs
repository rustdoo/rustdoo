//! The web client's grouped read: `formatted_read_group` and
//! `web_read_group` (`odoo/addons/web/models/models.py`). The ORM returns
//! raw group rows; this layer gives them the shape the list, kanban and
//! pivot views expect — a many2one as `[id, display_name]`, and the
//! `__extra_domain` that reopens the group as a plain search.

use crate::dispatch::{OrmService, RpcError};
use rusdoo_orm::domain::Domain;
use rusdoo_orm::fields::FieldType;
use rusdoo_orm::group::{Aggregate, GroupBy, GroupOptions, COUNT_SPEC};
use rusdoo_orm::model::Model;
use serde_json::{json, Map, Value};
use std::collections::HashMap;

/// A parsed grouped-read request: what to group by, what to aggregate,
/// and how to page the groups.
pub(crate) struct GroupRequest {
    pub(crate) groupby: Vec<GroupBy>,
    pub(crate) aggregates: Vec<Aggregate>,
    pub(crate) options: GroupOptions,
}

/// Read a list-of-strings argument (`groupby`, `aggregates`).
pub(crate) fn parse_specs(raw: Option<&Value>, what: &str) -> Result<Vec<String>, RpcError> {
    match raw {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| RpcError::invalid_params(format!("{what} must be strings")))
            })
            .collect(),
        Some(_) => Err(RpcError::invalid_params(format!(
            "{what} must be a list of strings"
        ))),
    }
}

impl OrmService {
    /// Parse and validate the specs of a grouped read against the model.
    /// Exposure is checked on every field mentioned: an aggregate over a
    /// private column (`min(password)`) would disclose it just as surely
    /// as reading it.
    pub(crate) fn parse_group_request(
        &self,
        model: &str,
        groupby: &[String],
        aggregates: &[String],
        options: GroupOptions,
    ) -> Result<GroupRequest, RpcError> {
        let m = self
            .registry
            .get(model)
            .ok_or_else(|| RpcError::invalid_params(format!("unknown model: {model}")))?;
        let groupby: Vec<GroupBy> = groupby
            .iter()
            .map(|spec| GroupBy::parse(m, spec).map_err(RpcError::from))
            .collect::<Result<_, _>>()?;
        let aggregates: Vec<Aggregate> = aggregates
            .iter()
            .map(|spec| Aggregate::parse(m, spec).map_err(RpcError::from))
            .collect::<Result<_, _>>()?;
        let mentioned: Vec<String> = groupby
            .iter()
            .map(|g| g.field.clone())
            .chain(aggregates.iter().filter_map(|a| match a {
                Aggregate::Count => None,
                Aggregate::Field { field, .. } => Some(field.clone()),
            }))
            .collect();
        self.ensure_exposed(model, &mentioned)?;
        Ok(GroupRequest {
            groupby,
            aggregates,
            options,
        })
    }

    /// `formatted_read_group`: the groups of `request`, each carrying its
    /// groupby values, its aggregates and the `__extra_domain` that
    /// selects exactly its records.
    pub(crate) async fn formatted_groups(
        &self,
        model: &str,
        domain: &Domain,
        request: &GroupRequest,
    ) -> Result<Vec<Value>, RpcError> {
        let rows = self
            .registry
            .read_group(
                &self.pool,
                model,
                domain,
                &request.groupby,
                &request.aggregates,
                &request.options,
            )
            .await?;
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let m = self
            .registry
            .get(model)
            .ok_or_else(|| RpcError::invalid_params(format!("unknown model: {model}")))?;
        // one name lookup per relational groupby, never one per group
        let mut names: HashMap<&str, HashMap<i64, String>> = HashMap::new();
        for group in &request.groupby {
            let Some(comodel) = comodel_of(m, &group.field) else {
                continue;
            };
            let ids: Vec<i64> = rows
                .iter()
                .filter_map(|row| row.get(&group.spec).and_then(Value::as_i64))
                .collect();
            if ids.is_empty() {
                continue;
            }
            names.insert(
                group.spec.as_str(),
                self.registry
                    .display_names(&self.pool, comodel, &ids)
                    .await?,
            );
        }
        Ok(rows
            .into_iter()
            .map(|row| format_group(m, request, &names, row))
            .collect())
    }

    /// The total number of groups behind a page of them, mirroring
    /// `_formatted_read_group_with_length`: only a full page can hide
    /// more groups, and only then is a second query worth it.
    pub(crate) async fn count_groups(
        &self,
        model: &str,
        domain: &Domain,
        request: &GroupRequest,
        page: usize,
    ) -> Result<u64, RpcError> {
        let offset = request.options.offset.unwrap_or(0);
        let Some(limit) = request.options.limit else {
            return Ok(page as u64 + offset);
        };
        if page as u64 != limit {
            return Ok(page as u64 + offset);
        }
        let rest = self
            .registry
            .read_group(
                &self.pool,
                model,
                domain,
                &request.groupby,
                &[],
                &GroupOptions {
                    offset: Some(offset + limit),
                    ..GroupOptions::default()
                },
            )
            .await?;
        Ok(offset + limit + rest.len() as u64)
    }
}

/// The comodel of a relational groupby field, if it is one.
fn comodel_of<'a>(model: &'a Model, field: &str) -> Option<&'a str> {
    match model.field(field).map(|f| &f.ty) {
        Some(FieldType::Many2one { comodel }) => Some(comodel),
        _ => None,
    }
}

/// Shape one raw group row: relational values become `[id, display_name]`
/// (`false` when empty, like every other empty many2one on the wire), and
/// the group's own domain is assembled from its values.
fn format_group(
    model: &Model,
    request: &GroupRequest,
    names: &HashMap<&str, HashMap<i64, String>>,
    row: Map<String, Value>,
) -> Value {
    let mut group = Map::new();
    let mut extra: Vec<Value> = Vec::new();
    for spec in &request.groupby {
        let raw = row.get(&spec.spec).cloned().unwrap_or(Value::Null);
        let is_relational = comodel_of(model, &spec.field).is_some();
        let value = match (&raw, is_relational) {
            (Value::Null, _) => Value::Bool(false),
            (_, true) => {
                let id = raw.as_i64().unwrap_or_default();
                let name = names
                    .get(spec.spec.as_str())
                    .and_then(|by_id| by_id.get(&id))
                    .cloned()
                    .unwrap_or_else(|| id.to_string());
                json!([id, name])
            }
            _ => raw.clone(),
        };
        extra.extend(group_domain(spec, &raw));
        group.insert(spec.spec.clone(), value);
    }
    group.insert("__extra_domain".into(), Value::Array(extra));
    for aggregate in &request.aggregates {
        let spec = match aggregate {
            Aggregate::Count => COUNT_SPEC.to_string(),
            Aggregate::Field { spec, .. } => spec.clone(),
        };
        let value = row.get(&spec).cloned().unwrap_or(Value::Null);
        group.insert(spec, value);
    }
    Value::Object(group)
}

/// The domain terms selecting the records of one group: an equality on
/// the value, or the half-open interval a date bucket covers.
fn group_domain(spec: &GroupBy, raw: &Value) -> Vec<Value> {
    let field = spec.field.as_str();
    let Some(granularity) = spec.granularity else {
        let value = if raw.is_null() {
            Value::Bool(false)
        } else {
            raw.clone()
        };
        return vec![json!([field, "=", value])];
    };
    let Some(start) = raw.as_str() else {
        // an empty bucket is the records with no date at all
        return vec![json!([field, "=", false])];
    };
    match granularity.bucket_end(start) {
        Some(end) => vec![json!([field, ">=", start]), json!([field, "<", end])],
        // an unparseable bucket would silently widen the group's domain
        None => vec![json!([field, "=", start])],
    }
}
