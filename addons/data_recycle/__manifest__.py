{
    'name': 'Reciclagem de dados',
    'version': '19.0.1.0',
    'category': 'Produtividade',
    'summary': 'Encontra registros velhos ou descartáveis e os arquiva ou apaga',
    'depends': ['base', 'web'],
    'data': [
        'security/ir.model.access.csv',
        'views/data_recycle_model_views.xml',
        'views/data_recycle_record_views.xml',
        'views/menus.xml',
        'data/ir_cron.xml',
    ],
    'installable': True,
}
