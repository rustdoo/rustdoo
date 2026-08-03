from odoo import models, fields


class PlantFamily(models.Model):
    _name = "demo.plant.family"
    _description = "A botanical family"
    _order = "name"

    name = fields.Char(required=True)
    plant_ids = fields.One2many("demo.plant", "family_id")
