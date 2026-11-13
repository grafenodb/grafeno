# grafeno-server

**Español** | [English](README.md)

El binario `grafeno` sirve y administra una base de datos Grafeno
(instaladores: [docs/instalacion.md](../../docs/instalacion.md)):

- `grafeno serve`: una API HTTP JSON, para aplicaciones y agentes, sin
  interfaz (Grafeno Studio se instala aparte y se conecta a esta API).
- `grafeno setup`: inicializa una instancia: su carpeta de datos, la
  configuración (un token nuevo), el primer administrador y el servicio,
  habilitado e iniciado; interactivo, o cada respuesta como flag o en un
  archivo de respuestas (`--unattended`, `--answers FILE`).
- `grafeno instances [list | drop NAME [--purge] | rollback NAME]`: las
  instancias de esta máquina (`default` es `grafeno.toml` y
  `grafeno.service`, las demás `<nombre>.toml` y
  `grafeno@<nombre>.service`); `rollback` restaura la copia que guardó la
  última actualización de formato.
- `grafeno doctor [--instance NAME]`: revisa la configuración y sus
  permisos, la carpeta de datos, el formato de la base, el espacio libre en
  disco, el puerto y el servicio; termina con error si una revisión falla.
- `grafeno config init | path | token`: el archivo de configuración de un
  servidor instalado (`grafeno.toml`).
- `grafeno service install [--account "NT SERVICE\Grafeno"] | uninstall`:
  el servicio de Windows (LocalSystem salvo que se indique una cuenta).
- `grafeno mcp`: un servidor MCP sobre stdio, para agentes (cualquier
  cliente MCP).
- `grafeno shell`: una consola interactiva para consultas GQ y
  administración, sobre un archivo o sobre un servidor en marcha.
- `grafeno namespaces | stats | check | rebuild | export | import`:
  comandos de administración sobre un archivo.
- `grafeno keys list | create | revoke`: claves de API, sobre un archivo o a
  través de un servidor en marcha (ver *Acceso* más abajo).
