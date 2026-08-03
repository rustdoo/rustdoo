"""The `odoo` package an addon imports, as far as this port implements it.

This is not Odoo's `odoo` package: it is the surface an addon touches,
projected onto the Rust core. What is here works; what is not here is
absent, and an addon that reaches for it gets an `AttributeError` at
import time rather than a wrong answer at runtime.
"""

from . import fields
from . import models
from .models import AbstractModel, BaseModel, Model, TransientModel

__all__ = [
    "fields",
    "models",
    "AbstractModel",
    "BaseModel",
    "Model",
    "TransientModel",
]
