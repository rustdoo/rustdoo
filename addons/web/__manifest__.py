{
    'name': 'Web',
    'version': '19.0.1.0',
    'category': 'Hidden',
    'summary': 'O cliente web do rusdoo: menus, listas e formulários',
    'depends': [],
    'data': [],
    # ordem de carga do bundle: utilitários, camada RPC, views e por
    # último o boot, que só roda quando o resto já existe
    'assets': {
        'web.assets_backend': [
            'web/static/src/core/utils.js',
            'web/static/src/core/rpc.js',
            'web/static/src/views/list_view.js',
            'web/static/src/views/form_view.js',
            'web/static/src/webclient/webclient.js',
            'web/static/src/webclient/webclient.css',
        ],
    },
    'installable': True,
}
