{
    'name': 'Warehouse Management: Batch Transfer',
    'version': '19.0.1.0',
    'category': 'Inventory',
    'summary': 'Group transfers into one trip through the warehouse: batches and waves',
    'depends': ['base', 'web', 'mail', 'product', 'stock'],
    'data': [
        'data/stock_picking_batch_data.xml',
        'security/ir.model.access.csv',
        'views/stock_picking_batch_views.xml',
        'views/stock_picking_wave_views.xml',
        'views/stock_picking_views.xml',
        'wizard/stock_picking_to_batch_views.xml',
        'wizard/stock_add_to_wave_views.xml',
        'views/menus.xml',
    ],
    'installable': True,
}
