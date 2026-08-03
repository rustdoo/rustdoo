//! Mirrors `odoo.models.BaseModel` metadata and its field registry.

use crate::fields::Field;

#[derive(Debug, Clone)]
pub struct ModelMeta {
    /// Odoo model name, e.g. `res.partner`
    pub name: String,
    /// PostgreSQL table, e.g. `res_partner`
    pub table: String,
    /// `_inherit` chain
    pub inherit: Vec<String>,
    /// `_inherits` delegation: (parent model, many2one link field)
    pub inherits: Vec<(String, String)>,
}

/// A rule a record must satisfy, port of `@api.constrains`.
///
/// It watches fields and answers with what is wrong, not with a bool: a
/// record refused without a reason is a screen the user cannot fix.
#[derive(Clone)]
pub struct Constraint {
    /// what it is called, for the log and for a message with no text
    pub name: String,
    /// the fields whose change makes it worth checking again
    pub fields: Vec<String>,
    /// `Ok` when the record is fine, `Err(reason)` when it is not
    pub check: fn(&serde_json::Map<String, serde_json::Value>) -> Result<(), String>,
}

impl std::fmt::Debug for Constraint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Constraint")
            .field("name", &self.name)
            .field("fields", &self.fields)
            .finish_non_exhaustive()
    }
}

/// A rule the database enforces, port of Odoo's `_sql_constraints`.
///
/// The difference from a `Constraint` is not style: a Rust check runs
/// before the `INSERT` and two concurrent requests can both pass it and
/// both write. A `UNIQUE` in PostgreSQL cannot be raced past.
#[derive(Debug, Clone)]
pub struct SqlConstraint {
    /// the constraint's name in the database
    pub name: String,
    /// what follows `ADD CONSTRAINT <name>`, e.g. `UNIQUE ("key")`
    pub definition: String,
    /// what the user is told when the database refuses the write
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct Model {
    pub meta: ModelMeta,
    fields: Vec<Field>,
    constraints: Vec<Constraint>,
    /// a `TransientModel`: its records are a dialog somebody had open,
    /// not data the business keeps
    transient: bool,
    /// `_order`: how a search with no order of its own comes back
    order: Option<String>,
    /// `_sql_constraints`: the rules the database itself enforces
    sql_constraints: Vec<SqlConstraint>,
}

impl Model {
    pub fn new(meta: ModelMeta, fields: Vec<Field>) -> Self {
        Model {
            meta,
            fields,
            constraints: Vec::new(),
            transient: false,
            order: None,
            sql_constraints: Vec::new(),
        }
    }

    /// Attach a rule the database enforces, port of `_sql_constraints`.
    ///
    /// `definition` is the SQL that follows `ADD CONSTRAINT <name>`, and
    /// `message` is what the user reads when the write is refused — a
    /// raw `duplicate key value violates unique constraint
    /// "ir_config_parameter_key_uniq"` is not a sentence anyone can act
    /// on.
    pub fn sql_constrained(mut self, name: &str, definition: &str, message: &str) -> Self {
        self.sql_constraints.push(SqlConstraint {
            name: name.to_string(),
            definition: definition.to_string(),
            message: message.to_string(),
        });
        self
    }

    pub fn sql_constraints(&self) -> &[SqlConstraint] {
        &self.sql_constraints
    }

    /// Mark the model transient (Odoo's `TransientModel`): the rows are
    /// the state of a dialog, and old ones are swept away instead of
    /// being kept forever.
    pub fn transient(mut self) -> Self {
        self.transient = true;
        self
    }

    pub fn is_transient(&self) -> bool {
        self.transient
    }

    /// Attach a rule every record of this model must satisfy.
    pub fn constrained(
        mut self,
        name: &str,
        fields: &[&str],
        check: fn(&serde_json::Map<String, serde_json::Value>) -> Result<(), String>,
    ) -> Self {
        self.constraints.push(Constraint {
            name: name.to_string(),
            fields: fields.iter().map(|f| (*f).to_string()).collect(),
            check,
        });
        self
    }

    pub fn constraints(&self) -> &[Constraint] {
        &self.constraints
    }

    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }

    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    /// Port of `_order`: how a search that names no order of its own
    /// comes back. `"date desc, id desc"` reads like Odoo's.
    ///
    /// Without it a `SELECT` with no `ORDER BY` may answer in any order
    /// PostgreSQL likes — which usually looks like insertion order right
    /// up to the first `UPDATE`, and then quietly stops.
    pub fn ordered(mut self, order: &str) -> Self {
        self.order = Some(order.to_string());
        self
    }

    /// What this model declared, if anything — what an `_inherit` child
    /// falls back to when it declares no order of its own.
    pub(crate) fn declared_order(&self) -> Option<&str> {
        self.order.as_deref()
    }

    /// The order to search by: the declared one, or `id` like Odoo.
    pub fn order(&self) -> &str {
        self.order.as_deref().unwrap_or("id")
    }

    /// The same model with its identity, fields and rules replaced —
    /// what the registry does after folding an `_inherit` chain.
    ///
    /// It rebuilds *from* the model instead of constructing a new one so
    /// that everything the caller declared and this function says nothing
    /// about is carried over untouched. The previous shape took the model
    /// apart and put a new one together, which meant every attribute
    /// added later had to be threaded through by hand — and the first one
    /// that was not, the constraints, was silently dropped for a while.
    pub(crate) fn rebuilt(
        mut self,
        meta: ModelMeta,
        fields: Vec<Field>,
        constraints: Vec<Constraint>,
    ) -> Self {
        self.meta = meta;
        self.fields = fields;
        self.constraints = constraints;
        self
    }
}
