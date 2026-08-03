{
    'name': 'Código de barras',
    'version': '19.0.1.0',
    'category': 'Estoque',
    'summary': 'Nomenclaturas e regras que dizem o que um código lido é',
    'depends': ['base', 'web'],
    'data': [
        'security/ir.model.access.csv',
        'views/barcodes_views.xml',
        'views/menus.xml',
        'data/barcodes_data.xml',
    ],
    'installable': True,
}
