{
    'name': 'Projetos',
    'version': '19.0.1.0',
    'category': 'Serviços',
    'summary': 'O trabalho, quem está fazendo e se já acabou',
    'description': """
Um **projeto** é um saco de tarefas com um cliente e um responsável. Uma
**tarefa** é um pedaço de trabalho, e ela vive em duas dimensões ao mesmo
tempo — a parte que vale a pena acertar:

* a **etapa**, a coluna em que ela está, que cada equipe nomeia do seu
  jeito;
* o **estado**, o punhado de respostas que toda equipe quer dizer a mesma
  coisa: em andamento, aguardando, concluída, cancelada.

Uma tarefa em "Sprint 3" não diz nada entre projetos; `1_done` diz a mesma
coisa em todos.
""",
    'depends': ['base', 'mail', 'analytic'],
    'data': [
        'security/ir.model.access.csv',
        'data/project_data.xml',
        'views/project_views.xml',
    ],
    'installable': True,
}
