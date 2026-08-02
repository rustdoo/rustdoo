{
    'name': 'Compras',
    'version': '19.0.1.0',
    'category': 'Compras',
    'summary': 'Pedidos de compra: recebimento e fatura de fornecedor',
    'depends': ['base', 'web', 'mail', 'product', 'account', 'stock'],
    'data': [
        'data/sequences.xml',
        'security/purchase_groups.xml',
        'security/ir.model.access.csv',
        'views/purchase_order_views.xml',
        'views/menus.xml',
    ],
    'installable': True,
}
