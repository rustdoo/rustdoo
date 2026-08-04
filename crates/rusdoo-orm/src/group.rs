//! Grouped reads, port of `_read_group` (`odoo/orm/models.py`): GROUP BY
//! over a search domain with aggregates. It is what the list, kanban and
//! pivot views run on.
//!
//! Every part of a grouped query is client-supplied — field names,
//! granularities, aggregate functions, ordering — so nothing here is
//! interpolated raw: names resolve through the model's fields and both
//! granularities and aggregate functions come from closed enums.

use crate::domain::Domain;
use crate::fields::FieldType;
use crate::model::Model;
use crate::registry::Registry;
use crate::sql::{quote_ident, render, Ctx};
use rusdoo_core::RusdooError;
use serde_json::Value;

/// Date buckets Odoo accepts in a `field:granularity` groupby.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Granularity {
    Day,
    Week,
    Month,
    Quarter,
    Year,
}

impl Granularity {
    fn parse(name: &str) -> Option<Granularity> {
        Some(match name {
            "day" => Granularity::Day,
            "week" => Granularity::Week,
            "month" => Granularity::Month,
            "quarter" => Granularity::Quarter,
            "year" => Granularity::Year,
            _ => return None,
        })
    }

    /// The `date_trunc` unit. ISO weeks start on Monday in PostgreSQL,
    /// which is what Odoo groups by too.
    fn unit(self) -> &'static str {
        match self {
            Granularity::Day => "day",
            Granularity::Week => "week",
            Granularity::Month => "month",
            Granularity::Quarter => "quarter",
            Granularity::Year => "year",
        }
    }
}

/// The wire formats dates take, shared by the read path and the group
/// buckets (`odoo/tools/misc.py` DEFAULT_SERVER_*_FORMAT).
const DATE_FORMAT: &str = "%Y-%m-%d";
const DATETIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

/// One `groupby` entry: a stored field, bucketed by a granularity when it
/// holds a date (`create_date:month`).
#[derive(Debug, Clone)]
pub struct GroupBy {
    /// the spec as the client wrote it — the key of the result column
    pub spec: String,
    pub field: String,
    pub granularity: Option<Granularity>,
}

/// Aggregate functions accepted in a `field:function` spec. A closed set:
/// PostgreSQL has many more, but each one reaching SQL must be one we
/// know is a pure aggregate over a single column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFunc {
    Count,
    CountDistinct,
    Sum,
    Avg,
    Min,
    Max,
    BoolAnd,
    BoolOr,
    ArrayAgg,
}

impl AggFunc {
    fn parse(name: &str) -> Option<AggFunc> {
        Some(match name {
            "count" => AggFunc::Count,
            "count_distinct" => AggFunc::CountDistinct,
            "sum" => AggFunc::Sum,
            "avg" => AggFunc::Avg,
            "min" => AggFunc::Min,
            "max" => AggFunc::Max,
            "bool_and" => AggFunc::BoolAnd,
            "bool_or" => AggFunc::BoolOr,
            "array_agg" => AggFunc::ArrayAgg,
            _ => return None,
        })
    }

    fn render(self, column: &str) -> String {
        match self {
            AggFunc::Count => format!("count({column})"),
            AggFunc::CountDistinct => format!("count(DISTINCT {column})"),
            AggFunc::Sum => format!("sum({column})"),
            AggFunc::Avg => format!("avg({column})"),
            AggFunc::Min => format!("min({column})"),
            AggFunc::Max => format!("max({column})"),
            AggFunc::BoolAnd => format!("bool_and({column})"),
            AggFunc::BoolOr => format!("bool_or({column})"),
            AggFunc::ArrayAgg => format!("array_agg({column})"),
        }
    }
}

