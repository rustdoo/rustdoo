//! Port of `odoo/orm/fields*.py`.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum FieldType {
    Boolean,
    Integer,
    Float {
        digits: Option<(u8, u8)>,
    },
    Char {
        size: Option<u32>,
    },
    Text,
    Html,
    Date,
    Datetime,
    Binary,
    Selection(Vec<(String, String)>),
    Many2one {
        comodel: String,
    },
    One2many {
        comodel: String,
        inverse: String,
    },
    Many2many {
        comodel: String,
        /// relation table joining the two models
        relation: String,
        /// column referencing this model in the relation table
        column1: String,
        /// column referencing the comodel
        column2: String,
    },
    Json,
    Monetary,
}

/// What a computed field is: the fields it reads, and the function that
/// turns them into its value.
///
/// The function is plain Rust (`odoo/orm/fields.py`'s `compute`, whose
/// bodies are Python methods). Keeping it compiled rather than
/// interpreted is deliberate: the compiler checks it, it evaluates
/// nothing that came from data, and it costs a call instead of a walk
/// over an expression tree.
#[derive(Clone)]
pub struct Compute {
    /// fields the function reads (`@api.depends`). They are read for the
    /// record before it runs, and they are what a stored compute would
    /// have to watch to know when to run again.
    pub depends: Vec<String>,
    pub func: fn(&serde_json::Map<String, Value>) -> Value,
}

impl std::fmt::Debug for Compute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // a fn pointer prints as an address, which says nothing useful
        f.debug_struct("Compute")
            .field("depends", &self.depends)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub ty: FieldType,
    pub required: bool,
    pub readonly: bool,
    pub stored: bool,
    /// whether the field may be returned through RPC read/search_read;
    /// false for secrets like password hashes
    pub exposed: bool,
    /// value the ORM writes when a create leaves the field out, and what
    /// `default_get` serves to a fresh form (`odoo/orm/fields.py`'s
    /// `default`)
    pub default: Option<Value>,
    /// dotted path this field mirrors (`odoo/orm/fields.py`'s `related`):
    /// the value lives on another record, reached by following many2one
    /// hops from this one
    pub related: Option<String>,
    /// how this field is derived from others, when it is not read from a
    /// column of its own
    pub compute: Option<Compute>,
    /// the `ir.sequence` code this field's default comes from — the port
    /// of Odoo's `default=lambda self: ...next_by_code(...)`. Resolved
    /// inside the create's own transaction, so two clients creating at
    /// once never get the same number.
    pub sequence: Option<String>,
    /// a default the ORM has to *run*, not read: Odoo's callable
    /// `default=`. See [`DefaultFn`].
    pub default_fn: Option<DefaultFn>,
    /// what the database does to this reference when the record it
    /// points at is deleted (Odoo's `ondelete=` on a many2one)
    pub ondelete: Option<OnDelete>,
}

/// What happens to a reference when its target is deleted, port of
/// Odoo's `ondelete=` on a many2one.
///
/// Without one of these, deleting a referenced record leaves the column
/// pointing at an id that no longer exists — a form that opens onto
/// nothing, and a join that silently drops rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnDelete {
    /// the reference is emptied — Odoo's default for a many2one
    SetNull,
    /// the delete is refused while anything still points here
    Restrict,
    /// the referring records go too: only for records that have no life
    /// of their own, like the lines of a document
    Cascade,
}

impl OnDelete {
    pub fn sql(self) -> &'static str {
        match self {
            OnDelete::SetNull => "SET NULL",
            OnDelete::Restrict => "RESTRICT",
            OnDelete::Cascade => "CASCADE",
        }
    }
}

/// What a dynamic default is given: the acting user, and the database.
///
/// The connection rather than the pool because the default runs inside
/// the create's own transaction — a default that reads a record the same
/// call just wrote must see it, and one that reads from another
/// connection would not.
pub struct DefaultCtx<'a> {
    pub registry: &'a crate::registry::Registry,
    pub conn: &'a mut sqlx::PgConnection,
    /// the user the record is being created as — what "my company" means
    pub uid: i64,
    pub model: &'a str,
}

/// A default the ORM runs at create time, port of Odoo's callable
/// `default=` (`fields.Datetime.now`, `lambda self: self.env.company`).
///
/// A constant cannot say "today", "the acting user's company", or "the
/// first stage of this pipeline" — and those are most of the defaults a
/// real model has.
pub type DefaultFn = for<'a> fn(
    DefaultCtx<'a>,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Value, rusdoo_core::RusdooError>> + Send + 'a>,
>;

