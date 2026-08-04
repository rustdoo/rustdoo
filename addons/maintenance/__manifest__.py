{
    'name': 'Manutenção',
    'version': '19.0.1.0',
    'category': 'Operações',
    'summary': 'As máquinas, e o que quebra nelas',
    'description': """
Um **equipamento** é algo que a empresa mantém funcionando: uma prensa, uma
van, um notebook. Uma **solicitação** é alguém dizendo que ele precisa de
atenção, e ela anda por **etapas** como qualquer outro funil, terminando
numa marcada como concluída.

As solicitações vêm em dois tipos, e a diferença é a razão do módulo
existir: **corretiva** é algo que quebrou, **preventiva** é algo sendo
cuidado antes de quebrar.
""",
    'depends': ['base', 'mail', 'hr'],
    'data': [
        'security/ir.model.access.csv',
        'data/maintenance_data.xml',
        'views/maintenance_views.xml',
    ],
    'installable': True,
}
