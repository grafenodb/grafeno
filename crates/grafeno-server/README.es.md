# grafeno-server

**Español** | [English](README.md)

El binario `grafeno` sirve y administra una base de datos Grafeno (instaladores:
[docs/install.md](../../docs/install.md)):

- `grafeno serve`: una API HTTP JSON, para aplicaciones como Grafito, y
  Grafeno Studio, la interfaz web, en `/studio/`.
- `grafeno config init | path | token`: el archivo de configuración de un
  servidor instalado (`grafeno.toml`).
- `grafeno service install | uninstall`: el servicio de Windows.
- `grafeno mcp`: un servidor MCP sobre stdio, para agentes (Claude Code, Claude
  Desktop y cualquier otro cliente MCP).
- `grafeno shell`: una consola interactiva para consultas GQ y administración,
  sobre un archivo o sobre un servidor en marcha (como `psql`).
- `grafeno namespaces | stats | check | rebuild | export | import`: comandos de
  administración sobre un archivo (como `pg_dump`).
- `grafeno keys list | create | revoke`: claves de API, sobre un archivo o a
  través de un servidor en marcha (ver *Acceso* más abajo).
- `grafeno backup OUT`: una copia coherente de toda la base de datos, desde un
  archivo o (con `--config`) desde un servidor en marcha, que sigue
  respondiendo. `grafeno check` verifica cada página del archivo contra su suma
  de comprobación antes de revisar los índices: ejecútalo sobre un archivo que
  pueda estar dañado. `grafeno check --recall 100` además estima el recall de
  las búsquedas vectoriales, y `grafeno compact -n NS` (o `--all`) reconstruye
  el grafo vectorial de un espacio de nombres cuando la comprobación informa de
  vectores que las búsquedas no alcanzan.

```bash
cargo install --path crates/grafeno-server
```

```bash
grafeno serve --db memory.redb --token change-me
```

```bash
grafeno mcp --db ~/.grafeno/memory.redb --namespace me
```

Un archivo de base de datos solo puede estar abierto en un proceso a la vez,
así que un servidor HTTP y un servidor MCP no pueden compartir archivo. Usa
`--memory` para una base de datos desechable en memoria.

Opciones de `grafeno serve` (cada una también desde el entorno o `--config`):

| Opción            | Entorno         | Por defecto       |
| ----------------- | --------------- | ----------------- |
| `--config FILE`   | `GRAFENO_CONFIG`| ninguno           |
| `--db FILE`       | `GRAFENO_DB`    | `grafeno.redb`    |
| `--memory`        |                 | desactivado       |
| `--addr ADDR`     | `GRAFENO_ADDR`  | `127.0.0.1:7810`  |
| `--token TOKEN`   | `GRAFENO_TOKEN` | ninguno (sin autenticación) |
| `--no-studio`     |                 | Studio activado   |
| `--cors-origin O` (repetible, `*` para cualquiera) | | ninguno |

## Shell

```bash
grafeno shell --db memory.redb -n vault
```

```bash
grafeno shell --url http://127.0.0.1:7810 --token change-me -n vault
```

Para un clúster, `--url` acepta varios miembros separados por comas; la shell
pasa al siguiente cuando uno no responde y lee sus propias escrituras.

```text
vault=> MATCH (p:Person)-[:works_at]->(c)
vault->   RETURN p.name, c.name LIMIT 3;
 p.name | c.name
--------+--------
 Ana    | Acme
 Bob    | Acme
(2 rows)
vault=> \context who works at Acme?
```

