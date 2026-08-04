{
    'name': 'Produtos',
    'version': '19.0.1.0',
    'category': 'Vendas',
    'summary': 'O catálogo que vendas e faturamento compartilham',
    'depends': ['base', 'web', 'uom'],
    'data': [
        'security/ir.model.access.csv',
        'views/product_views.xml',
        'data/demo_products.xml',
    ],
    'installable': True,
}
