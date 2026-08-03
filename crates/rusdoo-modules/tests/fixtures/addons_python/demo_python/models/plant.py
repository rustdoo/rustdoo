from odoo import models, fields, api
from odoo.exceptions import ValidationError


class Plant(models.Model):
    _name = "demo.plant"
    _description = "A plant the nursery sells"
    _order = "id"

    name = fields.Char(required=True)
    family_id = fields.Many2one("demo.plant.family", required=True)
    height_cm = fields.Integer(default=10)
    label = fields.Char(compute="_compute_label")

    @api.depends("name", "height_cm")
    def _compute_label(self):
        for plant in self:
            plant.label = "%s (%dcm)" % (plant.name, plant.height_cm)

    @api.constrains("height_cm")
    def _check_height(self):
        for plant in self:
            if plant.height_cm <= 0:
                raise ValidationError("%s: a plant has a height" % plant.name)

    def action_prune(self, by=1):
        self.write({"height_cm": max(1, self.height_cm - by)})
        return self.height_cm
