from odoo import models, fields


class Country(models.Model):
    _name = "demo.country"
    _description = "A country, whose name is not the same in every language"
    _order = "id"

    name = fields.Char(required=True, translate=True)
    code = fields.Char(size=2)