impl Field {
    pub fn new(name: &str, ty: FieldType) -> Self {
        let stored = !matches!(ty, FieldType::One2many { .. } | FieldType::Many2many { .. });
        Field {
            name: name.to_string(),
            ty,
            required: false,
            readonly: false,
            stored,
            exposed: true,
            default: None,
            related: None,
            compute: None,
            sequence: None,
            default_fn: None,
            ondelete: None,
        }
    }

    /// Declare what the database does to this reference when its target
    /// is deleted (Odoo's `ondelete=`).
    ///
    /// It becomes a real foreign key, which is the point: a rule that
    /// lives in the application is a rule that a second writer, an
    /// import, or a psql session walks straight past.
    pub fn ondelete(mut self, behaviour: OnDelete) -> Self {
        self.ondelete = Some(behaviour);
        self
    }

    /// Declare a default the ORM runs, port of Odoo's callable
    /// `default=`. It wins over a constant default on the same field:
    /// declaring both is a mistake, and the one that had to be written
    /// as code is the one that meant something.
    pub fn default_from(mut self, func: DefaultFn) -> Self {
        self.default_fn = Some(func);
        self
    }

    /// Derive the value from `depends` with `func`.
    ///
    /// Like a related field it has no column of its own, so it is not
    /// stored and is readonly: computing a value and writing it are
    /// opposite directions, and Odoo needs an explicit inverse for the
    /// second one.
    pub fn computed(
        self,
        depends: &[&str],
        func: fn(&serde_json::Map<String, Value>) -> Value,
    ) -> Self {
        Field {
            compute: Some(Compute {
                depends: depends.iter().map(|d| (*d).to_string()).collect(),
                func,
            }),
            stored: false,
            readonly: true,
            ..self
        }
    }

    /// Mirror the value at `path` (`"partner_id.country_id.name"`).
    ///
    /// The field has no column of its own: it is read by following the
    /// path and written by writing the target, so it is not stored and
    /// is readonly here — Odoo lets a related field be made writable,
    /// which needs write-through the ORM does not do yet.
    /// The field is numbered by an `ir.sequence`: creating a record
    /// without a value for it draws the next one.
    pub fn from_sequence(self, code: &str) -> Self {
        Field {
            sequence: Some(code.to_string()),
            ..self
        }
    }

    pub fn related(self, path: &str) -> Self {
        Field {
            related: Some(path.to_string()),
            stored: false,
            readonly: true,
            ..self
        }
    }

    /// Declare the field's default value.
    pub fn default_value(self, value: Value) -> Self {
        Field {
            default: Some(value),
            ..self
        }
    }

    pub fn required(self) -> Self {
        Field {
            required: true,
            ..self
        }
    }

    pub fn readonly(self) -> Self {
        Field {
            readonly: true,
            ..self
        }
    }

    /// Materialize a computed field into a real column
    /// (`odoo/orm/fields.py`'s `store=True`): the ORM writes it whenever
    /// a dependency changes, and in exchange it can be indexed, ordered
    /// and grouped by like any other column.
    ///
    /// It stays readonly to callers — only the recompute writes it.
    pub fn store(self) -> Self {
        debug_assert!(
            self.compute.is_some(),
            "store() materializes a computed field"
        );
        Field {
            stored: true,
            ..self
        }
    }

    /// Mark the field as never returned over RPC (secrets).
    pub fn private(self) -> Self {
        Field {
            exposed: false,
            ..self
        }
    }

    /// PostgreSQL column type; `None` for field types without their own
    /// column (one2many lives on the inverse, many2many in a relation table).
    pub fn column_type(&self) -> Option<String> {
        use FieldType::*;
        Some(match &self.ty {
            Boolean => "bool".into(),
            Integer => "int4".into(),
            Float {
                digits: Some((precision, scale)),
            } => format!("numeric({precision},{scale})"),
            Float { digits: None } => "float8".into(),
            Monetary => "numeric".into(),
            Char { size: Some(n) } => format!("varchar({n})"),
            Char { size: None } | Selection(_) => "varchar".into(),
            Text | Html => "text".into(),
            Date => "date".into(),
            Datetime => "timestamp".into(),
            Binary => "bytea".into(),
            Many2one { .. } => "int4".into(),
            Json => "jsonb".into(),
            One2many { .. } | Many2many { .. } => return None,
        })
    }

    /// The value stored when the field is "not set" but the column holds a
    /// concrete default — mirrors `falsy_value` on Odoo field classes
    /// (0 for numeric, '' for text, false for boolean). `None` means the
    /// only unset representation is SQL NULL.
    pub fn falsy_value(&self) -> Option<Value> {
        use FieldType::*;
        match &self.ty {
            Boolean => Some(Value::Bool(false)),
            Integer | Float { .. } | Monetary => Some(Value::from(0)),
            Char { .. } | Text | Html => Some(Value::String(String::new())),
            _ => None,
        }
    }
}
