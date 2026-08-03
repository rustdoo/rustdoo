{
    'name': 'Vendas',
    'version': '19.0.1.0',
    'category': 'Vendas',
    'summary': 'Pedidos de venda: produtos, linhas e totais',
    'depends': ['base', 'web', 'mail', 'product', 'account', 'stock'],
    'data': [
        'data/sequences.xml',
        'security/sale_groups.xml',
        'security/ir.model.access.csv',
        'views/sale_order_views.xml',
        'views/menus.xml',
        'views/report_sale_order.xml',
    ],
    'installable': True,
}
