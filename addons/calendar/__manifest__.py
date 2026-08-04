{
    'name': 'Agenda',
    'version': '19.0.1.0',
    'category': 'Produtividade',
    'summary': 'Reuniões, quem foi convidado e o que cada um respondeu',
    'description': """
Uma reunião (`calendar.event`), o lugar de cada pessoa nela
(`calendar.attendee`) — que é registro próprio porque guarda uma resposta
que uma linha de ligação não guardaria — e a regra que a repete
(`calendar.recurrence`), cujas ocorrências são reuniões comuns apontando de
volta para ela: é isso que deixa alguém mover uma sem mover as outras.

Os convites e lembretes por e-mail não são enviados: dependem de
`mail.template` e da renderização atrás dele, que o port ainda não tem. O
`calendar.alarm` está aqui, com a antecedência como coluna de verdade, para
quando houver o que enviar.
""",
    'depends': ['base', 'mail'],
    'data': [
        'security/ir.model.access.csv',
        'data/calendar_data.xml',
        'views/calendar_views.xml',
        'views/menus.xml',
    ],
    'installable': True,
}