Las sentencias son [GQ](../../docs/query-language.md) y terminan en `;`; las
líneas que empiezan por `\` son metacomandos (`\?` los lista): `\ns` (y
`\ns create NAME`, `\ns drop NAME --yes`), `\use`, `\stats`, `\check`,
`\rebuild`, `\search`, `\recall`, `\context`, `\export`, `\import`,
`\format table|json`, `\timing`, `\i FILE`, `\q`. El historial se guarda en
`~/.grafeno_history`.

Sin terminal ejecuta scripts y termina, con un código de error si una sentencia
falló: `-c "…"`, `-f FILE` (`-` para la entrada estándar) o un script enviado
por tubería. `--json` imprime los resultados como JSON.

```bash
grafeno shell --db memory.redb -n vault -c "MATCH (n) RETURN count(*);"
```

## Comandos de administración

Abren el archivo directamente (detén antes el servidor):

| Comando                                   | Qué hace                                                       |
| ----------------------------------------- | -------------------------------------------------------------- |
| `grafeno namespaces --db F`               | Espacios de nombres con sus recuentos de nodos y aristas.      |
| `grafeno stats --db F [-n NS]...`         | Estadísticas en JSON: recuentos por etiqueta, textos, vectores. |
| `grafeno check --db F [-n NS]...`         | Comprobación de integridad; termina con error si algo está mal. |
| `grafeno rebuild --db F (-n NS... \| --all)` | Reconstruye índices y contadores a partir de los registros. |
| `grafeno export --db F [-n NS]... [-o FILE]` | Exportación JSON Lines (a la salida estándar por defecto).  |
| `grafeno extract --db F -n NS` (`--all`, `--force`) | Lee los textos nuevos y modificados y enlaza lo que mencionan; muestra lo que queda pendiente de revisión. |
| `grafeno import --db F FILE`              | Importa una exportación (`-` lee la entrada estándar), todo o nada; `--bulk` carga exportaciones grandes en bloques. |

```bash
grafeno export --db memory.redb -o backup.jsonl
```

```bash
grafeno import --db restored.redb backup.jsonl
```

| Opción        | Entorno             | Por defecto        | Comando         |
| ------------- | ------------------- | ------------------ | --------------- |
| `--db`        | `GRAFENO_DB`        | `grafeno.redb`     | `serve`, `mcp`  |
| `--memory`    |                     | desactivado        | `serve`, `mcp`  |
| `--addr`      | `GRAFENO_ADDR`      | `127.0.0.1:7810`   | `serve`         |
| `--token`     | `GRAFENO_TOKEN`     | ninguno (sin autenticación) | `serve` |
| `--namespace` | `GRAFENO_NAMESPACE` | `default`          | `mcp`           |

## Docker

```bash
docker build -t grafeno .
```

```bash
docker run -p 7810:7810 -v grafeno-data:/data -e GRAFENO_TOKEN=change-me grafeno
```

La imagen escucha en `0.0.0.0:7810` y guarda los datos en `/data`. Define un
token siempre que el puerto sea accesible para algo más que la aplicación.

## Acceso

Quienes llaman son **cuentas**: **usuarios**, que inician sesión con una
contraseña y obtienen una sesión, y **cuentas de servicio**, que usan claves de
API (14.1). Cada solicitud bajo `/v1` lleva `Authorization: Bearer <secret>`:
el token de acceso de una sesión (`gfs_…`), una clave de API (`gfk_…`) o el
**token** del servidor (`grafeno.toml`, `--token`), una credencial de
emergencia que puede hacerlo todo. Una cuenta tiene permisos: un rol sobre un
espacio de nombres (`vault`), sobre todos los que empiezan por un prefijo
(`team-*`) o sobre todos (`*`). Los roles se suman:

| Rol | Puede |
| --- | --- |
| `read` | leer, buscar, recuperar, ejecutar consultas de lectura y algoritmos |
| `write` | además crear y cambiar nodos y hechos, fusionar, archivar, `touch` |
| `admin` | además índices y restricciones, reconstruir, describir y eliminar el espacio de nombres, crearlo |

Los **roles** (14.2) van más allá: un rol reúne privilegios, niveles
predefinidos sobre patrones de espacios de nombres y otros roles, y las cuentas
tienen roles. Un privilegio es una acción sobre un objeto, concedida o
denegada; una denegación prevalece sobre cualquier concesión. Sobre el
servidor: `create_namespace`, `manage_accounts`, `manage_roles`, `backup`,
`metrics`, `audit`, `cluster`. Sobre un patrón de espacio de nombres: `read`,
`create`, `update`, `delete`, `merge`, `retract`, `import`, `export`,
`schema`, `algorithms`, `drop`, `create_namespace` (los niveles de arriba son
conjuntos de estos: `GET /v1/privileges` los lista). Sobre una etiqueta o una
propiedad: `read` y `write` (las consultas los comprueban desde 14.4). Un rol
llamado `public`, cuando existe, es de todas las cuentas.

```bash
curl -X PUT localhost:7810/v1/roles/team-reader -H "Authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"privileges": [
        {"action": "read", "object": {"on": "namespace", "namespace": "team-*"}},
        {"action": "read", "object": {"on": "namespace", "namespace": "team-secret"}, "deny": true}]}'
