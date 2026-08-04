{
    'name': 'Purchase Agreements',
    'version': '19.0.1.0',
    'category': 'Purchase',
    'summary': 'Blanket orders, purchase templates and calls for tender',
    'depends': ['base', 'web', 'mail', 'product', 'uom', 'account', 'stock', 'purchase'],
    'data': [
        'data/sequences.xml',
        'security/purchase_requisition_groups.xml',
        'security/ir.model.access.csv',
        'views/purchase_requisition_views.xml',
        'views/purchase_requisition_wizard_views.xml',
        'views/purchase_views.xml',
        'views/menus.xml',
    ],
    'installable': True,
}
