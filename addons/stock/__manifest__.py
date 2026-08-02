{
    'name': 'Estoque',
    'version': '19.0.1.0',
    'category': 'Estoque',
    'summary': 'Entregas e recebimentos: documentos e movimentos',
    'depends': ['base', 'web', 'mail', 'product'],
    'data': [
        'data/sequences.xml',
        'security/stock_groups.xml',
        'security/ir.model.access.csv',
        'data/locations.xml',
        'views/stock_picking_views.xml',
        'views/menus.xml',
    ],
    'installable': True,
}