```

```bash
curl -X PATCH localhost:7810/v1/accounts/ana -H "Authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' -d '{"roles": ["team-reader"]}'
```

El acceso también se gestiona en GQ (14.3): `CREATE USER`, `ALTER USER`,
`CREATE ROLE`, `GRANT`, `DENY`, `REVOKE`, `GRANT ROLE`, `SHOW USERS`,
`SHOW ROLES`, `SHOW PRIVILEGES [FOR name]`, `SHOW CURRENT USER` (ver
[el lenguaje de consultas](../../docs/query-language.md#users-roles-and-privileges)),
con `grafeno roles` y `grafeno grants`, y en la vista Access de Studio.

```bash
grafeno roles create team_reader --config grafeno.toml
```

```bash
grafeno grants grant read,export --on "namespace:team-*" --to team_reader --config grafeno.toml
```

```bash
grafeno grants deny read --on property:Person.email@crm --to team_reader --config grafeno.toml
```

```bash
grafeno grants add-role team_reader --to ana --config grafeno.toml
```

Sin token, las cuentas cierran el servidor: la primera cuenta debe ser un
superusuario, y se rechaza cualquier cambio que dejaría sin superusuarios
activos (o sin cuentas). Las cuentas pueden caducar (`ALTER USER … SET
EXPIRES`, `expires_at`): una cuenta caducada responde 401 `account_expired`.

Una cuenta también puede ser **superusuario** (todo, incluida la gestión de
cuentas; `owner` en respuestas antiguas) y puede limitarse a **sus propios
registros**: entonces solo cambia o elimina los nodos y hechos que registró
(los de otros dan `403 forbidden`, también en GQ). Cada escritura registra el
nombre de su cuenta: `recorded_by` en nodos y aristas, `n.@recorded_by` en GQ.
Una clave solo ve los espacios de nombres que puede leer: en `/v1/namespaces`,
`SHOW NAMESPACES` y las exportaciones.

```bash
grafeno keys create research-agent --grant vault:write --grant "team-*:read" --config grafeno.toml
```

```bash
grafeno keys list --config grafeno.toml
```

```bash
grafeno keys revoke research-agent --config grafeno.toml
```

Con `--config`, los comandos pasan por el servidor en marcha (su dirección y su
token); con `--db`, abren un archivo de base de datos cuyo servidor está
detenido. El secreto se muestra una sola vez; solo se guarda su SHA-256. Una
clave creada así con permisos es una cuenta de servicio con su nombre que la
posee. `grafeno users create|list|password|disable|enable|delete` gestionan
las cuentas (`--superuser`, `--service`, `--grant`). Un servidor sin token ni
cuentas está abierto a cualquiera que llegue a él; su primera cuenta
(`grafeno users create … --superuser`, o el primer registro) debe ser un
superusuario, y a partir de entonces cada solicitud necesita una credencial.

### Cuentas e inicio de sesión (14.1)

| Método | Ruta | Cuerpo | Devuelve |
| --- | --- | --- | --- |
| `GET` | `/v1/auth/status` | | `{open, setup, password_sign_in}` (cualquiera) |
| `POST` | `/v1/auth/setup` | `{name, password}` | en un servidor abierto sin cuentas: la primera, un superusuario, con la sesión iniciada (201) |
| `POST` | `/v1/auth/signin` | `{name, password}` | `{access_token, expires_in, refresh_token, refresh_expires_in, session, account}`; 401 si es incorrecto, 429 si está bloqueada |
| `POST` | `/v1/auth/refresh` | `{refresh_token}` | tokens nuevos, ambos reemplazados; un token de renovación usado dos veces termina la sesión |
| `POST` | `/v1/auth/signout` | | termina la sesión de quien llama |
| `GET` | `/v1/auth/sessions` | `?account=` (superusuarios; `*` para todas) | las sesiones, con `current` marcando la de quien llama |
| `DELETE` | `/v1/auth/sessions/{id}` | | termina una de las de quien llama (un superusuario: la de cualquiera) |
| `POST` | `/v1/auth/password` | `{current_password, new_password}` | la cambia; las demás sesiones terminan |
| `GET`, `POST` | `/v1/accounts` | `{name, kind: user\|service, password?, grants, roles, superuser, own_only, description}` | `manage_accounts`; solo los superusuarios crean superusuarios |
| `GET`, `PATCH`, `DELETE` | `/v1/accounts/{name}` | `{password?, grants?, roles?, superuser?, own_only?, disabled?, description?}` | `manage_accounts`, no sobre superusuarios; desactivarla o una contraseña nueva terminan sus sesiones |
| `GET` | `/v1/privileges` | | cada acción por objeto y los niveles predefinidos (cualquiera con sesión) |
| `GET`, `POST`, `PATCH`, `DELETE` | `/v1/admin/users`, `/v1/admin/users/{name}` | como `/v1/accounts`, más `expires_at` (microsegundos, ISO 8601 o `null`) | las cuentas con el nombre del diseño (14.3) |
| `GET`, `PUT`, `DELETE` | `/v1/admin/roles`, `/v1/admin/roles/{name}` | como `/v1/roles` | los roles (`manage_roles`) |
| `GET` | `/v1/admin/grants?holder=name` | | `{columns: [source, access, privilege, on], rows}`: los privilegios de una cuenta o un rol (los propios: cualquiera; los de otros: `manage_roles` o `manage_accounts`) |
| `POST` | `/v1/admin/grants` | `{privileges: ["read", "editor", …], on: {on: …}, to: [roles], deny?}` | concede o deniega; `{rows: [["granted 2 privileges …"]]}` (`manage_roles`) |
| `POST` | `/v1/admin/grants/revoke` | `{privileges, on, from: [roles], which?: grants\|denials\|both}` | revoca |
| `POST` | `/v1/admin/grants/roles`, `/v1/admin/grants/roles/revoke` | `{roles, to}` / `{roles, from}` | da o quita roles: a cuentas `manage_accounts`, a roles `manage_roles` |
| `GET` | `/v1/roles` | | los roles (`manage_roles`) |
| `GET`, `PUT`, `DELETE` | `/v1/roles/{name}` | `{privileges: [{action, object: {on: server\|namespace\|label\|property, namespace?, label?, edges?, property?}, deny?}], grants, roles, description}` | un rol; `PUT` lo define o reemplaza (201 si es nuevo) y rechaza ciclos; `DELETE` se lo quita a todos |
| `GET`, `POST` | `/v1/keys` | `{name, account?, scope?: {namespaces, read_only}, expires_in_days?, description?}` | las claves propias de una cuenta (superusuarios: todas); el secreto una vez |
| `POST` | `/v1/keys/{name}/rotate` | | un secreto nuevo, una vez; el anterior deja de funcionar |
| `DELETE` | `/v1/keys/{name}` | | la revoca |

Un token de acceso caducado responde 401 `session_expired` (renuévalo); una
clave caducada, `key_expired`; una cuenta desactivada, `account_disabled`.
`/v1/whoami` nombra la cuenta, cómo llegó (`via: {kind, name}`), el `scope` de
una clave y cada rol que alcanza (`effective_roles`). Un rechazo nombra el
privilegio que falta (`` the account "carl" has no `delete` privilege on the
namespace "other" ``). Por defecto, las sesiones duran 15 minutos (acceso) y 30
días (renovación) (`[auth]` en `grafeno.toml`).

## API HTTP

Todos los cuerpos son JSON. Todo lo que está bajo `/v1` necesita
`Authorization: Bearer <secret>` (el token o una clave), salvo que el servidor
esté abierto. Los errores tienen la forma
`{"error": {"code": "not_found", "message": "…"}}`, con los códigos
`bad_request` (cuerpo mal formado), `invalid` (entrada rechazada),
`invalid_query` (un error de GQ), `query_limit` (una consulta GQ que supera su
límite de tiempo, lecturas, filas, memoria o coste; ver `[query]` en
`grafeno.toml`), `not_found`, `conflict` (409: ya existe), `unauthorized` (401:
sin un secreto válido), `forbidden` (403: la clave no puede hacer esto), `busy`
(503 con `Retry-After`: demasiadas solicitudes a la vez o demasiadas escrituras
en espera) e `internal`.

Las escrituras de solicitudes concurrentes se confirman juntas (*group
commit*): cada solicitud sigue recibiendo su propio resultado de todo o nada, y
una escritura que falla no afecta a las demás. Las lecturas se ejecutan sobre
instantáneas, en paralelo entre sí y junto al escritor; un recorrido grande de
GQ de solo lectura también se reparte entre hilos, que comparten todas las
consultas (`scan_threads` en `[query]`, uno por CPU por defecto).

Los identificadores son UUID. Las fechas en las respuestas son microsegundos
desde la época Unix; las solicitudes aceptan eso o una cadena ISO 8601
(`2024-05-01`, `2024-05-01T10:30:00Z`).

| Método   | Ruta                                  | Cuerpo / consulta                                       | Devuelve                      |
| -------- | ------------------------------------- | ------------------------------------------------------- | ----------------------------- |
| `GET`    | `/health`                             |                                                         | `{status, version}`           |
| `GET`    | `/v1/namespaces`                      |                                                         | `[{name, nodes, edges, created_at, description?}]`, también los vacíos |
| `POST`   | `/v1/namespaces`                      | `{name, description?}`                                  | el espacio de nombres (201); 409 si existe |
| `GET`    | `/v1/namespaces/{ns}`                 |                                                         | el espacio de nombres, o 404  |
| `PATCH`  | `/v1/namespaces/{ns}`                 | `{description}` (`null` la quita)                       | el espacio de nombres         |
| `DELETE` | `/v1/namespaces/{ns}`                 |                                                         | `{nodes, edges}` eliminados con él |
| `GET`    | `/v1/backup`                          |                                                         | todo el archivo de base de datos, coherente (propietarios) |
| `GET`    | `/v1/export`                          | `?ns=a,b` (todos por defecto)                           | JSON Lines                    |
| `POST`   | `/v1/import`                          | JSON Lines (hasta 1 GiB); `?bulk=true` carga en bloques entre otras escrituras | `{namespaces, nodes, edges}`  |
| `GET`    | `/v1/{ns}/stats`                      |                                                         | estadísticas                  |
| `POST`   | `/v1/{ns}/compact`                    |                                                         | reconstruye el grafo vectorial (admin): `{vectors, dangling_before, unreachable_before, unreachable_after}` |
| `GET`    | `/v1/{ns}/vectors/recall`             | `?sample=100&ef=64`                                     | `{sample, ef, recall}`: recall@10 estimado |
| `GET`    | `/v1/{ns}/check`                      |                                                         | `{…, problems}`               |
| `POST`   | `/v1/{ns}/rebuild`                    |                                                         | informe de reconstrucción     |
| `POST`   | `/v1/{ns}/nodes`                      | nodo (con `id`: upsert)                                 | `{id, created}`               |
| `GET`    | `/v1/{ns}/nodes`                      | `?label=&after=&limit=`                                 | `{items, next}`               |
| `GET`    | `/v1/{ns}/edges`                      | `?label=&after=&limit=`                                 | `{items, next}`               |
| `POST`   | `/v1/{ns}/read-many`                  | `{nodes: [id], edges: [id]}` (512 como máximo en total) | `{nodes, edges}` en el orden pedido, `null` para los que faltan, una instantánea (`read`) |
| `GET`    | `/v1/{ns}/nodes/{id}`                 |                                                         | nodo con `text` y `memory`    |
| `PUT`    | `/v1/{ns}/nodes/{id}`                 | nodo                                                    | `{id, created}`               |
| `PATCH`  | `/v1/{ns}/nodes/{id}`                 | parche                                                  | nodo con `text`               |
| `DELETE` | `/v1/{ns}/nodes/{id}`                 |                                                         | 204                           |
| `GET`    | `/v1/{ns}/nodes/{id}/neighbors`       | `?direction=&label=a,b&valid_at=&known_at=`             | `[{node, edge}]`              |
| `POST`   | `/v1/{ns}/edges`                      | arista                                                  | `{id}`                        |
| `GET`    | `/v1/{ns}/edges/{id}`                 |                                                         | arista                        |
| `DELETE` | `/v1/{ns}/edges/{id}`                 |                                                         | 204                           |
| `POST`   | `/v1/{ns}/edges/{id}/invalidate`      | `{at}`                                                  | arista                        |
| `POST`   | `/v1/{ns}/edges/{id}/retract`         |                                                         | arista                        |
| `POST`   | `/v1/{ns}/neighborhoods`              | `{seeds: [id] (8 como máximo), direction, filter}`      | un subgrafo de profundidad uno por semilla, en orden, una instantánea (`read`) |
| `POST`   | `/v1/{ns}/k-hop`                      | `{start, depth, direction, filter}`                     | `[{node, hops}]`              |
| `POST`   | `/v1/{ns}/path`                       | `{from, to, max_depth, direction, filter}`              | `{path}` (o `null`)           |
| `POST`   | `/v1/{ns}/subgraph`                   | `{seeds, depth, direction, filter}`                     | `{nodes, edges}`              |
| `POST`   | `/v1/{ns}/search/text`                | `{query, limit}`                                        | `[{node, score}]`             |
| `POST`   | `/v1/{ns}/search/vector`              | `{embedding, limit, ef}`                                | `[{node, score}]`             |
| `POST`   | `/v1/{ns}/recall`                     | consulta de recuperación                                | `[{node, score, signals}]`    |
| `POST`   | `/v1/{ns}/context`                    | consulta de recuperación + `{budget, fact_share, max_facts, facts, communities}` | `{overview, items, facts, tokens, rendered}`; `communities` pone primero ese número de resúmenes de comunidad que coinciden |
| `POST`   | `/v1/{ns}/batch`                      | `{ops}`                                                 | `{results, refs}`             |
| `POST`   | `/v1/{ns}/query`                      | `{query, params?}` ([GQ](../../docs/query-language.md)) | `{columns, rows, stats}`      |
| `GET`    | `/v1/whoami`                          |                                                         | quien llama: `{name, owner, grants, own_only}` |
| `GET`    | `/v1/metrics`                         |                                                         | contadores e histogramas en el formato de texto de Prometheus (cualquier secreto) |
| `GET`    | `/v1/keys`                            |                                                         | las claves, nunca sus secretos (propietarios) |
| `POST`   | `/v1/keys`                            | `{name, grants: [{namespace, role}], owner?, own_only?, description?}` | `{key, secret}` (201; el secreto solo aquí) |
| `DELETE` | `/v1/keys/{name}`                     |                                                         | 204                           |
| `GET`    | `/v1/algorithms`                      |                                                         | `[{name, arguments, columns, description}]` |
| `POST`   | `/v1/{ns}/algo/{name}`                | `{args?, settings?, write?, column?}`                   | `{columns, rows, stats}`      |
| `GET`    | `/v1/{ns}/resolve`                    | `?name=&label=`                                         | nodos con ese nombre o alias  |
| `GET`    | `/v1/{ns}/duplicates`                 | `?label=&min_score=&vectors=&limit=`                    | `[{a, b, score, reasons, conflicts}]` |
| `POST`   | `/v1/{ns}/merge`                      | `{keep, merge, force?, dry_run?, functional?, strong_keys?}` | `{id, kept, merged, edges_moved, edges_dropped, conflicts}`; 409 con `error.conflicts` cuando discrepan; `{conflicts}` con `dry_run` |
| `GET`    | `/v1/{ns}/merges`                     | `?node=&limit=`                                         | registros de fusión, los más recientes primero |
| `POST`   | `/v1/{ns}/merges/{id}/undo`           | `{force?}` (opcional)                                   | el registro, marcado como deshecho |
| `GET`    | `/v1/{ns}/fuzzy`                      | `?label=&key=&q=&limit=&measure=`                       | `[{node, score}]`: nodos cuya propiedad se parece a `q` (necesita un índice difuso) |
| `GET`    | `/v1/{ns}/constraints`                |                                                         | restricciones sobre hechos y propiedades funcionales (`CREATE CONSTRAINT` en GQ) |
| `GET`    | `/v1/{ns}/contradictions`             | `?limit=`                                               | `[{constraint, action, new_edge, old_edge, replacement, at, by}]`, las más recientes primero |
| `GET`    | `/v1/{ns}/nodes/{id}/history/{key}`   |                                                         | `[{value, from, to, by}]`: valores anteriores de una propiedad funcional |
| `GET`    | `/v1/{ns}/export.graphml`             |                                                         | el espacio de nombres como GraphML (sin los hechos retractados) |
| `GET`    | `/v1/{ns}/export/nodes.csv`, `/export/edges.csv` |                                              | como CSV, con las cabeceras de `neo4j-admin import` |
| `POST`   | `/v1/{ns}/import/graphml`, `/import/nodes.csv`, `/import/edges.csv` | el archivo (admin)        | `{nodes, edges, failed, errors}`; los ids de Grafeno se actualizan, otros ids se asignan a nodos nuevos |
| `GET`    | `/v1/{ns}/changes`                    | `?since=&limit=`; `Accept: text/event-stream` o `?stream=true` para un flujo | `{changes, last, oldest, gap}`, o eventos enviados por el servidor (`id` = número de secuencia; se reanuda con `Last-Event-ID`) |
| `POST`   | `/v1/{ns}/ingest`                     | NDJSON: `{"key", "node": {…}}` o `{"key", "edge": {from, label, to, …}}` por línea; los extremos pueden ser `$key` | `{lines, nodes, edges, duplicates, failed, errors}`; una clave enviada de nuevo no escribe nada |
| `GET`    | `/v1/{ns}/webhooks`                   |                                                         | los webhooks del espacio de nombres (admin) |
| `POST`   | `/v1/{ns}/webhooks`                   | `{url, secret, labels, since}`                          | una URL que recibe los cambios en orden, firmados con `X-Grafeno-Signature: sha256=<HMAC>` |
| `GET`    | `/v1/{ns}/webhooks/{id}`              |                                                         | su cursor, entregas, fallos y último error |
| `DELETE` | `/v1/{ns}/webhooks/{id}`              |                                                         | lo detiene |
| `GET`    | `/v1/background`                      |                                                         | lo que hicieron las tareas de `[background]` (propietario) |
| `POST`   | `/v1/{ns}/communities/refresh`        | `{min_size, resolution, max_levels, labels, edge_labels, weight, central, terms, facts}`, todos opcionales | `{nodes, levels, communities, created, updated, unchanged, removed, stale, modularity}` |
| `GET`    | `/v1/{ns}/communities`                | `?q=&level=&limit=`                                     | resúmenes de comunidad que coinciden con `q`, o los principales sin él |
| `GET`    | `/v1/{ns}/communities/{id}`           | `?limit=`                                               | `{community, children, members}`: el detalle |
| `PUT`    | `/v1/{ns}/communities/{id}/text`      | `{text}` (`null` para deshacer)                         | el texto propio de un agente para un resumen |
| `POST`   | `/v1/{ns}/extract`                    | `{source_labels?, entity_labels?, identifiers?, min_phrase_sources?, force?, …}` (opcional) | `{read, unchanged, mentions, linked, created, queued, found}` |
| `GET`    | `/v1/{ns}/review`                     | `?limit=`                                               | `[{key, kind, value, occurrences, candidates}]` |
| `POST`   | `/v1/{ns}/review/{key}`               | `{action: "link", node}` / `{action: "create", label?}` / `{action: "dismiss"}` | `{node, report}` |
| `POST`   | `/v1/{ns}/nodes/{id}/mentions`        | `{mentions: [{surface, label?, props?, start?, end?}], default_label?}` | el informe |
| `GET`    | `/v1/{ns}/match-rules`                |                                                         | las reglas de coincidencia del espacio de nombres |
| `PUT`    | `/v1/{ns}/match-rules/{label}`        | `{strong_keys?, fields: [{key, measure, weight?, …}], neighbors?, same?, review?}` | la regla (admin) |
| `DELETE` | `/v1/{ns}/match-rules/{label}`        |                                                         | 204 (admin)                   |
| `POST`   | `/v1/{ns}/match`                      | `{label, props, neighbors?, limit?, rule?}`             | `[{node, score, decision, decided_by, fields, neighbors}]` |
| `GET`    | `/v1/{ns}/matches`                    | `?label=&limit=`                                        | `[{a, b, match}]`: pares que la regla considera iguales o pendientes de revisión |
| `POST`   | `/v1/{ns}/touch`                      | `{ids}`                                                 | 204                           |
| `GET`    | `/v1/{ns}/weak`                       | `?half_life_days=&threshold=&label=&limit=`             | `[{node, strength, memory}]`  |
| `POST`   | `/v1/{ns}/archive`                    | `{ids}`                                                 | `{archived}`                  |
| `GET`    | `/v1/{ns}/consolidation`              | `?older_than_days=&min_size=&labels=a,b&limit=`         | `[{entity, memories}]`        |
| `POST`   | `/v1/{ns}/summaries`                  | `{text, sources, about?, name?, labels?, embedding?, archive_sources?}` | `{id}` (201)  |

- **Algoritmos de grafos** (`pageRank`, `louvain`, `shortestPath`, … ver
  [GQ](../../docs/query-language.md#graph-algorithms)): `args` son los
  argumentos anteriores a la configuración (ids de nodos, `k`), `settings` el
  mapa de configuración (`{"labels": "Person", "weight": "km"}`). Las filas
  contienen los nodos como `{id, labels, name}`. Con `write: "rank"` (o
  `"@embedding"`) el resultado se guarda en cada nodo, a partir de `column` (la
  que sigue a `node` por defecto), y la respuesta es el número de nodos
  escritos.
- **Listados**: devuelven hasta `limit` elementos (100 por defecto, 1000 como
  máximo) en orden de id; cuando `next` tiene valor, pásalo como `after` para
  obtener la página siguiente.
- **nodo**: `{labels, props?, sources?, text?, embedding?}`. `PUT` y los upserts
  reemplazan el nodo entero; un `text` o `embedding` ausente se elimina, y las
  aristas se conservan.
- **parche**: `{props?, add_labels?, remove_labels?, text?, embedding?,
  importance?}`. `props` se combina (`null` elimina una clave); `text: null` o
  `embedding: null` los eliminan.
- **arista**: `{from, label, to, props?, sources?, valid_from?, valid_to?}`.
- **filter**: `{labels?, valid_at?, known_at?}`. Sin `known_at`, las aristas
  retractadas quedan fuera.
- **consulta de recuperación**: `{text?, embedding?, limit?, labels?,
  candidates?, expand?, ranking?, include_archived?, touch?}`, donde `expand` es
  `true` (por defecto), `false` o `{depth, direction, filter, decay, seeds}`;
  `ranking` es `true` (por defecto: cuentan la importancia y la recencia),
  `false` o `{importance_weight, recency_weight, half_life_days}`; las memorias
  archivadas quedan fuera salvo con `include_archived`; `touch` registra un
  acceso a cada resultado.
- **memory** (en las respuestas de nodos): `{importance, accesses,
  last_access}`.

### Lotes

Un lote se ejecuta en una sola transacción: si una operación falla, no se
escribe nada y el mensaje de error empieza por `operation N:`. Las operaciones
pueden nombrar los nodos que crean con `ref` (`"$name"`), y las operaciones
posteriores pueden usar ese nombre donde acepten un id de nodo. Operaciones:
`create_node`, `upsert_node` (`node.id` obligatorio), `update_node`
(`{id, patch}`), `delete_node`, `create_edge`, `delete_edge`,
`delete_edges_from` (`{from, label?}`), `invalidate_edge` (`{id, at}`) y
`retract_edge`.

Sincronizar una nota y reemplazar sus enlaces en una sola solicitud:

```json
{ "ops": [
  { "op": "upsert_node", "ref": "$note",
    "node": { "id": "0190f5c2-7a3e-7cde-8f01-00000000000a", "labels": ["Note"],
              "props": { "title": "Rust" }, "text": "The borrow checker…" } },
  { "op": "delete_edges_from", "from": "$note", "label": "links_to" },
  { "op": "create_edge", "edge": { "from": "$note", "label": "links_to",
                                   "to": "0190f5c2-7a3e-7cde-8f01-00000000000b" } }
] }
```

### Clúster

En un miembro de un clúster (`[cluster]` en `grafeno.toml`, ver
[docs/install.md](../../docs/install.md)), cualquier miembro acepta
cualquier solicitud: un seguidor reenvía las escrituras (y las lecturas
linealizables) al líder, esperando una elección si no lo hay; sin la dirección
`api` del líder responde `not_leader` (421) indicando el líder. Cabeceras:

| Cabecera | Dirección | Significado |
| --- | --- | --- |
| `x-grafeno-position` | respuesta | la posición aplicada del miembro (un índice Raft, igual en todos los miembros) |
| `x-grafeno-after: N` | solicitud | espera hasta 5 s a que el miembro aplique N (503 si no): leer tus propias escrituras |
| `x-grafeno-consistency` | solicitud | `local` (por defecto), `bounded` (al líder cuando el miembro va más de 1 000 entradas atrasado) o `linearizable` (a través del líder, con ReadIndex) |
| `x-grafeno-split` | solicitud | `auto` (por defecto: una lectura se reparte entre los miembros cuando su coste estimado alcanza `split_cost`), `on` (siempre que su forma lo permita) u `off`; una respuesta repartida incluye `split: {parts, here}` |
| `Idempotency-Key` | solicitud | en `POST /v1/{ns}/query` (o `idempotency_key` en su cuerpo): una escritura con la misma clave se ejecuta una vez, y los envíos posteriores devuelven su primer resultado |

Una lectura repartida pide a cada miembro su parte con
`POST /v1/{ns}/query/part` (interna: el miembro que reparte envía el plan, la
posición y las credenciales de quien llama). Los webhooks y el trabajo en
segundo plano se ejecutan solo en el líder. Rutas del propietario:

| Método | Ruta | Cuerpo | Devuelve |
| --- | --- | --- | --- |
| `GET` | `/v1/cluster` | | `{id, role, term, leader, commit, applied, last_index, snapshot_index, voters, outgoing}` |
| `POST` | `/v1/cluster/voters` | `{voters: [ids]}` | `{index}` (en el líder) |
| `POST` | `/v1/cluster/compact` | | `{snapshot_index}` |
| `GET` | `/v1/cluster/groups` | | cada grupo Raft: `{id, voters, held, status}` (13.5) |
| `GET` | `/v1/cluster/placement` | | `[{namespace, group, moving_to}]`: los espacios de nombres colocados fuera del grupo 0 |
| `POST` | `/v1/cluster/placement` | `{namespace, group}` | lo mueve sin detenerlo: `{from, to, records, catch_up_rounds, paused_ms, …}` |

Con varios grupos, una solicitud para un espacio de nombres va a su grupo (en
cualquier miembro); las escrituras durante la pausa de un traslado responden
`busy` (503) y deben reintentarse. Las rutas internas bajo
`/v1/cluster/groups/{id}/` transportan las copias, la barrera y el directorio
de un traslado.

`grafeno cluster status|voters|compact --url … --token … [--group N]` las
llaman; `/v1/cluster/groups/{id}/status`, `…/voters` y `…/compact` hacen lo
mismo para otro grupo. El estado (13.6) también cuenta elecciones, cambios de
líder e instantáneas enviadas e instaladas, indica el formato de entradas que
escribe el grupo y lo que ejecuta cada miembro (`members`) y, en el líder, el
retraso de cada uno de los demás miembros (`peers`); `/v1/metrics` los ofrece
como series `grafeno_cluster_*` por grupo. `GET /v1/backup?group=N` copia el
archivo de otro grupo. Ver
[docs/cluster-runbooks.md](../../docs/cluster-runbooks.md).

## MCP

`grafeno mcp` habla MCP (versiones de protocolo 2025-06-18, 2025-03-26 y
2024-11-05) por stdin y stdout, y escribe su registro en stderr. Sus
herramientas:

| Herramienta | Qué hace                                                                      |
| ----------- | ----------------------------------------------------------------------------- |
| `remember` | Guarda una memoria (texto, nombre opcional, etiquetas, importancia y enlaces). Un `name` conocido actualiza esa entidad. |
| `recall`   | Búsqueda híbrida; devuelve ids, nombres, fragmentos y por qué coincidió cada resultado. Registra los accesos. |
| `context`  | Memorias y hechos relevantes como texto compacto dentro de un presupuesto de tokens, después de los resúmenes de comunidad que coinciden. |
| `relate`   | Registra un hecho (arista) entre dos nodos, con fechas de validez opcionales; indica qué hechos contradijo bajo una restricción (y terminó). |
| `update`   | Reemplaza el texto de un nodo, lo renombra, combina propiedades, cambia etiquetas o importancia. |
| `forget`   | Invalida (por defecto), retracta o elimina un hecho; elimina un nodo.         |
| `get_node` | Un nodo con su texto, sus propiedades y los hechos que lo conectan.           |
| `query`    | Una sentencia [GQ](../../docs/query-language.md) de solo lectura; 200 filas como máximo. |
| `merge`    | Fusiona un nodo duplicado en otro; se rechaza si discrepan, salvo con `force`. Devuelve un `merge_id`. |
| `undo_merge` | Deshace una fusión por su `merge_id`: el nodo fusionado y sus hechos vuelven. |
| `find_matches` | Antes de crear una entidad, los nodos que ya podrían ser ella, según la regla de coincidencia de la etiqueta, con una decisión y la similitud de cada campo. |
| `extract`  | Lee las memorias nuevas y modificadas y enlaza lo que mencionan (nombres, correos, URL, usuarios, hashtags, fechas, importes, nombres nuevos recurrentes). |
| `add_mentions` | Las entidades que menciona una memoria, según la lectura propia del agente: enlazadas, creadas o en espera. |
| `review` / `resolve_review` | Lo que la extracción no pudo decidir, y resolverlo (enlazar, crear, descartar). |
| `maintain` | Lista posibles duplicados, memorias débiles y grupos listos para consolidar.  |
| `consolidate` | Guarda un resumen de varias memorias y las archiva.                        |
| `overview` | Para preguntas amplias: resúmenes de comunidad que coinciden con un tema, o los principales; con `id`, las partes y los miembros de una comunidad. |
| `summarize_communities` | Encuentra las comunidades (y comunidades de comunidades) y escribe o actualiza sus resúmenes. |
| `set_summary` | Reemplaza el texto de un resumen de comunidad por el propio del agente (o lo deshace). |
| `changes`  | Lo que se escribió desde un número de secuencia: nodos y hechos creados, actualizados y eliminados. |

Cada herramienta acepta un `namespace` opcional, que por defecto es
`--namespace`.

El servidor MCP abre el archivo de base de datos por sí mismo, así que no puede
impedir el paso al usuario que lo inicia, pero sí acota al agente al que sirve:

| Opción | Efecto |
| --- | --- |
| `--allow vault,team-*` | los espacios de nombres que el agente puede usar (todos por defecto) |
| `--read-only` | solo se ofrecen las herramientas de lectura (y `recall` no registra accesos) |
| `--as NAME` | sus escrituras se registran como `NAME` (`@recorded_by`) |
| `--own-only` | con `--as`: solo puede cambiar lo que registró |

Claude Code:

```bash
claude mcp add grafeno -- grafeno mcp --db ~/.grafeno/memory.redb --namespace me
```

Claude Desktop (`claude_desktop_config.json`) u otros clientes:

```json
{
  "mcpServers": {
    "grafeno": {
      "command": "grafeno",
      "args": ["mcp", "--db", "/path/to/memory.redb", "--namespace", "me"]
    }
  }
}
```