impl Granularity {
    /// The exclusive upper bound of the bucket `start` opens, in the same
    /// wire format. It is what turns a bucket into a domain
    /// (`>= start AND < end`), and months and quarters make that calendar
    /// arithmetic, not a fixed number of days.
    pub fn bucket_end(self, start: &str) -> Option<String> {
        let (datetime, has_time) =
            match chrono::NaiveDateTime::parse_from_str(start, DATETIME_FORMAT) {
                Ok(moment) => (moment, true),
                Err(_) => (
                    chrono::NaiveDate::parse_from_str(start, DATE_FORMAT)
                        .ok()?
                        .and_hms_opt(0, 0, 0)?,
                    false,
                ),
            };
        let end = match self {
            Granularity::Day => datetime.checked_add_days(chrono::Days::new(1))?,
            Granularity::Week => datetime.checked_add_days(chrono::Days::new(7))?,
            Granularity::Month => datetime.checked_add_months(chrono::Months::new(1))?,
            Granularity::Quarter => datetime.checked_add_months(chrono::Months::new(3))?,
            Granularity::Year => datetime.checked_add_months(chrono::Months::new(12))?,
        };
        Some(if has_time {
            end.format(DATETIME_FORMAT).to_string()
        } else {
            end.date().format(DATE_FORMAT).to_string()
        })
    }
}

/// One `aggregates` entry: the group's record count, or a function over
/// one of its fields.
#[derive(Debug, Clone)]
pub enum Aggregate {
    /// `__count`, the number of records in the group
    Count,
    Field {
        /// the spec as the client wrote it — the key of the result column
        spec: String,
        field: String,
        function: AggFunc,
    },
}

impl Aggregate {
    fn spec(&self) -> &str {
        match self {
            Aggregate::Count => COUNT_SPEC,
            Aggregate::Field { spec, .. } => spec,
        }
    }
}

/// Odoo's name for the group record count.
pub const COUNT_SPEC: &str = "__count";

#[derive(Debug, Clone, Default)]
pub struct GroupOptions {
    /// `"spec asc, other desc"` over groupby/aggregate specs; defaults to
    /// the groupby specs ascending, like Odoo's `formatted_read_group`
    pub order: Option<String>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
}

/// Split a `name:qualifier` spec. A dotted path is refused: grouping
/// through a related field needs a join the reader does not build yet,
/// and silently grouping by the local column instead would be wrong.
fn split_spec<'a>(spec: &'a str, kind: &str) -> Result<(&'a str, Option<&'a str>), RusdooError> {
    let (name, qualifier) = match spec.split_once(':') {
        Some((name, qualifier)) => (name, Some(qualifier)),
        None => (spec, None),
    };
    if name.contains('.') {
        return Err(RusdooError::Validation(format!(
            "{kind} through a related field is not supported yet: {spec:?}"
        )));
    }
    Ok((name, qualifier))
}

/// The stored, groupable/aggregatable field behind a spec — the model's
/// own, or the one it reaches through `_inherits`.
fn stored_field<'a>(
    registry: &'a Registry,
    model: &'a Model,
    name: &str,
    spec: &str,
) -> Result<&'a crate::fields::Field, RusdooError> {
    let field = registry.field_of(model, name).ok_or_else(|| {
        RusdooError::Validation(format!(
            "unknown field on {}: {spec:?}",
            model.meta.name.as_str()
        ))
    })?;
    if !field.stored {
        return Err(RusdooError::Validation(format!(
            "field is not stored: {spec:?}"
        )));
    }
    Ok(field)
}

impl GroupBy {
    /// Parse one groupby spec against the model. A date or datetime field
    /// without an explicit granularity buckets by month, like Odoo.
    pub fn parse(registry: &Registry, model: &Model, spec: &str) -> Result<GroupBy, RusdooError> {
        let (name, qualifier) = split_spec(spec, "grouping")?;
        let field = stored_field(registry, model, name, spec)?;
        let is_date = matches!(field.ty, FieldType::Date | FieldType::Datetime);
        let granularity = match (qualifier, is_date) {
            (Some(q), true) => Some(Granularity::parse(q).ok_or_else(|| {
                RusdooError::Validation(format!("unknown date granularity: {q:?}"))
            })?),
            (Some(q), false) => {
                return Err(RusdooError::Validation(format!(
                    "granularity {q:?} only applies to date fields, not {spec:?}"
                )))
            }
            (None, true) => Some(Granularity::Month),
            (None, false) => None,
        };
        Ok(GroupBy {
            spec: spec.to_string(),
            field: name.to_string(),
            granularity,
        })
    }

