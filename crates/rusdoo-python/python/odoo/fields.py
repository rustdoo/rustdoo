"""The `odoo.fields` an addon writes against.

Each class here is a *declaration*, not storage: it records what the
field is and hands that to the Rust registry when the model class is
built. What an addon writes is unchanged from Odoo —
`fields.Char(required=True)` — and what the server ends up with is a
`rusdoo_orm::fields::Field`.

The keyword arguments accepted are the ones the port's ORM can honour
today. An unknown one is kept and ignored rather than refused: an addon
that passes `groups=` or `tracking=` should still install, and refusing
it would mean refusing the whole addon over a feature it may not even
depend on. What is *not* silently accepted is a field type the ORM has
no column for — that would be a field nobody could read.
"""


class Field:
    """The base of every field declaration."""

    type = None

    def __init__(self, string=None, **options):
        self.string = string
        self.required = bool(options.pop("required", False))
        self.readonly = bool(options.pop("readonly", False))
        self.default = options.pop("default", None)
        self.translate = bool(options.pop("translate", False))
        self.comodel_name = options.pop("comodel_name", None)
        self.size = options.pop("size", None)
        self.selection = options.pop("selection", None)
        # the name of the method that produces the value, and whether it
        # gets a column of its own. Odoo also accepts a callable here; a
        # name is what the bridge can carry, because what crosses to Rust
        # is a name and never a Python object.
        self.compute = options.pop("compute", None)
        self.store = bool(options.pop("store", False))
        # kept so a later version can honour them without the addon
        # having to change; see the module docstring on why they are not
        # an error
        self.extra = options

    def declare(self, name):
        """The dict the Rust side reads this field from."""
        spec = {
            "name": name,
            "type": self.type,
            "string": self.string,
            "required": self.required,
            "readonly": self.readonly,
            "translate": self.translate,
        }
        if self.comodel_name:
            spec["comodel"] = self.comodel_name
        if self.size:
            spec["size"] = self.size
        if self.selection:
            spec["selection"] = [list(pair) for pair in self.selection]
        if self.compute:
            if callable(self.compute):
                raise TypeError(
                    "%s: compute= must be the method's name, not the method "
                    "itself — what crosses to the ORM is a name" % name
                )
            spec["compute"] = self.compute
            spec["store"] = self.store
            # the metaclass fills this in from the method's `@api.depends`,
            # which it can reach and a field declaration cannot
            spec["depends"] = []
        # a callable default is a function the ORM would have to run;
        # only constants cross for now, and a callable is dropped rather
        # than mis-stored as the function's repr
        if self.default is not None and not callable(self.default):
            spec["default"] = self.default
        return spec


class Char(Field):
    type = "char"

    def __init__(self, string=None, size=None, **options):
        super().__init__(string, size=size, **options)


class Text(Field):
    type = "text"


class Html(Field):
    type = "html"


class Integer(Field):
    type = "integer"


class Float(Field):
    type = "float"

    def __init__(self, string=None, digits=None, **options):
        super().__init__(string, **options)
        self.digits = digits

    def declare(self, name):
        spec = super().declare(name)
        if self.digits:
            spec["digits"] = list(self.digits)
        return spec


class Monetary(Field):
    type = "monetary"


class Boolean(Field):
    type = "boolean"


class Date(Field):
    type = "date"


class Datetime(Field):
    type = "datetime"


class Binary(Field):
    type = "binary"


class Json(Field):
    type = "json"


class Selection(Field):
    type = "selection"

    def __init__(self, selection=None, string=None, **options):
        super().__init__(string, selection=selection, **options)


class Many2one(Field):
    type = "many2one"

    def __init__(self, comodel_name=None, string=None, **options):
        super().__init__(string, comodel_name=comodel_name, **options)


class One2many(Field):
    type = "one2many"

    def __init__(self, comodel_name=None, inverse_name=None, string=None, **options):
        super().__init__(string, comodel_name=comodel_name, **options)
        self.inverse_name = inverse_name

    def declare(self, name):
        spec = super().declare(name)
        spec["inverse"] = self.inverse_name
        return spec


class Many2many(Field):
    type = "many2many"

    def __init__(
        self,
        comodel_name=None,
        relation=None,
        column1=None,
        column2=None,
        string=None,
        **options
    ):
        super().__init__(string, comodel_name=comodel_name, **options)
        self.relation = relation
        self.column1 = column1
        self.column2 = column2

    def declare(self, name):
        spec = super().declare(name)
        spec["relation"] = self.relation
        spec["column1"] = self.column1
        spec["column2"] = self.column2
        return spec
