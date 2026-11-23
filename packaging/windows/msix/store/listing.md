# Microsoft Store listing — GrafenoDB

Text for Partner Center (Store listings), in English and Spanish. It
describes only what the engine does today; review it against the release
before each submission. Limits: product name 256 characters, short
description 1,000 (shown first), description 10,000, each feature 200.

| Field | Value |
| --- | --- |
| Product | GrafenoDB (Store ID 9PH0179H0JL5) |
| Category | Developer tools |
| Price | Free |
| Package | `GrafenoDB_<version>_x64.msix` from the MSIX workflow |
| Device family | Windows Desktop, Windows 10 version 2004 (build 19041) or later, x64 |

## English

**Product name:** GrafenoDB

**Short description:**
A graph database engine for AI agent memory and structured knowledge. It runs on your computer as a local server with an HTTP API, an MCP server and a command line.

**Description:**

GrafenoDB is a graph database engine for the memory of AI agents and for structured knowledge. It stores nodes and the facts between them, each fact with the time it holds and the time it was recorded, so that what changed and what was known at a date can both be asked.

Starting GrafenoDB opens a console that serves your database on http://127.0.0.1:7810, on this computer only. Agents connect to it through MCP (Model Context Protocol), programs through its HTTP API, and you through the `grafeno` command in PowerShell or the Command Prompt.

Your data stays on your computer: the database and its settings are in the GrafenoDB folder of your user profile, and they stay there if you uninstall the app. GrafenoDB sends nothing anywhere unless you configure it to (webhooks, cluster members).

GrafenoDB is a database engine, not a graphical application: it has a console and a command line. Grafeno Studio, a separate graphical interface, can connect to it.

**Features:**
- Property graph with namespaces, labels and properties on nodes and facts
- Facts with valid time and record time: ask what held, or what was known, at any date
- Hyperedges: facts that relate several nodes at once, each in a role
- GQ, a graph query language with patterns, paths and aggregations
- Full-text search, vector search and hybrid recall for agent memory
- MCP server for AI agents, and an HTTP API for programs
- Accounts, roles and API keys, with a token created at first start
- Consistent backups and checked restores (`grafeno backup`, `grafeno restore`)
- Works offline; listens only on this computer by default

**Getting started (in the description or release notes):**
1. Open GrafenoDB from the Start menu: a console serves your database.
2. In PowerShell: `grafeno config token` prints the access token.
3. `grafeno shell` opens a query console; `grafeno doctor` checks the setup.
4. Back up with `grafeno backup <file>`. Stop the server with Ctrl+C.

**What's new (0.5.x):**
Hyperedges in queries, batches and exports; first release in the Microsoft Store, with the database kept in your profile folder and `grafeno migrate` to copy a database from the setup's installation.

**System requirements:** Windows 10 version 2004 or later, or Windows 11; x64. About 40 MB of disk for the program, plus your data.

**Support:** https://github.com/grafenodb/grafeno/issues — documentation: https://github.com/grafenodb/grafeno/tree/main/docs

**Search terms (up to 7):** graph database, agent memory, knowledge graph, MCP, GQL, vector search, database

## Español

**Nombre del producto:** GrafenoDB

**Descripción breve:**
Un motor de bases de datos de grafos para la memoria de agentes de IA y el conocimiento estructurado. Se ejecuta en tu equipo como servidor local, con API HTTP, servidor MCP y línea de comandos.

**Descripción:**

GrafenoDB es un motor de bases de datos de grafos para la memoria de los agentes de IA y para el conocimiento estructurado. Guarda nodos y los hechos entre ellos, cada hecho con el tiempo en que es válido y el momento en que se registró, para poder preguntar tanto qué cambió como qué se sabía en una fecha.

Al iniciar GrafenoDB se abre una consola que sirve tu base de datos en http://127.0.0.1:7810, solo en este equipo. Los agentes se conectan por MCP (Model Context Protocol), los programas por su API HTTP, y tú con el comando `grafeno` en PowerShell o en el Símbolo del sistema.

Tus datos se quedan en tu equipo: la base de datos y su configuración están en la carpeta GrafenoDB de tu perfil de usuario, y se conservan si desinstalas la aplicación. GrafenoDB no envía nada a ningún sitio salvo que lo configures (webhooks, miembros de un clúster).

GrafenoDB es un motor de bases de datos, no una aplicación gráfica: tiene una consola y una línea de comandos. Grafeno Studio, una interfaz gráfica aparte, puede conectarse a él.

**Funcionalidades:**
- Grafo de propiedades con espacios de nombres, etiquetas y propiedades en nodos y hechos
- Hechos con tiempo de validez y tiempo de registro: qué era cierto, o qué se sabía, en cualquier fecha
- Hiperaristas: hechos que relacionan varios nodos a la vez, cada uno en un rol
- GQ, un lenguaje de consultas de grafos con patrones, caminos y agregaciones
- Búsqueda de texto completo, búsqueda vectorial y recuperación híbrida para memoria de agentes
- Servidor MCP para agentes de IA y API HTTP para programas
- Cuentas, roles y claves de API, con un token creado en el primer inicio
- Copias de seguridad coherentes y restauraciones verificadas (`grafeno backup`, `grafeno restore`)
- Funciona sin conexión; por defecto solo escucha en este equipo

**Primeros pasos:**
1. Abre GrafenoDB desde el menú Inicio: una consola sirve tu base de datos.
2. En PowerShell: `grafeno config token` muestra el token de acceso.
3. `grafeno shell` abre una consola de consultas; `grafeno doctor` revisa la instalación.
4. Haz copias con `grafeno backup <archivo>`. Detén el servidor con Ctrl+C.

**Novedades (0.5.x):**
Hiperaristas en consultas, lotes y exportaciones; primera versión en Microsoft Store, con la base de datos en la carpeta de tu perfil y `grafeno migrate` para copiar una base de datos de la instalación con el instalador.

**Requisitos del sistema:** Windows 10 versión 2004 o posterior, o Windows 11; x64. Unos 40 MB de disco para el programa, más tus datos.

**Soporte:** https://github.com/grafenodb/grafeno/issues — documentación: https://github.com/grafenodb/grafeno/tree/main/docs

**Términos de búsqueda:** base de datos de grafos, memoria de agentes, grafo de conocimiento, MCP, GQL, búsqueda vectorial, base de datos

## Screenshots

Partner Center needs at least one (PNG, 1366×768 or larger). Take them
from the real program, installed from the package, on a clean Windows 11
machine, at 1920×1080, with a dark terminal and no personal paths or
tokens on screen (use a test account whose user folder name is neutral):

1. **The server**: GrafenoDB opened from the Start menu, showing its start
   message (address, database folder, how to stop).
2. **The command line**: PowerShell with `grafeno --version` and
   `grafeno doctor` (all checks passing).
3. **A GQ query**: `grafeno shell` with a small example graph (for
   instance the one in docs/query-language.md) and a `MATCH` query with
   its table of results.
4. **MCP**: an MCP client's configuration pointing at
   `grafeno mcp --url http://127.0.0.1:7810 --token <token>` with the token
   hidden, or `grafeno mcp` answering `tools/list` in a terminal.
5. Optional: `grafeno backup` and its result.

Do not edit what the program prints, and do not show another product's
interface as if it were GrafenoDB's.
