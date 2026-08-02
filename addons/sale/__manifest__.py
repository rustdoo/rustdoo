{
    'name': 'Vendas',
    'version': '19.0.1.0',
    'category': 'Vendas',
    'summary': 'Pedidos de venda: produtos, linhas e totais',
    'depends': ['base', 'web'],
    'data': [
        'security/sale_groups.xml',
        'security/ir.model.access.csv',
        'views/product_views.xml',
        'views/sale_order_views.xml',
        'views/menus.xml',
        'data/demo_products.xml',
    ],
    'installable': True,
}