    /// The SQL expression the rows group on. Dates collapse to their
    /// bucket, rendered in the same wire format the read path uses
    /// (`YYYY-MM-DD` / `YYYY-MM-DD HH:MM:SS`), so a grouped value and a
    /// read value of the same record are the same string.
    fn expression(&self, registry: &Registry, model: &Model) -> Result<String, RusdooError> {
        let column = match crate::sql::delegated_expr(registry, model, &self.field)? {
            Some(expr) => expr,
            None => quote_ident(&self.field)?,
        };
        let Some(granularity) = self.granularity else {
            return Ok(column);
        };
        let is_datetime = matches!(
            registry.field_of(model, &self.field).map(|f| &f.ty),
            Some(FieldType::Datetime)
        );
        let format = if is_datetime {
            "YYYY-MM-DD HH24:MI:SS"
        } else {
            "YYYY-MM-DD"
        };
        Ok(format!(
            "to_char(date_trunc('{}', {column}), '{format}')",
            granularity.unit()
        ))
    }
}

impl Aggregate {
    /// Parse one aggregate spec against the model: `__count`, or
    /// `field:function` over a stored column.
    pub fn parse(registry: &Registry, model: &Model, spec: &str) -> Result<Aggregate, RusdooError> {
        if spec == COUNT_SPEC {
            return Ok(Aggregate::Count);
        }
        let (name, qualifier) = split_spec(spec, "aggregating")?;
        let Some(qualifier) = qualifier else {
            return Err(RusdooError::Validation(format!(
                "aggregate needs a function, e.g. {name}:sum (got {spec:?})"
            )));
        };
        let function = AggFunc::parse(qualifier).ok_or_else(|| {
            RusdooError::Validation(format!("unknown aggregate function: {qualifier:?}"))
        })?;
        stored_field(registry, model, name, spec)?;
        Ok(Aggregate::Field {
            spec: spec.to_string(),
            field: name.to_string(),
            function,
        })
    }

    fn expression(&self, registry: &Registry, model: &Model) -> Result<String, RusdooError> {
        match self {
            Aggregate::Count => Ok("count(*)".to_string()),
            Aggregate::Field {
                field, function, ..
            } => {
                let column = match crate::sql::delegated_expr(registry, model, field)? {
                    Some(expr) => expr,
                    None => quote_ident(field)?,
                };
                Ok(function.render(&column))
            }
        }
    }
}

/// A grouped query: the SQL, its bound parameters, and the column each
/// selected value lands in.
pub struct GroupQuery {
    pub sql: String,
    pub params: Vec<Value>,
    pub columns: Vec<GroupColumn>,
}

/// One selected column: its SQL alias and the spec it answers to (the key
/// it takes in the decoded group).
pub struct GroupColumn {
    pub alias: String,
    pub spec: String,
}

