{
    'name': 'Fleet',
    'version': '19.0.1.0',
    'category': 'Human Resources/Fleet',
    'summary': 'Vehicles, their drivers, odometer readings, services and contracts',
    'depends': ['base', 'web', 'mail'],
    'data': [
        'security/fleet_groups.xml',
        'security/ir.model.access.csv',
        'views/fleet_vehicle_model_views.xml',
        'views/fleet_vehicle_views.xml',
        'views/fleet_vehicle_cost_views.xml',
        'views/menus.xml',
        'data/fleet_data.xml',
    ],
    'installable': True,
}
