{
    'name': 'Funcionários',
    'version': '19.0.1.0',
    'category': 'Recursos Humanos',
    'summary': 'As pessoas com quem a empresa trabalha e a forma da organização',
    'description': """
Um **funcionário** é uma pessoa que a empresa emprega; um **departamento**
é onde ela senta e a quem responde; uma **vaga** é o assento, ocupado ou
ainda em recrutamento.

Um funcionário também é um **recurso** — algo que se agenda, com horas de
trabalho e fuso. Não é enfeite: é o que permite planejamento, apontamento
de horas e produção perguntarem quando essa pessoa está disponível sem
saber nada de recursos humanos. Criar um funcionário cria o recurso dele, e
o nome é um valor só nos dois.
""",
    'depends': ['base', 'mail', 'resource'],
    'data': [
        'security/ir.model.access.csv',
        'views/hr_department_views.xml',
        'views/hr_job_views.xml',
        'views/hr_employee_views.xml',
        'views/menus.xml',
    ],
    'installable': True,
}