impl Registry {
    /// Build the grouped read: one row per group, carrying the groupby
    /// values and the aggregates. Every value is selected through
    /// `to_jsonb` so a row decodes into JSON without the reader having to
    /// guess the SQL type each aggregate produces.
    pub fn read_group_sql(
        &self,
        model_name: &str,
        domain: &Domain,
        groupby: &[GroupBy],
        aggregates: &[Aggregate],
        opts: &GroupOptions,
    ) -> Result<GroupQuery, RusdooError> {
        let model = self
            .get(model_name)
            .ok_or_else(|| RusdooError::Validation(format!("unknown model: {model_name}")))?;
        if groupby.is_empty() {
            return Err(RusdooError::Validation(
                "read_group needs at least one groupby".into(),
            ));
        }
        let mut selected = Vec::new();
        let mut columns = Vec::new();
        let mut group_exprs = Vec::new();
        // spec -> expression, for resolving the ORDER BY below
        let mut sortable: Vec<(String, String)> = Vec::new();
        for (index, group) in groupby.iter().enumerate() {
            let expr = group.expression(self, model)?;
            selected.push(format!(r#"to_jsonb({expr}) AS "g{index}""#));
            sortable.push((group.spec.clone(), expr.clone()));
            group_exprs.push(expr);
            columns.push(GroupColumn {
                alias: format!("g{index}"),
                spec: group.spec.clone(),
            });
        }
        for (index, aggregate) in aggregates.iter().enumerate() {
            let expr = aggregate.expression(self, model)?;
            selected.push(format!(r#"to_jsonb({expr}) AS "a{index}""#));
            sortable.push((aggregate.spec().to_string(), expr));
            columns.push(GroupColumn {
                alias: format!("a{index}"),
                spec: aggregate.spec().to_string(),
            });
        }
        let mut params = Vec::new();
        let where_sql = render(domain, &mut params, Ctx::full(model, self))?;
        let mut sql = format!(
            "SELECT {} FROM {} WHERE {where_sql} GROUP BY {}",
            selected.join(", "),
            quote_ident(&model.meta.table)?,
            group_exprs.join(", ")
        );
        sql.push_str(&format!(
            " ORDER BY {}",
            order_by(opts.order.as_deref(), groupby, &sortable)?
        ));
        if let Some(limit) = opts.limit {
            sql.push_str(&format!(" LIMIT {limit}"));
        }
        if let Some(offset) = opts.offset {
            sql.push_str(&format!(" OFFSET {offset}"));
        }
        Ok(GroupQuery {
            sql,
            params,
            columns,
        })
    }
}

/// Resolve the ORDER BY against the specs the query actually selects —
/// an order over anything else is refused rather than ignored. Without an
/// explicit order the groups come back sorted by the groupby values, the
/// default `formatted_read_group` applies.
fn order_by(
    order: Option<&str>,
    groupby: &[GroupBy],
    sortable: &[(String, String)],
) -> Result<String, RusdooError> {
    let Some(order) = order.map(str::trim).filter(|o| !o.is_empty()) else {
        let mut exprs = Vec::new();
        for group in groupby {
            let expr = sortable
                .iter()
                .find(|(spec, _)| spec == &group.spec)
                .map(|(_, expr)| expr.clone())
                .expect("groupby specs are all sortable");
            exprs.push(format!("{expr} ASC"));
        }
        return Ok(exprs.join(", "));
    };
    let mut clauses = Vec::new();
    for part in order.split(',') {
        let mut words = part.split_whitespace();
        let spec = words
            .next()
            .ok_or_else(|| RusdooError::Validation("empty ORDER BY clause".into()))?;
        let expr = sortable
            .iter()
            .find(|(known, _)| known == spec)
            .map(|(_, expr)| expr.clone())
            .ok_or_else(|| {
                RusdooError::Validation(format!(
                    "cannot order groups by {spec:?}: it is neither a groupby nor an aggregate"
                ))
            })?;
        let direction = match words.next().map(str::to_ascii_lowercase).as_deref() {
            None | Some("asc") => "ASC",
            Some("desc") => "DESC",
            Some(other) => {
                return Err(RusdooError::Validation(format!(
                    "invalid order direction: {other:?}"
                )))
            }
        };
        if words.next().is_some() {
            return Err(RusdooError::Validation(format!(
                "malformed order clause: {part:?}"
            )));
        }
        clauses.push(format!("{expr} {direction}"));
    }
    Ok(clauses.join(", "))
}
