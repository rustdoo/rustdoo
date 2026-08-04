{
    'name': 'Ponto',
    'version': '19.0.1.0',
    'category': 'Recursos Humanos',
    'summary': 'Quem está no trabalho agora, e por quanto tempo esteve',
    'description': """
Um modelo e dois momentos. A **entrada** abre um registro sem saída — a
pessoa está no trabalho. A **saída** fecha, e as horas entre os dois são o
que a folha, o custo de projeto e toda tela de "cadê todo mundo" leem.

As duas regras que o modelo existe para manter são as que uma tela
apressada quebra: uma saída nunca é anterior à entrada, e ninguém entra
duas vezes sem ter saído.

As horas extras ficam de fora: o Odoo as calcula contra a agenda de
trabalho de cada dia e guarda com estado de aprovação. O que está portado
são as horas cruas — que é o que a hora extra é calculada *a partir de*.
""",
    'depends': ['base', 'hr'],
    'data': [
        'security/ir.model.access.csv',
        'views/hr_attendance_views.xml',
    ],
    'installable': True,
}
