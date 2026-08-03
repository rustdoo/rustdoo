"""The `odoo` package an addon imports, as far as this port implements it.

This is not Odoo's `odoo` package: it is the surface an addon touches,
projected onto the Rust core. What is here works; what is not here is
absent, and an addon that reaches for it gets an `AttributeError` at
import time rather than a wrong answer at runtime.
"""

from . import api
from . import exceptions
from . import fields
from . import models
from .api import Environment
from .models import AbstractModel, BaseModel, Model, RecordSet, TransientModel

__all__ = [
    "api",
    "exceptions",
    "fields",
    "models",
    "Environment",
    "RecordSet",
    "AbstractModel",
    "BaseModel",
    "Model",
    "TransientModel",
]
