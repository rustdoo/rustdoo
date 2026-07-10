# Rusdoo — Port do Odoo para Rust

Port integral do [Odoo](https://github.com/odoo/odoo) para Rust. O código-fonte
original vive em `./odoo/` (clone shallow, branch `master`) e serve como
referência canônica durante o port.

## Escala do problema (medida em 2026-07-10)

| Métrica | Valor |
|---|---|
| Python (backend) | 8.549 arquivos, ~1,19M linhas |
| JavaScript (web client/Owl) | 5.824 arquivos, ~1,33M linhas |
| Módulos addon | 628 |
| Tamanho do clone shallow | 1,4 GB |

## Arquitetura do workspace

Cada crate espelha um subsistema do núcleo Python (`odoo/odoo/`):

| Crate | Origem no Odoo | Responsabilidade |
|---|---|---|
| `rusdoo-core` | `odoo/api.py`, `odoo/exceptions.py` | Environment, registry, erros |
| `rusdoo-orm` | `odoo/orm/` | Models, fields, domains → PostgreSQL (sqlx) |
| `rusdoo-http` | `odoo/http.py` | Servidor axum, JSON-RPC 2.0, sessões |
| `rusdoo-qweb` | `ir_qweb.py` | Engine de templates QWeb (XML) |
| `rusdoo-modules` | `odoo/modules/` | Manifests, grafo de dependências, loading |
| `rusdoo-server` | `odoo-bin` | Binário `rusdoo` (CLI + bootstrap) |

## Fases do port

O Odoo inteiro depende de ~5% do código (o framework). A ordem é ditada por isso:

1. **Fase 0 — Fundação** *(atual)*: workspace compilando, tipos centrais stub.
2. **Fase 1 — ORM**: fields, domains → SQL, CRUD, `search`/`read`/`write`,
   herança (`_inherit`/`_inherits`), campos computed, access rights.
   É o coração: sem isso nenhum módulo roda.
3. **Fase 2 — HTTP/RPC**: `/jsonrpc`, `/web/dataset/call_kw`, sessões,
   autenticação. Meta: o web client JS original conversa com o backend Rust.
4. **Fase 3 — Módulos + dados**: parser de `__manifest__.py`, loader de XML/CSV
   de dados (`ir.model.data`), instalação do módulo `base`.
5. **Fase 4 — QWeb + relatórios**: templates server-side, views.
6. **Fase 5 — Addons de negócio**: port módulo a módulo em ordem do grafo de
   dependências (`base` → `web` → `mail` → `sale`/`account`/`stock` → …).
   O web client JS (1,33M linhas) pode ser mantido como está — ele só fala
   JSON-RPC — ou portado depois para WASM.

## Build

```sh
cargo build          # compila todos os crates
cargo run -p rusdoo-server
```

## Licença

LGPL-3.0, herdada do Odoo (obrigatória para trabalho derivado).