- `grafeno backup OUT`: una copia consistente de toda la base de datos,
  desde un archivo o (con `--config`) desde un servidor en marcha, que sigue
  atendiendo. `grafeno restore BACKUP NEW` recupera un respaldo en un archivo
  de base nuevo (páginas verificadas, formato actualizado, namespaces
  comprobados; el respaldo solo se lee y nunca se reemplaza un archivo
  existente). `grafeno check` verifica cada página del archivo contra su
  suma de comprobación antes de revisar los índices: ejecútalo sobre un
  archivo que pueda estar dañado. `grafeno check --recall 100` además estima
  el recall de las búsquedas vectoriales e informa cuántos vectores se
  eliminaron desde que se construyó cada grafo vectorial; `grafeno compact
  -n NS` (o `--all`) reconstruye el grafo vectorial de un namespace y el de
  cada uno de sus índices vectoriales. El servidor además compacta por su
  cuenta un grafo desgastado más allá de `compact_vectors_at` (ver
  `[background]` en [docs/instalacion.md](../../docs/instalacion.md#configuración)).

```bash
cargo install --path crates/grafeno-server
```

```bash
grafeno serve --db memory.redb --token change-me
```

```bash
grafeno mcp --db ~/.grafeno/memory.redb --namespace me
```

Un archivo de base solo puede estar abierto en un proceso a la vez, así que
un servidor HTTP y un servidor MCP no pueden compartir un archivo. Usa
`--memory` para una base desechable en memoria.

Opciones de `grafeno serve` (cada una también desde el entorno o `--config`):

| Opción            | Entorno         | Por defecto      |
| ----------------- | --------------- | ---------------- |
| `--config FILE`   | `GRAFENO_CONFIG`| ninguno          |
| `--db FILE`       | `GRAFENO_DB`    | `grafeno.redb`   |
| `--memory`        |                 | desactivado      |
| `--addr ADDR`     | `GRAFENO_ADDR`  | `127.0.0.1:7810` |
| `--token TOKEN`   | `GRAFENO_TOKEN` | ninguno (sin autenticación) |
| `--cors-origin O` (repetible, `*` para cualquiera) | | ninguno |
| `--stop-with-stdin` |               | desactivado: se detiene limpiamente al cerrarse la entrada estándar (para un programa que inicia el motor, como Grafeno Desktop) |

## Consola

```bash
grafeno shell --db memory.redb -n vault
```

```bash
grafeno shell --url http://127.0.0.1:7810 --token change-me -n vault
```

Para un clúster, `--url` acepta varios miembros, separados por comas; la
consola pasa al siguiente cuando uno no responde, y lee sus propias
escrituras.

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
líneas que empiezan con `\` son metacomandos (`\?` los lista): `\ns` (y
`\ns create NAME`, `\ns drop NAME --yes`), `\use`, `\stats`, `\check`,
`\rebuild`, `\search`, `\recall`, `\context`, `\export`, `\import`,
`\format table|json`, `\timing`, `\trace` (cada sentencia como `TRACE`, sus
eventos en una tabla), `\i FILE`, `\q`. `BEGIN` (o `BEGIN READ ONLY`),
`COMMIT` y `ROLLBACK` ejecutan las sentencias entre ellos en una
transacción, sobre un archivo o sobre un servidor; el prompt muestra `*`
mientras hay una abierta, y un script o una sesión que termina dentro de
una la revierte. El historial se guarda en `~/.grafeno_history`.

Sin terminal ejecuta scripts y termina, con un código de error si una
sentencia falló: `-c "…"`, `-f FILE` (`-` para la entrada estándar), o un
script enviado por una tubería. `--json` imprime los resultados como JSON;
`--stop-on-error` se detiene en la primera sentencia que falla (por defecto
el resto se sigue ejecutando). Los scripts pueden crear su namespace con
`CREATE NAMESPACE … IF NOT EXISTS` (ver la guía del lenguaje de consultas).

```bash
grafeno shell --db memory.redb -n vault -c "MATCH (n) RETURN count(*);"
```

## Comandos de administración

Abren el archivo directamente (detén antes el servidor):

| Comando                                   | Qué hace                                                       |
| ----------------------------------------- | -------------------------------------------------------------- |
| `grafeno namespaces --db F`               | Los namespaces con su número de nodos y aristas.               |
| `grafeno namespaces --db F create NAME [--description TEXT] [--if-not-exists]` | Crea un namespace vacío. |
| `grafeno namespaces --db F describe NAME [TEXT]` | Fija (o, sin TEXT, quita) su descripción.               |
| `grafeno namespaces --db F drop NAME --yes` | Elimina un namespace y todo lo que contiene.                 |
| `grafeno stats --db F [-n NS]...`         | Estadísticas en JSON: conteos por etiqueta, textos, vectores.  |
| `grafeno check --db F [-n NS]...`         | Revisión de integridad; termina con error si algo está mal.    |
| `grafeno rebuild --db F (-n NS... \| --all)` | Reconstruye índices y contadores a partir de los registros.  |
| `grafeno export --db F [-n NS]... [-o FILE]` | Exportación JSON Lines (salida estándar por defecto).       |
| `grafeno extract --db F -n NS` (`--all`, `--force`) | Lee los textos nuevos y modificados y enlaza lo que mencionan; imprime lo que espera revisión. |
| `grafeno import --db F FILE`              | Importa una exportación (`-` lee la entrada estándar), todo o nada; `--bulk` carga exportaciones grandes por tramos. |

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

## Acceso

Quienes llaman son **cuentas**: **usuarios**, que inician sesión con una
contraseña y obtienen una sesión, y **cuentas de servicio**, que usan claves
de API (14.1). Cada petición bajo `/v1` lleva
`Authorization: Bearer <secreto>`: el token de acceso de una sesión
(`gfs_…`), una clave de API (`gfk_…`), o el **token** del servidor
(`grafeno.toml`, `--token`), una credencial de emergencia que puede hacerlo
todo. Una cuenta tiene permisos: un rol sobre un namespace (`vault`), sobre
cada namespace que empieza con un prefijo (`team-*`), o sobre todos (`*`).
Los roles se suman:

| Rol | Puede |
| --- | --- |
| `read` | leer, buscar, recordar, ejecutar consultas de lectura y algoritmos |
| `write` | además crear y cambiar nodos y hechos, fusionar, archivar, registrar accesos |
| `admin` | además índices y restricciones, reconstruir, describir y eliminar el namespace, crearlo |

Los **roles** (14.2) van más allá: un rol contiene privilegios, niveles
predefinidos sobre patrones de namespace y otros roles, y las cuentas
tienen roles. Un privilegio es una acción sobre un objeto, concedida o
denegada; una denegación gana sobre cualquier concesión. Sobre el servidor:
`create_namespace`, `manage_accounts`, `manage_roles`, `backup`, `metrics`,
`audit`, `cluster`. Sobre un patrón de namespace: `read`, `create`,
`update`, `delete`, `merge`, `retract`, `import`, `export`, `schema`,
`algorithms`, `drop`, `create_namespace`, `events` (suscripciones de eventos
y sus valores, 16.10) (los niveles de arriba son grupos de estos:
`GET /v1/privileges` los lista). Sobre una etiqueta o una propiedad: `read`
y `write` (comprobados por las consultas desde la 14.4). Un rol llamado
`public`, si está definido, es de todas las cuentas.

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

El acceso también se administra en GQ (14.3): `CREATE USER`, `ALTER USER`,
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

**Aplicación** (14.4): lo que quien llama no puede leer está ausente de
toda respuesta, como si no existiera: nodos con una etiqueta que no puede
leer (o que una política le oculta), aristas que no puede leer o que llevan
a esos nodos, y propiedades ocultas. Los listados, las lecturas por id
(404), los recorridos, las búsquedas, recall y context, GQ, los algoritmos,
las exportaciones, el registro de cambios (y su stream), las estadísticas y
la lista de namespaces los omiten. La resolución de entidades, los
duplicados, las fusiones, los resúmenes de comunidades, la extracción, la
cola de revisión, el decaimiento, la consolidación, `check` y el recall
vectorial responden 403 a quien ve un namespace registro por registro. Cada
cambio se comprueba contra etiquetas, propiedades y la operación; una
escritura que tocaría algo prohibido falla entera. Las **políticas de
registros** limitan un rol a los registros que cumplen una condición:

```bash
curl -X PUT localhost:7810/v1/roles/analyst -H "Authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"grants": [{"namespace": "crm", "role": "read"}],
       "policies": [{"namespace": "crm", "label": "Customer", "applies": "read",
                     "condition": {"kind": "property", "key": "tenant",
                                   "equals": {"from": "attribute", "name": "tenant"}}}]}'
```

```bash
curl -X PATCH localhost:7810/v1/accounts/ana -H "Authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' -d '{"attributes": {"tenant": "acme"}}'
```

**El registro de auditoría** (14.5) guarda cada evento de seguridad una
vez: inicios de sesión y sus fallos, bloqueos, sesiones, claves, cambios en
cuentas y roles (concesiones, denegaciones y políticas incluidas) y cada
petición rechazada, con la cuenta, la clave o sesión, la dirección, el
objetivo y el resultado. Leerlo requiere el privilegio `audit`:

```bash
curl "localhost:7810/v1/audit?since=0&limit=100" -H "Authorization: Bearer $TOKEN"
```

```bash
curl -N localhost:7810/v1/audit -H "Authorization: Bearer $TOKEN" -H "Accept: text/event-stream"
```

```bash
curl "localhost:7810/v1/audit?format=jsonl" -H "Authorization: Bearer $TOKEN" > audit.jsonl
```

En GQ, `CALL db.audit(0, 1000) YIELD seq, at, kind, account, via, address,
target, outcome, detail`. `[audit]` en `grafeno.toml` fija cuánto se
conserva (`max_entries`, `max_age_days`). Quienes administran cuentas
listan y terminan las sesiones de otras cuentas (no las de superusuarios).

Sin token, las cuentas cierran el servidor: la primera cuenta debe ser un
superusuario, y se rechaza un cambio que dejaría sin ningún superusuario
activo (o sin cuentas). Las cuentas pueden expirar
(`ALTER USER … SET EXPIRES`, `expires_at`): una cuenta expirada responde 401
`account_expired`.

Una cuenta también puede ser **superusuario** (todo, incluido administrar
cuentas; `owner` en respuestas antiguas) y puede limitarse a **sus propios
registros**: entonces solo cambia o elimina nodos y hechos que registró
ella (los de otros dan `403 forbidden`, también en GQ). Cada escritura
registra el nombre de su cuenta: `recorded_by` en nodos y aristas,
`n.@recorded_by` en GQ. Una clave solo ve los namespaces que puede leer: en
`/v1/namespaces`, `SHOW NAMESPACES` y las exportaciones.

```bash
grafeno keys create research-agent --grant vault:write --grant "team-*:read" --config grafeno.toml
```

```bash
grafeno keys list --config grafeno.toml
```

```bash
grafeno keys revoke research-agent --config grafeno.toml
```

Con `--config`, los comandos pasan por el servidor en marcha (su dirección
y token); con `--db`, abren un archivo de base cuyo servidor está detenido.
El secreto se imprime una vez; solo se guarda su SHA-256. Una clave creada
así con permisos es una cuenta de servicio con su nombre que la contiene.
`grafeno users create|list|password|disable|enable|delete` administran las
cuentas (`--superuser`, `--service`, `--grant`). Un servidor sin token ni
cuentas está abierto a cualquiera que llegue a él; su primera cuenta
(`grafeno users create … --superuser`, o el primer registro) debe ser un
superusuario, y desde entonces cada petición necesita una credencial.

### Cuentas e inicio de sesión (14.1)

| Método | Ruta | Cuerpo | Devuelve |
| --- | --- | --- | --- |
| `GET` | `/v1/auth/status` | | `{open, setup, password_sign_in}` (cualquiera) |
| `POST` | `/v1/auth/setup` | `{name, password}` | en un servidor abierto sin cuentas: la primera, un superusuario, con sesión iniciada (201) |
| `POST` | `/v1/auth/signin` | `{name, password}` | `{access_token, expires_in, refresh_token, refresh_expires_in, session, account}`; 401 si es incorrecto, 429 si está bloqueada |
| `POST` | `/v1/auth/refresh` | `{refresh_token}` | tokens nuevos, ambos reemplazados; un refresh token usado dos veces termina la sesión |
| `POST` | `/v1/auth/signout` | | termina la sesión de quien llama |
| `GET` | `/v1/auth/sessions` | `?account=` (superusuarios; `*` para todas) | las sesiones, con `current` marcando la de quien llama |
| `DELETE` | `/v1/auth/sessions/{id}` | | termina una de las de quien llama (un superusuario: la de cualquiera) |
| `POST` | `/v1/auth/password` | `{current_password, new_password}` | la cambia; las demás sesiones terminan |
| `GET`, `POST` | `/v1/accounts` | `{name, kind: user\|service, password?, grants, roles, superuser, own_only, description}` | `manage_accounts`; solo los superusuarios crean superusuarios |
| `GET`, `PATCH`, `DELETE` | `/v1/accounts/{name}` | `{password?, grants?, roles?, superuser?, own_only?, disabled?, description?}` | `manage_accounts`, no sobre superusuarios; deshabilitarla o una contraseña nueva terminan sus sesiones |
| `GET` | `/v1/privileges` | | cada acción por objeto, y los niveles predefinidos (cualquiera con sesión) |
| `GET` | `/v1/audit` | `?since=&limit=`, `?format=jsonl`, o `Accept: text/event-stream` | eventos de auditoría (`audit`): `{events, last, oldest, gap}`, JSON Lines, o un stream |
| `GET`, `POST` | `/v1/admin/users` | como `/v1/accounts`, más `expires_at` (microsegundos, ISO 8601, o `null`) | las cuentas con el nombre del diseño (14.3) |
| `GET`, `PATCH`, `DELETE` | `/v1/admin/users/{name}` | como `/v1/accounts/{name}`, más `expires_at` | una cuenta |
| `GET` | `/v1/admin/roles` | | los roles (`manage_roles`) |
| `GET`, `PUT`, `DELETE` | `/v1/admin/roles/{name}` | como `/v1/roles/{name}` | un rol |
| `GET` | `/v1/admin/grants?holder=name` | | `{columns: [source, access, privilege, on], rows}`: los privilegios de una cuenta o un rol (los propios: cualquiera; los de otros: `manage_roles` o `manage_accounts`) |
| `POST` | `/v1/admin/grants` | `{privileges: ["read", "editor", …], on: {on: …}, to: [roles], deny?}` | concede o deniega; `{rows: [["granted 2 privileges …"]]}` (`manage_roles`) |
| `POST` | `/v1/admin/grants/revoke` | `{privileges, on, from: [roles], which?: grants\|denials\|both}` | revoca |
| `POST` | `/v1/admin/grants/roles`, `/v1/admin/grants/roles/revoke` | `{roles, to}` / `{roles, from}` | da o quita roles: a cuentas `manage_accounts`, a roles `manage_roles` |
| `GET` | `/v1/roles` | | los roles (`manage_roles`) |
| `GET`, `PUT`, `DELETE` | `/v1/roles/{name}` | `{privileges: [{action, object: {on: server\|namespace\|label\|property, namespace?, label?, edges?, property?}, deny?}], grants, roles, description}` | un rol; `PUT` lo define o reemplaza (201 si es nuevo), rechazando ciclos; `DELETE` se lo quita a todos |
| `GET`, `POST` | `/v1/keys` | `{name, account?, scope?: {namespaces, read_only}, expires_in_days?, description?}` | las claves propias de una cuenta (superusuarios: todas); el secreto una vez |
| `POST` | `/v1/keys/{name}/rotate` | | un secreto nuevo, una vez; el anterior deja de funcionar |
| `DELETE` | `/v1/keys/{name}` | | la revoca |

Un token de acceso expirado responde 401 `session_expired` (renuévalo); una
clave expirada `key_expired`; una cuenta deshabilitada `account_disabled`.
`/v1/whoami` nombra la cuenta, cómo llegó (`via: {kind, name}`), el `scope`
de una clave y cada rol que alcanza (`effective_roles`). Un rechazo nombra
el privilegio que falta (`` the account "carl" has no `delete`
privilege on the namespace "other" ``). Las sesiones duran 15 minutos
(acceso) y 30 días (renovación) por defecto (`[auth]` en `grafeno.toml`).

## API HTTP

Todos los cuerpos son JSON. Todo lo que está bajo `/v1` necesita
`Authorization: Bearer <secreto>` (el token o una clave) salvo que el
servidor esté abierto. Los errores tienen la forma
`{"error": {"code": "not_found", "message": "…"}}`, con los códigos
`bad_request` (cuerpo mal formado), `invalid` (entrada rechazada),
`invalid_query` (un error de GQ), `query_limit` (una consulta GQ por encima
de su límite de tiempo, lecturas, filas, memoria o costo; ver `[query]` en
`grafeno.toml`), `not_found`, `conflict` (409: ya existe), `unauthorized`
(401: sin un secreto válido), `forbidden` (403: la clave no puede hacer
esto), `busy` (503 con `Retry-After`: demasiadas peticiones a la vez, o
demasiadas escrituras en espera) e `internal`.

Las escrituras de peticiones concurrentes se confirman juntas (group
commit): cada petición sigue obteniendo su propio resultado de todo o nada,
y una escritura que falla no afecta a las demás. Las lecturas se ejecutan
sobre snapshots, en paralelo y junto al escritor; un recorrido GQ grande de
solo lectura además se reparte entre hilos, que comparten todas las
consultas (`scan_threads` en `[query]`, uno por CPU por defecto).

Los ids son UUID. Los tiempos en las respuestas son microsegundos desde la
época Unix; las peticiones aceptan eso o una cadena ISO 8601 (`2024-05-01`,
`2024-05-01T10:30:00Z`).

| Método   | Ruta                                  | Cuerpo / consulta                                       | Devuelve                      |
| -------- | ------------------------------------- | ------------------------------------------------------- | ----------------------------- |
| `GET`    | `/health`                             |                                                         | `{status, version, layout}`   |
| `GET`    | `/v1/about`                           | (público)                                               | `{product, version, api: {version, supported}, layout, capabilities, auth: {token, accounts, open, password_sign_in}, cluster}`: ver *La API como contrato* |
| `GET`    | `/v1/settings`                        | (superusuarios)                                         | la configuración con la que corre el servidor, sin el token ni el secreto del clúster |
| `GET`    | `/v1/namespaces`                      |                                                         | `[{name, nodes, edges, created_at, description?}]`, también los vacíos |
| `POST`   | `/v1/namespaces`                      | `{name, description?}`                                  | el namespace (201); 409 si existe |
| `GET`    | `/v1/namespaces/{ns}`                 |                                                         | el namespace, o 404           |
| `PATCH`  | `/v1/namespaces/{ns}`                 | `{description}` (`null` la quita)                       | el namespace                  |
| `DELETE` | `/v1/namespaces/{ns}`                 |                                                         | `{nodes, edges}` eliminados con él |
| `GET`    | `/v1/backup`                          |                                                         | todo el archivo de la base, consistente (owners) |
| `GET`    | `/v1/export`                          | `?ns=a,b` (todos por defecto)                           | JSON Lines                    |
| `POST`   | `/v1/import`                          | JSON Lines (hasta 1 GiB); `?bulk=true` carga por tramos entre otras escrituras | `{namespaces, nodes, edges}`  |
| `GET`    | `/v1/{ns}/stats`                      |                                                         | estadísticas                  |
| `POST`   | `/v1/{ns}/compact`                    | `?index=` (un índice vectorial; sin él, los vectores del propio namespace) | reconstruye ese grafo vectorial (admin): `{vectors, dangling_before, unreachable_before, unreachable_after}` |
| `GET`    | `/v1/{ns}/vectors/health`             |                                                         | `[{index, dimension, vectors, removed, repaired, churn, building}]`: las eliminaciones de cada grafo vectorial desde que se construyó (`churn` = removed / vectors), solo de su metadata |
| `GET`    | `/v1/{ns}/vectors/recall`             | `?sample=100&ef=64`                                     | `{sample, ef, recall}`: recall@10 estimado |
| `GET`    | `/v1/{ns}/check`                      |                                                         | `{…, problems}`               |
| `POST`   | `/v1/{ns}/rebuild`                    |                                                         | informe de la reconstrucción  |
| `POST`   | `/v1/{ns}/nodes`                      | nodo (con `id`: upsert)                                 | `{id, created}`               |
| `GET`    | `/v1/{ns}/nodes`                      | `?label=&after=&limit=`                                 | `{items, next}`               |
| `GET`    | `/v1/{ns}/edges`                      | `?label=&after=&limit=`                                 | `{items, next}`               |
| `POST`   | `/v1/{ns}/read-many`                  | `{nodes: [id], edges: [id]}` (512 como máximo en total) | `{nodes, edges}` en el orden pedido, `null` para los que faltan, un snapshot (`read`) |
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
| `POST`   | `/v1/{ns}/neighborhoods`              | `{seeds: [id] (8 como máximo), direction, filter}`      | un subgrafo de profundidad uno por semilla, en orden, un snapshot (`read`) |
| `POST`   | `/v1/{ns}/k-hop`                      | `{start, depth, direction, filter}`                     | `[{node, hops}]`              |
| `POST`   | `/v1/{ns}/path`                       | `{from, to, max_depth, direction, filter}`              | `{path}` (o `null`)           |
| `POST`   | `/v1/{ns}/subgraph`                   | `{seeds, depth, direction, filter}`                     | `{nodes, edges}`              |
| `POST`   | `/v1/{ns}/search`                     | `{text?, index?, embedding?, mode, fusion?, k?, text_weight?, limit, candidates, fragments}` | `[{node, score, text_rank?, text_score?, vector_rank?, similarity?, fragments?}]`: `mode` `text`, `vector` o `hybrid` (16.6) |
| `POST`   | `/v1/{ns}/search/text`                | `{query, limit, index?, fragments?}`                    | `[{node, score, fragments?}]`: con `index`, un índice de texto completo y su lenguaje de consulta |
| `POST`   | `/v1/{ns}/search/vector`              | `{embedding \| embeddings, limit, ef?, index?, filter?: {labels, props}}` | `[{node, score}]`, o una lista por embedding: un índice vectorial con `index`, aplicando `filter` durante el recorrido (16.7) |
| `POST`   | `/v1/{ns}/similar`                    | `{node, limit, ef?, index?, filter?}`                   | `[{node, score}]`: los más cercanos al vector del propio nodo, sin incluirlo (16.7) |
| `POST`   | `/v1/{ns}/recall`                     | consulta de recall                                      | `[{node, score, signals}]`    |
| `POST`   | `/v1/{ns}/context`                    | consulta de recall + `{budget, fact_share, max_facts, facts, communities}` | `{overview, items, facts, tokens, rendered}`; `communities` antepone esa cantidad de resúmenes de comunidades que coinciden |
| `POST`   | `/v1/{ns}/batch`                      | `{ops}`                                                 | `{results, refs}`             |
| `POST`   | `/v1/{ns}/query`                      | `{query, params?, trace?}` ([GQ](../../docs/query-language.md)) | `{columns, rows, stats}`, y `trace` con `trace: true` (16.9) |
| `POST`   | `/v1/{ns}/transactions`               | `{read_only?, timeout_secs?}`                           | 201 `{id, read_only, started_at, timeout_secs, statements, failed}`: una transacción explícita (16.4) |
| `GET`    | `/v1/{ns}/transactions/{id}`          |                                                         | su estado |
| `POST`   | `/v1/{ns}/transactions/{id}/query`    | `{query, params?}`                                      | una sentencia dentro de ella: `{columns, rows, stats}` |
| `POST`   | `/v1/{ns}/transactions/{id}/commit`   |                                                         | `{id, committed}`; 409 `rolled_back` si una sentencia falló o un clúster perdió su líder |
| `POST`   | `/v1/{ns}/transactions/{id}/rollback` |                                                         | `{id, rolled_back}` |
| `POST`   | `/v1/{ns}/query/part`                 | una parte de una consulta repartida (entre miembros, 13.4) | las filas y estadísticas de la parte |
| `GET`    | `/v1/whoami`                          |                                                         | quien llama: `{name, owner, grants, own_only}` |
| `GET`    | `/v1/metrics`                         |                                                         | contadores e histogramas en el formato de texto de Prometheus (cualquier secreto) |
| `GET`    | `/v1/keys`                            |                                                         | las claves, nunca sus secretos (owners) |
| `POST`   | `/v1/keys`                            | `{name, grants: [{namespace, role}], owner?, own_only?, description?}` | `{key, secret}` (201; el secreto solo aquí) |
| `DELETE` | `/v1/keys/{name}`                     |                                                         | 204                           |
| `GET`    | `/v1/algorithms`                      |                                                         | `[{name, arguments, columns, description}]` |
| `POST`   | `/v1/{ns}/algo/{name}`                | `{args?, settings?, write?, column?, stats?}`           | `{columns, rows, stats}`: con `stats: true`, una fila por columna numérica (`column, count, min, max, mean, sum`) en lugar de las filas; `mode` y `write` no van en `settings` |
| `GET`    | `/v1/{ns}/resolve`                    | `?name=&label=`                                         | nodos con ese nombre o alias  |
| `GET`    | `/v1/{ns}/duplicates`                 | `?label=&min_score=&vectors=&limit=`                    | `[{a, b, score, reasons, conflicts}]` |
| `POST`   | `/v1/{ns}/merge`                      | `{keep, merge, force?, dry_run?, functional?, strong_keys?}` | `{id, kept, merged, edges_moved, edges_dropped, conflicts}`; 409 con `error.conflicts` cuando discrepan; `{conflicts}` con `dry_run` |
| `GET`    | `/v1/{ns}/merges`                     | `?node=&limit=`                                         | registros de fusiones, los más recientes primero |
| `POST`   | `/v1/{ns}/merges/{id}/undo`           | `{force?}` (opcional)                                   | el registro, marcado como deshecho |
| `GET`    | `/v1/{ns}/fuzzy`                      | `?label=&key=&q=&limit=&measure=`                       | `[{node, score}]`: nodos cuya propiedad se parece a `q` (necesita un índice difuso) |
| `GET`    | `/v1/{ns}/constraints`                |                                                         | restricciones sobre hechos y propiedades: uno por origen, funcional, no nulo, tipo (`CREATE CONSTRAINT` en GQ; las restricciones de unicidad y clave son índices) |
| `GET`    | `/v1/{ns}/contradictions`             | `?limit=`                                               | `[{constraint, action, new_edge, old_edge, replacement, at, by}]`, las más recientes primero |
| `GET`    | `/v1/{ns}/nodes/{id}/history/{key}`   |                                                         | `[{value, from, to, by}]`: valores anteriores de una propiedad funcional |
| `GET`    | `/v1/{ns}/export.graphml`             |                                                         | el namespace como GraphML (sin los hechos retractados) |
| `GET`    | `/v1/{ns}/export/nodes.csv`, `/export/edges.csv` |                                              | como CSV, con cabeceras tipadas (`name:int`, `:LABEL`) |
| `POST`   | `/v1/{ns}/import/graphml`, `/import/nodes.csv`, `/import/edges.csv` | el archivo (admin)        | `{nodes, edges, failed, errors}`; los ids de Grafeno actualizan, los demás ids se asignan a nodos nuevos |
| `GET`    | `/v1/{ns}/changes`                    | `?since=&limit=`; `Accept: text/event-stream` o `?stream=true` para un stream | `{changes, last, oldest, gap}`, o server-sent events (`id` = número de secuencia; se reanuda con `Last-Event-ID`) |
| `GET`    | `/v1/{ns}/events/{name}`              | `?since=&limit=`; `Accept: text/event-stream` o `?stream=true` para un stream | los eventos de una suscripción con sus valores: `{changes, last, oldest, gap, through}` (continúa después de `through`), o server-sent events llamados `event`; necesita `read` y `events` |
| `POST`   | `/v1/{ns}/ingest`                     | NDJSON: `{"key", "node": {…}}` o `{"key", "edge": {from, label, to, …}}` por línea; los extremos pueden ser `$key` | `{lines, nodes, edges, duplicates, failed, errors}`; una clave enviada de nuevo no escribe nada |
| `GET`    | `/v1/{ns}/webhooks`                   |                                                         | los webhooks del namespace (admin) |
| `POST`   | `/v1/{ns}/webhooks`                   | `{url, secret, labels, since}`                          | una URL que recibe los cambios en orden, firmados con `X-Grafeno-Signature: sha256=<HMAC>` |
| `GET`    | `/v1/{ns}/webhooks/{id}`              |                                                         | su cursor, entregas, fallos y último error |
| `DELETE` | `/v1/{ns}/webhooks/{id}`              |                                                         | lo detiene |
| `GET`    | `/v1/background`                      |                                                         | lo que hicieron las tareas de `[background]` (owner) |
| `POST`   | `/v1/{ns}/communities/refresh`        | `{min_size, resolution, max_levels, labels, edge_labels, weight, central, terms, facts}`, todos opcionales | `{nodes, levels, communities, created, updated, unchanged, removed, stale, modularity}` |
| `GET`    | `/v1/{ns}/communities`                | `?q=&level=&limit=`                                     | los resúmenes de comunidades que coinciden con `q`, o los principales sin él |
| `GET`    | `/v1/{ns}/communities/{id}`           | `?limit=`                                               | `{community, children, members}`: profundizar |
| `PUT`    | `/v1/{ns}/communities/{id}/text`      | `{text}` (`null` para deshacer)                         | el texto propio de un agente para un resumen |
| `POST`   | `/v1/{ns}/extract`                    | `{source_labels?, entity_labels?, identifiers?, min_phrase_sources?, force?, …}` (opcional) | `{read, unchanged, mentions, linked, created, queued, found}` |
| `GET`    | `/v1/{ns}/review`                     | `?limit=`                                               | `[{key, kind, value, occurrences, candidates}]` |
| `POST`   | `/v1/{ns}/review/{key}`               | `{action: "link", node}` / `{action: "create", label?}` / `{action: "dismiss"}` | `{node, report}` |
| `POST`   | `/v1/{ns}/nodes/{id}/mentions`        | `{mentions: [{surface, label?, props?, start?, end?}], default_label?}` | el informe |
| `GET`    | `/v1/{ns}/match-rules`                |                                                         | las reglas de coincidencia del namespace |
| `PUT`    | `/v1/{ns}/match-rules/{label}`        | `{strong_keys?, fields: [{key, measure, weight?, …}], neighbors?, same?, review?}` | la regla (admin) |
| `DELETE` | `/v1/{ns}/match-rules/{label}`        |                                                         | 204 (admin)                   |
| `POST`   | `/v1/{ns}/match`                      | `{label, props, neighbors?, limit?, rule?}`             | `[{node, score, decision, decided_by, fields, neighbors}]` |
| `GET`    | `/v1/{ns}/matches`                    | `?label=&limit=`                                        | `[{a, b, match}]`: pares que la regla considera iguales o para revisar |
| `POST`   | `/v1/{ns}/touch`                      | `{ids}`                                                 | 204                           |
| `GET`    | `/v1/{ns}/weak`                       | `?half_life_days=&threshold=&label=&limit=`             | `[{node, strength, memory}]`  |
| `POST`   | `/v1/{ns}/archive`                    | `{ids}`                                                 | `{archived}`                  |
| `GET`    | `/v1/{ns}/consolidation`              | `?older_than_days=&min_size=&labels=a,b&limit=`         | `[{entity, memories}]`        |
| `POST`   | `/v1/{ns}/summaries`                  | `{text, sources, about?, name?, labels?, embedding?, archive_sources?}` | `{id}` (201)  |

- **Algoritmos de grafos** (`pageRank`, `louvain`, `shortestPath`, … ver
  [GQ](../../docs/query-language.md#graph-algorithms)): `args` son los
  argumentos antes de la configuración (ids de nodos, `k`), `settings` el
  mapa de configuración (`{"labels": "Person", "weight": "km"}`). Las filas
  contienen los nodos como `{id, labels, name}`. Con `write: "rank"` (o
  `"@embedding"`) el resultado se guarda en cada nodo, desde `column` (por
  defecto la que sigue a `node`), y la respuesta es el número de nodos
  escritos.
- **Listados**: devuelven hasta `limit` elementos (100 por defecto, 1000
  como máximo) en orden de id; cuando hay `next`, pásalo como `after` para
  obtener la página siguiente.
- **nodo**: `{labels, props?, sources?, text?, embedding?}`. `PUT` y los
  upserts reemplazan el nodo entero; un `text` o `embedding` ausente se
  elimina, las aristas se conservan.
- **parche**: `{props?, remove_props?, add_labels?, remove_labels?, text?,
  embedding?, importance?}`. `props` se combina, y un `null` en él se guarda
  como valor (la clave se queda); `remove_props` lista las claves a quitar;
  `text: null` o `embedding: null` los eliminan.
- **arista**: `{from, label, to, props?, sources?, valid_from?, valid_to?}`.
- **filtro** (`filter`): `{labels?, valid_at?, known_at?}`. Sin `known_at`,
  las aristas retractadas se omiten.
- **consulta de recall**: `{text?, embedding?, limit?, labels?, candidates?,
  expand?, ranking?, include_archived?, touch?}`, donde `expand` es `true`
  (por defecto), `false`, o `{depth, direction, filter, decay, seeds, hubs,
  fanout}`, donde `hubs` (1 por defecto, 0 lo desactiva) atenúa los nodos
  según su número de aristas y `fanout` (100 por defecto, 0 para todos)
  limita los vecinos que se siguen desde cada nodo;
  `ranking` es `true` (por defecto: la importancia y la recencia cuentan),
  `false`, o `{importance_weight, recency_weight, half_life_days}`; los
  recuerdos archivados se omiten salvo con `include_archived`; `touch`
  registra un acceso a cada resultado.
- **memory** (en las respuestas de nodos): `{importance, accesses,
  last_access}`.

### Lotes

Un lote se ejecuta en una transacción: si una operación falla, no se
escribe nada y el mensaje de error empieza con `operation N:`. Las
operaciones pueden nombrar los nodos que crean con `ref` (`"$nombre"`) y las
operaciones siguientes pueden usar ese nombre donde aceptan un id de nodo.
Operaciones: `create_node`, `upsert_node` (`node.id` obligatorio),
`update_node` (`{id, patch}`), `delete_node`, `create_edge`, `delete_edge`,
`delete_edges_from` (`{from, label?}`), `invalidate_edge` (`{id, at}`) y
`retract_edge`.

Sincronizar una nota y reemplazar sus enlaces en una sola petición:

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

### Transacciones

`POST /v1/{ns}/transactions` inicia una transacción explícita; su id recibe
sentencias en `/v1/{ns}/transactions/{id}/query` hasta `commit` o
`rollback`. Solo quien la inició puede usarla.

- **De lectura** (`{"read_only": true}`): cada sentencia ve el snapshot de
  su inicio; las escrituras se rechazan.
- **De escritura**: retiene el escritor del servidor desde su inicio hasta
  su fin, así que las escrituras siguen siendo de una en una (aislamiento
  serializable). Las demás escrituras la esperan; una segunda transacción
  de escritura espera hasta `wait_secs` y luego recibe 409
  `transaction_busy`. Nada de lo que hace es visible para otros hasta que
  confirma, y entonces todo lo es, a la vez.
- Una sentencia que falla puede haber hecho parte de su trabajo, que no se
  puede deshacer por separado: la transacción queda entonces fallida, sus
  siguientes sentencias se rechazan, y `commit` la revierte (409
  `rolled_back`).
- Una transacción inactiva más tiempo que su timeout (`timeout_secs`, como
  máximo `idle_timeout_secs`), o con más antigüedad que `max_secs`, se
  revierte; su id da entonces 404. Una caída antes de `commit` no deja nada
  de ella.
- En un clúster se ejecuta en el líder (los demás miembros reenvían sus
  peticiones) en una transacción que captura lo que hace, y su `commit`
  replica todo como una sola entrada; si el líder cambia entretanto, nada
  de ella ocurrió (409). Con varios grupos, una transacción se queda en el
  grupo del namespace en el que empezó.
- Los usuarios, roles y permisos se administran fuera de las transacciones;
  `grafeno_transactions_open{kind}` en `/v1/metrics` cuenta las abiertas.

### La API como contrato

La API HTTP es la única entrada al motor: Grafeno Studio, el cliente de
TypeScript y cualquier otro programa la usan y nada más. Cada ruta está en
las tablas de arriba (una prueba comprueba que el servidor responde
exactamente estas). `GET /v1/about` (público) dice qué es el motor:

```json
{
  "product": "grafeno",
  "version": "0.2.1",
  "api": { "version": 1, "supported": [1] },
  "layout": 18,
  "capabilities": ["namespaces", "nodes", "…", "cluster.groups", "query.split"],
  "auth": { "token": true, "accounts": true, "open": false, "password_sign_in": true },
  "cluster": null
}
```

Cada respuesta bajo `/v1` lleva `x-grafeno-api: 1`. La versión de la API
cambia solo cuando un cambio rompería un cliente escrito para la anterior
(una ruta eliminada, un campo renombrado o con otro tipo); lo que se agrega
conserva la versión y aparece como una capacidad, así un cliente desactiva
lo que le falta a un motor más antiguo en lugar de fallar. Las capacidades:

| Capacidad | Qué es |
| --------- | ------ |
| `namespaces`, `nodes`, `edges`, `facts` | el grafo y sus hechos con tiempo de validez y de registro |
| `null_values` | una propiedad `null` se guarda como valor; `remove_props` quita claves (16.2) |
| `search.text`, `search.vector` | búsqueda de texto completo y vectorial |
| `algorithms.analysis`, `algorithms.simulation` | `algo.kCore`, `leiden`, `hits`, `eigenvector`, `harmonic`, `articulationPoints`, `bridges`, `spanningTree`, `maxFlow`, `bipartiteMatching`, `topologicalOrder`, `cycle`, `randomWalks`, `clustering`, `conductance`; `algo.distanceField`, `influence`, `floodFill`, `diffusion`, `automaton`, `assignment`, `minCostFlow` (16.8) |
| `vectors.health` | `GET /v1/{ns}/vectors/health`, `POST /v1/{ns}/compact?index=`, y grafos vectoriales compactados por su cuenta más allá de `compact_vectors_at` (#12) |
| `search.vector.indexes` | índices vectoriales con nombre (similitud, `m`, `ef`), búsquedas filtradas y de varios vectores, `/similar`, construcciones en segundo plano (16.7) |
| `search.fulltext`, `search.hybrid` | índices de texto completo con un lenguaje de consulta y fragmentos; `POST /v1/{ns}/search` con un modo (16.6) |
| `recall`, `context`, `memory` | recall híbrido, contexto para un prompt, importancia y accesos |
| `recall.hubs` | `hubs` y `fanout` en el `expand` del recall: nodos muy conectados atenuados, vecinos por nodo limitados |
| `transactions` | `BEGIN`, `COMMIT` y `ROLLBACK` a lo largo de varias peticiones (16.4) |
| `indexes.advanced` | índices sobre varias propiedades, sobre aristas y sobre varias etiquetas; restricciones `NOT NULL`, de tipo y de clave (16.5) |
| `query`, `query.explain`, `batches`, `traversal`, `algorithms` | GQ, `EXPLAIN`, lotes, k-hop, caminos, subgrafos y vecindarios, algoritmos de grafos |
| `query.trace` | `TRACE` y `trace: true` en `/query`: el recorrido de una sentencia, evento por evento (16.9) |
| `resolution`, `match-rules`, `extraction`, `constraints`, `communities`, `summaries`, `indexes` | mantener el conocimiento en forma (fase 11) |
| `changes`, `changes.stream`, `webhooks`, `ingest` | el registro de cambios, en páginas y como stream, webhooks, ingesta NDJSON |
| `events.values` | suscripciones de eventos (`CREATE EVENT SUBSCRIPTION`) cuyos eventos llevan valores, leídos en `/v1/{ns}/events/{name}` o enviados a un webhook; desactivadas salvo con `[events] enabled = true` (16.10) |
| `export`, `import`, `import.csv`, `import.graphml`, `backup` | mover datos hacia dentro y hacia fuera |
| `metrics`, `settings` | `GET /v1/metrics`, `GET /v1/settings` |
| `accounts`, `sessions`, `keys`, `roles`, `grants`, `policies`, `audit` | acceso (fase 14) |
| `cluster`, `cluster.groups`, `query.split` | clústeres (fase 13) |

### Clúster

En un miembro de un clúster (`[cluster]` en `grafeno.toml`, ver
[docs/instalacion.md](../../docs/instalacion.md)), cualquier miembro acepta
cualquier petición: un seguidor reenvía las escrituras (y las lecturas
linealizables) al líder, esperando una elección si no lo hay; sin la
dirección `api` del líder responde `not_leader` (421) nombrando al líder.
Cabeceras:

| Cabecera | Dirección | Significado |
| --- | --- | --- |
| `x-grafeno-position` | respuesta | la posición aplicada del miembro (un índice Raft, el mismo en cada miembro) |
| `x-grafeno-after: N` | petición | espera hasta 5 s a que el miembro aplique N (503 si no): leer tus propias escrituras |
| `x-grafeno-consistency` | petición | `local` (por defecto), `bounded` (al líder cuando el miembro está más de 1 000 entradas atrasado) o `linearizable` (a través del líder, con ReadIndex) |
| `x-grafeno-split` | petición | `auto` (por defecto: una lectura se reparte entre miembros cuando su costo estimado llega a `split_cost`), `on` (siempre que su forma lo permita) u `off`; una respuesta repartida tiene `split: {parts, here}` |
| `Idempotency-Key` | petición | en `POST /v1/{ns}/query` (o `idempotency_key` en su cuerpo): una escritura con la misma clave se ejecuta una vez, y los envíos posteriores devuelven su primer resultado |

Una lectura repartida pide a cada miembro su parte con
`POST /v1/{ns}/query/part` (interno: el miembro que reparte envía el plan y
la posición, y las credenciales de quien llama). Los webhooks y el trabajo
en segundo plano se ejecutan solo en el líder. Rutas de owner:

| Método | Ruta | Cuerpo | Devuelve |
| --- | --- | --- | --- |
| `GET` | `/v1/cluster` | | `{id, role, term, leader, commit, applied, last_index, snapshot_index, voters, outgoing}` |
| `POST` | `/v1/cluster/voters` | `{voters: [ids]}` | `{index}` (en el líder) |
| `POST` | `/v1/cluster/compact` | | `{snapshot_index}` |
| `GET` | `/v1/cluster/groups` | | cada grupo Raft: `{id, voters, held, status}` (13.5) |
| `GET` | `/v1/cluster/placement` | | `[{namespace, group, moving_to}]`: los namespaces colocados fuera del grupo 0 |
| `POST` | `/v1/cluster/placement` | `{namespace, group}` | lo mueve sin detenerlo: `{from, to, records, catch_up_rounds, paused_ms, …}` |
| `GET` | `/v1/cluster/groups/{group}/status` | | la vista de un grupo, como `/v1/cluster` |
| `POST` | `/v1/cluster/groups/{group}/voters` | `{voters: [ids]}` | cambia los votantes de ese grupo |
| `POST` | `/v1/cluster/groups/{group}/compact` | | toma un snapshot de la base de ese grupo |

Con varios grupos, una petición para un namespace va a su grupo (en
cualquier miembro); las escrituras durante la pausa de un movimiento
responden `busy` (503) y deben reintentarse. Rutas que los miembros usan
entre sí (con el secreto del clúster, no para clientes):

| Método | Ruta | Cuerpo | Devuelve |
| --- | --- | --- | --- |
| `POST` | `/v1/cluster/audit` | eventos de auditoría que vio un seguidor | registrados por el líder |
| `POST` | `/v1/cluster/groups/{group}/import` | las copias de registros de un movimiento | `{records}` |
| `POST` | `/v1/cluster/groups/{group}/fence` | el namespace que se mueve | bloquea sus escrituras |
| `POST` | `/v1/cluster/groups/{group}/directory` | el grupo nuevo de un namespace | actualiza el directorio |

`grafeno cluster status|voters|compact --url … --token … [--group N]` las
llaman; `/v1/cluster/groups/{id}/status`, `…/voters` y `…/compact` hacen lo
mismo para otro grupo. El estado (13.6) también cuenta elecciones, cambios
de líder y snapshots enviados e instalados, da el formato de entrada que
escribe el grupo y qué ejecuta cada miembro (`members`), y en el líder el
atraso de cada otro miembro (`peers`); `/v1/metrics` los tiene como series
`grafeno_cluster_*` por grupo. `GET /v1/backup?group=N` copia el archivo de
otro grupo. Ver [docs/cluster-runbooks.md](../../docs/cluster-runbooks.md)
(en inglés).

## MCP

`grafeno mcp` habla MCP (versiones del protocolo 2025-06-18, 2025-03-26 y
2024-11-05) sobre stdin y stdout, y escribe sus registros en stderr. Sus
herramientas:

| Herramienta | Qué hace                                                                       |
| ----------- | ------------------------------------------------------------------------------ |
| `remember` | Guarda un recuerdo (texto, nombre opcional, etiquetas, importancia y enlaces). Un `name` conocido actualiza esa entidad. |
| `recall`   | Búsqueda híbrida; devuelve ids, nombres, fragmentos y por qué coincidió cada resultado. Registra accesos. |
| `context`  | Recuerdos y hechos relevantes como texto compacto dentro de un presupuesto de tokens, después de los resúmenes de comunidades que coinciden. |
| `relate`   | Registra un hecho (arista) entre dos nodos, con fechas de validez opcionales; indica qué hechos contradijo bajo una restricción (y terminó). |
| `update`   | Reemplaza el texto de un nodo, lo renombra, combina propiedades (un `null` se guarda), quita propiedades por nombre (`remove_props`), cambia etiquetas o importancia. |
| `forget`   | Invalida (por defecto), retracta o elimina un hecho; elimina un nodo.         |
| `get_node` | Un nodo con su texto, propiedades y los hechos que lo conectan.               |
| `query`    | Una sentencia [GQ](../../docs/query-language.md) de solo lectura; 200 filas como máximo. |
| `merge`    | Fusiona un nodo duplicado en otro; se rechaza cuando discrepan salvo con `force`. Devuelve un `merge_id`. |
| `undo_merge` | Deshace una fusión por su `merge_id`: el nodo fusionado y sus hechos vuelven. |
| `find_matches` | Antes de crear una entidad, los nodos que ya podrían serla, según la regla de coincidencia de la etiqueta, con una decisión y la similitud de cada campo. |
| `extract`  | Lee los recuerdos nuevos y modificados y enlaza lo que mencionan (nombres, correos, URLs, usuarios, hashtags, fechas, montos, nombres nuevos recurrentes). |
| `add_mentions` | Las entidades que menciona un recuerdo, según la propia lectura del agente: enlazadas, creadas o en cola. |
| `review` / `resolve_review` | Lo que la extracción no pudo decidir, y resolverlo (enlazar, crear, descartar). |
| `maintain` | Lista los posibles duplicados, los recuerdos débiles y los grupos listos para consolidar. |
| `consolidate` | Guarda un resumen de varios recuerdos y los archiva.                        |
| `overview` | Para preguntas amplias: los resúmenes de comunidades que coinciden con un tema, o los principales; con `id`, las partes y miembros de una comunidad. |
| `summarize_communities` | Encuentra las comunidades (y comunidades de comunidades) y escribe o actualiza sus resúmenes. |
| `set_summary` | Reemplaza el texto de un resumen de comunidad por el del propio agente (o lo deshace). |
| `changes`  | Lo escrito desde un número de secuencia: nodos y hechos creados, actualizados y eliminados. |

Cada herramienta acepta un `namespace` opcional, que por defecto es
`--namespace`.

El servidor MCP abre el archivo de la base por sí mismo, así que no puede
dejar fuera al usuario que lo inicia, pero sí limita al agente al que
atiende:

| Flag | Efecto |
| --- | --- |
| `--allow vault,team-*` | los namespaces que el agente puede usar (todos por defecto) |
| `--read-only` | solo se ofrecen las herramientas que leen (y recall no registra accesos) |
| `--as NAME` | sus escrituras se registran como `NAME` (`@recorded_by`) |
| `--own-only` | con `--as`: solo puede cambiar lo que registró |

La configuración de un cliente MCP (el archivo y su ubicación dependen del
cliente):

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
