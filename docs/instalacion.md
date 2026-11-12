# Instalar Grafeno

**Español** | [English](install.md)

Hay dos familias de instaladores:

- **Grafeno Desktop**, la aplicación de escritorio: Grafeno Studio en su propia
  ventana, que abre archivos de base de datos con el motor dentro de la
  aplicación (sin servicio ni puerto) o se conecta a un servidor Grafeno. Sirve
  para usar Grafeno en tu propio equipo.
- **El servidor Grafeno**: el programa `grafeno` como servicio, para agentes y
  aplicaciones que necesitan Grafeno en marcha todo el tiempo (y para equipos
  sin escritorio).

Ambos pueden instalarse a la vez. Un archivo de base de datos solo puede estar
abierto en uno de ellos a la vez.

## Grafeno Desktop

| Sistema | Archivo |
| ------- | ------- |
| Windows 10/11 (x64) | `grafeno-desktop-<version>-windows-x64-setup.exe` (para tu usuario, sin administrador) o `grafeno-desktop-<version>-windows-x64.msi` |
| Debian, Ubuntu (x86_64) | `grafeno-desktop_<version>_amd64.deb` |
| Fedora, RHEL, openSUSE (x86_64) | `grafeno-desktop-<version>-1.x86_64.rpm` |
| Cualquier Linux (x86_64) | `grafeno-desktop-<version>-x86_64.AppImage` (`chmod +x` y luego ejecútalo) |

Los instaladores añaden **Grafeno Desktop** al menú Inicio o al menú de
aplicaciones, con el icono de Grafeno, y hacen que los archivos `.redb` se abran
con él. En Windows la ventana usa WebView2, que ya forma parte de Windows 10 y
11 (el instalador lo añade si falta); en Linux los paquetes dependen de
WebKitGTK, que instala el gestor de paquetes.

Al iniciarse por primera vez muestra la pantalla de inicio: **New database…**
crea un archivo, **Open…** abre uno, se listan las bases recientes y **A Grafeno
server** se conecta al servicio instalado (`http://127.0.0.1:7810` con el token
de `grafeno config token`) o a otro equipo. Más detalles en
[apps/desktop/README.es.md](../apps/desktop/README.es.md).

## El servidor Grafeno

Todas las formas de instalar el servidor dan el mismo programa `grafeno`: el
servidor HTTP con Grafeno Studio (la interfaz web) dentro, el servidor MCP, la
shell y los comandos de administración. Los instaladores además lo ejecutan como
un servicio que arranca con el sistema.

| Sistema | Archivo | Qué configura |
| ------- | ------- | ------------- |
| Windows 10/11, Server 2019+ (x64) | `grafeno-<version>-windows-x64-setup.exe` | `C:\Program Files\Grafeno`, el servicio `Grafeno`, el PATH y accesos directos en el menú Inicio |
| Debian, Ubuntu y derivados (x86_64) | `grafeno_<version>-1_amd64.deb` | `/usr/bin/grafeno`, el servicio systemd `grafeno` y su usuario |
| Fedora, RHEL, Rocky, Alma, openSUSE (x86_64) | `grafeno-<version>-1.x86_64.rpm` | lo mismo |
| Cualquier Linux (x86_64) | `grafeno-x86_64-unknown-linux-musl.tar.gz` o `install.sh` | el binario; el servicio con `--service` |
| Contenedores | `Dockerfile` | una imagen que sirve en el puerto 7810, con los datos en `/data` |

El binario de Linux es estático (musl): el mismo archivo funciona en cualquier
distribución, sea cual sea su versión de glibc.

Después de instalar, abre **Grafeno Studio** en <http://127.0.0.1:7810/studio/>.
Te pedirá el token del servidor, que generó el instalador:

```bash
grafeno config token
```

(En Linux, como root: `sudo grafeno config token`.)

### Windows

Ejecuta `grafeno-<version>-windows-x64-setup.exe` (pide permisos de
administrador). Permite elegir:

- **Run Grafeno as a Windows service**: el servicio `Grafeno` arranca con
  Windows y sirve en `127.0.0.1:7810`.
- **Add grafeno to the system PATH**: para ejecutar `grafeno` desde cualquier
  terminal (abre una nueva después de instalar).

Crea `C:\ProgramData\Grafeno\grafeno.toml` (dirección, token, Studio) y guarda
la base de datos en `C:\ProgramData\Grafeno\data`. El menú Inicio recibe
*Grafeno Studio*, *Grafeno Shell* (conectada al servicio) y *Grafeno settings*.
El servicio escribe su registro en `C:\ProgramData\Grafeno\grafeno.log`.

Gestiona el servicio como cualquier otro (`services.msc`, o PowerShell como
administrador):

```powershell
Restart-Service Grafeno
```

Para actualizar, ejecuta el nuevo instalador sobre el anterior: detiene el
servicio, reemplaza el programa y conserva la configuración y los datos. La
desinstalación (Configuración → Aplicaciones) elimina el programa, el servicio y
la entrada del PATH, y conserva `C:\ProgramData\Grafeno`.

Sin el instalador, `grafeno-x86_64-pc-windows-msvc.zip` contiene solo
`grafeno.exe`; como administrador, el servicio puede configurarse a mano:

```powershell
grafeno config init
```

```powershell
grafeno service install
```

### Debian y Ubuntu

```bash
sudo apt install ./grafeno_0.1.0-1_amd64.deb
```

### Fedora, RHEL y derivados

```bash
sudo dnf install ./grafeno-0.1.0-1.x86_64.rpm
```

Fedora 45 y posteriores rechazan paquetes sin firmar; mientras las versiones no
estén firmadas, añade `--nogpgcheck` a `dnf` (o `--nosignature` a `rpm -i`).

#### Qué hacen los paquetes

- Crean el usuario de sistema `grafeno` y `/var/lib/grafeno` (sus datos).
- Crean `/etc/grafeno/grafeno.toml` con un token nuevo, legible solo por root y
  el grupo `grafeno`. Las actualizaciones lo conservan.
- Instalan e inician el servicio systemd `grafeno`, aislado para que solo pueda
  escribir en su carpeta de datos (`systemd-analyze security grafeno` le da 1.3,
  "OK").
- Añaden **Grafeno Studio** al menú de aplicaciones, con el icono de Grafeno:
  abre el Studio del servicio en el navegador.

```bash
systemctl status grafeno
```

```bash
journalctl -u grafeno -f
```

Al quitar el paquete se detiene el servicio y se conservan `/var/lib/grafeno` y
`/etc/grafeno`, incluso con purge: pueden ser la única copia de la memoria de
alguien.

### Cualquier Linux: install.sh

```bash
curl -fsSL https://raw.githubusercontent.com/Seerendo/grafeno/main/install.sh | sh
```

Descarga la última versión para tu arquitectura, comprueba su SHA-256 e instala
`grafeno` en `~/.local/bin` (o en `/usr/local/bin` como root). Con `--service`
(como root y con systemd) también configura el servicio igual que los paquetes:

```bash
curl -fsSL https://raw.githubusercontent.com/Seerendo/grafeno/main/install.sh | sudo sh -s -- --service
```

Otras opciones: `--version v0.1.0`, `--prefix DIR`. `GRAFENO_BASE_URL` lo dirige
a un espejo.

### Docker

```bash
docker build -t grafeno .
```

```bash
docker run -d -p 7810:7810 -v grafeno-data:/data -e GRAFENO_TOKEN=change-me grafeno
```

## Un clúster de servidores

Tres (o cinco) servidores pueden mantener los mismos datos y seguir respondiendo
cuando uno falla (fases 13.2 y 13.3). Cada uno recibe la misma sección
`[cluster]` en su `grafeno.toml`, con su propio `id` y `addr`:

```toml
[cluster]
id = 1                         # 2 and 3 on the others
addr = "10.0.0.1:7811"         # where this member listens for the others
secret = "a long random string, the same on every member"
voters = [1, 2, 3]             # the first members; [] on a member that joins later
peers = [                      # addr: the members' port; api: the HTTP API, for forwarding
  { id = 1, addr = "10.0.0.1:7811", api = "10.0.0.1:7810" },
  { id = 2, addr = "10.0.0.2:7811", api = "10.0.0.2:7810" },
  { id = 3, addr = "10.0.0.3:7811", api = "10.0.0.3:7810" },
]
```

Inicia los tres: eligen un líder en más o menos un segundo. Envía las
solicitudes a cualquier miembro: un seguidor reenvía las escrituras al líder
(con `api` en `peers`; sin él, responde 421 `not_leader` e indica el líder), y
las lecturas las atiende el propio miembro. Una lectura puede pedir más con la
cabecera `x-grafeno-consistency`: `bounded` va al líder cuando el miembro está
muy atrasado y `linearizable` ve toda escritura confirmada antes que ella. Cada
respuesta lleva `x-grafeno-position`; una solicitud con `x-grafeno-after: N`
espera hasta que su miembro haya aplicado N (leer tus propias escrituras). Una
escritura GQ enviada con la cabecera `Idempotency-Key` se ejecuta una sola vez,
por muchas veces que se reintente. La shell (`--url` con varias direcciones
separadas por comas) y el cliente TypeScript (`url: [...]`) pasan a otro
miembro cuando uno falla y leen sus propias escrituras. Cuando el líder cae,
las escrituras se reanudan en alrededor de medio segundo.

Una lectura costosa (coste estimado por encima de `split_cost` en `[cluster]`,
200 000 por defecto, tal como lo muestra `EXPLAIN`) se reparte entre los
miembros: cada uno ejecuta una parte en la misma posición y el miembro
consultado une las partes en las mismas filas que devolvería un solo servidor.
`x-grafeno-split: on` reparte una lectura siempre que su forma lo permita, y
`off` nunca. Los miembros que puedan recibir partes necesitan `api` en `peers`.

Las escrituras de un grupo pasan por un solo líder a la vez. Para escribir más,
ejecuta más grupos, cada uno con su propio líder y su archivo de base de datos
(`<db>.group-<id>.redb`) en sus votantes:

```toml
[cluster]
# … as above, then:
groups = [{ id = 1, voters = [1, 2, 3] }, { id = 2, voters = [1, 2, 3] }]
```

Los espacios de nombres viven en el grupo 0 hasta que se colocan en otro:

```bash
curl -X POST http://10.0.0.1:7810/v1/cluster/placement -H "Authorization: Bearer …" -H 'content-type: application/json' -d '{"namespace": "vault", "group": 1}'
```

Un espacio de nombres que ya tiene datos se mueve sin dejar de responder: sus
escrituras se pausan unas decenas de milisegundos (responden *busy* y los
clientes reintentan) y no se pierde ninguna. Cualquier miembro acepta
solicitudes para cualquier espacio de nombres. Respalda el archivo de cada grupo
(`GET /v1/backup?group=1`; los respaldos pueden hacerse en un seguidor).
`grafeno cluster status --url http://10.0.0.1:7810 --token …` (`--group 1` para
otro grupo) muestra la vista de un miembro: elecciones, cambios de líder,
instantáneas, el formato de entradas que escribe el grupo, lo que ejecuta cada
miembro y, en el líder, el retraso de cada miembro. `/v1/metrics` ofrece lo
mismo como series `grafeno_cluster_*`, y Grafeno Studio una vista Cluster.
`grafeno cluster voters 1 2 3 4` (en el líder) cambia los miembros; el nuevo
debe figurar en los `peers` de todos y arrancar con `voters = []`.
`grafeno cluster compact` toma una instantánea de la base de un miembro y
recorta su registro. Cada miembro guarda su estado Raft en `<db>.raft` y sus
instantáneas en `<db>.snapshots/`.

Un clúster de tres miembros con compose, configuraciones de servicio para
systemd y Windows, y los procedimientos para perder un miembro, perder la
mayoría, reemplazar un disco y actualizar están en
[cluster-runbooks.md](cluster-runbooks.md).

Por ahora: el tráfico entre miembros se autentica con el secreto, pero no se
cifra (mantenlos en una red privada); las importaciones se hacen en un solo
servidor antes de que inicie un clúster. Los webhooks y el trabajo en segundo
plano (extracción, resúmenes) se ejecutan en el líder, y un nuevo líder los
retoma.

## Cuentas

Un servidor nuevo está abierto a cualquiera que llegue a él (aceptable en tu
propio equipo) hasta que tenga cuentas o un token en `grafeno.toml`. Crea la
primera cuenta, un superusuario, con el servidor detenido o a través de él:

```bash
grafeno users create admin --superuser --config /etc/grafeno/grafeno.toml
```

(En un servidor abierto, Grafeno Studio también ofrece crearla.) Después crea
las cuentas de las personas que inician sesión, con permisos por espacio de
nombres, y cuentas de servicio para agentes y aplicaciones, que usan claves de
API:

```bash
grafeno users create ana --grant notes:write --config /etc/grafeno/grafeno.toml
```

```bash
grafeno users create indexer --service --grant "team-*:write" --config /etc/grafeno/grafeno.toml
```

Los usuarios inician sesión desde Studio, la shell (`grafeno shell --url …
--user ana`) o la API (`POST /v1/auth/signin`), y pueden crear sus propias
claves de API, limitadas a algunos espacios de nombres o a solo lectura.
`grafeno users password`, `disable`, `enable` y `delete` las gestionan. Las
claves anteriores a 14.1 siguen funcionando: cada una pasó a ser una cuenta de
servicio con su nombre. El token de `grafeno.toml`, cuando existe, sigue
pudiendo hacerlo todo: guárdalo para emergencias. `[auth]` (abajo) define cuánto
duran las sesiones y cuándo los inicios de sesión fallidos bloquean una cuenta.

Los roles (14.2) agrupan privilegios: define uno con `PUT /v1/roles/{name}`
(por ejemplo, leer todos los espacios `team-*` menos `team-secret`, o ver solo
métricas, para un agente de monitorización) y asígnalo a cuentas con
`PATCH /v1/accounts/{name}` y `{"roles": [...]}`. Un rol llamado `public` es de
todos. El README del servidor lista los privilegios.

Lo mismo en GQ (14.3), desde la shell o la vista Scripts de Studio, con una
cuenta que pueda gestionar roles:

```cypher
CREATE ROLE team_reader;
GRANT read, export ON NAMESPACE 'team-*' TO team_reader;
DENY read ON NAMESPACE 'team-secret' TO team_reader;
GRANT ROLE team_reader TO ana;
```

o desde la línea de comandos (`grafeno grants grant read --on namespace:team-* --to team_reader`,
`grafeno roles show team_reader`), y en la vista Access de Studio.

## Respaldos

Un respaldo es una copia coherente de todo el archivo de base de datos (todos
los espacios de nombres y las claves de API), tomada mientras el servicio sigue
en marcha:

```bash
grafeno backup grafeno-backup.redb --config /etc/grafeno/grafeno.toml
```

(En Windows, la configuración está en `C:\ProgramData\Grafeno\grafeno.toml`;
`grafeno config path` muestra dónde.) Para restaurar, detén el servicio, coloca
la copia donde apunta `db` en `grafeno.toml` e inícialo de nuevo. Antes de usar
un archivo que podría estar dañado (un disco que falla, una copia defectuosa),
compruébalo con el servicio detenido:

```bash
grafeno check --db grafeno-backup.redb
```

Verifica cada página contra su suma de comprobación y después los índices. Las
exportaciones (`grafeno export`) son el otro tipo de copia: JSON Lines, legibles
y portables entre versiones, pero sin las claves.

Para llevar un espacio de nombres a otras herramientas o traerlo desde ellas
(hojas de cálculo, Gephi, yEd, NetworkX, APOC), usa CSV o GraphML. Contienen el
conocimiento actual, sin hechos retractados ni el historial que hay detrás, así
que no reemplazan un respaldo:

```bash
grafeno export --format csv -n notes -o notes-csv
```

```bash
grafeno import --format csv -n notes notes-csv/nodes.csv --edges notes-csv/edges.csv
```

```bash
grafeno export --format graphml -n notes -o notes.graphml
```

```bash
grafeno import --format graphml -n notes notes.graphml
```

## Configuración

`grafeno.toml` lo leen el servicio, `grafeno serve --config` y
`grafeno shell --config`. Reinicia el servicio después de cambiarlo.

```toml
db = "/var/lib/grafeno/grafeno.redb"
addr = "127.0.0.1:7810"     # "0.0.0.0:7810" listens on every network interface
token = "…"                 # every /v1 request needs `Authorization: Bearer <token>`
studio = true               # serve Grafeno Studio at /studio/
cors_origins = []           # origins allowed to call the API from a browser ("*": any)
durability = "immediate"    # or "eventual": faster writes, a crash loses at most 0.5 s
max_requests = 512          # served at once; more get 503 with Retry-After

[query]                     # limits for each GQ query over HTTP and MCP; 0 turns one off
timeout_secs = 30
max_reads = 50000000        # nodes, edges and index entries read
max_rows = 1000000          # rows held at once
max_memory_mb = 512         # one value a query builds, or the rows it holds at once
max_cost = 0                # refuse queries EXPLAIN estimates above this (0: off)
scan_threads = 0            # threads large read scans share, over every query (0: one per CPU; 1: off)

[writer]                    # the group commit (see "Monitoring" below)
max_batch = 512             # writes per transaction at most
queue = 4096                # writes that may wait; more get 503
flush_ms = 500              # with eventual durability, how often commits reach the disk
import_chunk = 2000         # records per transaction of an import with ?bulk=true

[memory]                    # caches, in MiB (see "Memory" below)
cache_mb = 256              # pages of the database file
projection_cache_mb = 256   # graphs kept for graph algorithms
vector_cache_mb = 128       # vectors decoded while inserting embeddings

[changes]                   # the change log that streams and webhooks follow
enabled = true
max_entries = 1000000       # per namespace; 0 for no limit
max_age_days = 7            # 0 for no limit

[auth]                      # accounts and sign-in
session_minutes = 15        # an access token's life (sessions renew it)
refresh_days = 30           # a session's life
lockout_attempts = 5        # failed sign-ins that lock an account for a while
address_attempts = 20       # failed sign-ins that refuse an address for a while
lockout_minutes = 15

[background]                # work that follows the change log
extract = ["notes"]         # link what new and changed texts mention
communities = ["notes"]     # keep community summaries fresh
communities_every_secs = 300
communities_min_changes = 50
```

Los comentarios del ejemplo siguen en inglés para que coincida con la
[versión en inglés](install.md#settings).

Las opciones de línea de comandos y las variables de entorno (`GRAFENO_DB`,
`GRAFENO_ADDR`, `GRAFENO_TOKEN`) tienen prioridad sobre el archivo. Expón el
puerto más allá de `127.0.0.1` solo detrás de TLS (un proxy inverso) y con un
token.

### Memoria

Las cachés de Grafeno son la principal memoria que retiene: páginas del archivo
de base de datos (`cache_mb`), grafos proyectados para algoritmos
(`projection_cache_mb`) y vectores decodificados al insertar
(`vector_cache_mb`). Todo lo demás es por solicitud y está acotado por los
límites de `[query]`. Con 10 millones de aristas, un servidor se mantiene cerca
de 300 MiB con los valores por defecto; en un equipo de 512 MiB usa
`cache_mb = 128` y `projection_cache_mb = 64` (los algoritmos de grafos sobre
un grafo así construyen entonces su proyección, de unos 140 MiB, cada vez, y
pueden necesitar un `timeout_secs` mayor). Cargar muchos embeddings va unas dos
veces más rápido con `vector_cache_mb = 1024` y transacciones de escritura
grandes, si hay memoria disponible. Las cifras están en los
[benchmarks](benchmarks.md).

### Monitorización

`GET /v1/metrics` responde en el formato de texto de Prometheus: solicitudes y
su duración, consultas GQ, los límites que alcanzan, la cola del escritor, los
grupos, el tiempo de confirmación y la latencia de escritura por carril.
Necesita un secreto, como el resto de `/v1`; sirve una clave con cualquier
permiso:

```yaml
scrape_configs:
  - job_name: grafeno
    metrics_path: /v1/metrics
    authorization:
      credentials: gfk_…
    static_configs:
      - targets: ["127.0.0.1:7810"]
```

Vigila `grafeno_write_queue` y `grafeno_write_seconds`: una cola que se mantiene
alta significa que las escrituras llegan más rápido de lo que un escritor las
confirma (unas 11 000 por segundo en un SSD con durabilidad inmediata, más con
`durability = "eventual"`); `grafeno_write_rejected_total` cuenta los 503 que
superan el límite de la cola. Carga las exportaciones grandes con `?bulk=true`:
así entran en bloques entre las escrituras de los agentes en lugar de antes que
ellas.

## Grafeno Studio por separado

Studio es una aplicación web estática. Además de la copia incluida en
`grafeno`, su compilación (`studio/dist`) puede alojarse en cualquier sitio y
apuntar a un servidor, que entonces debe permitir ese origen:

```bash
grafeno serve --config grafeno.toml --cors-origin https://studio.example.com
```

`grafeno serve --no-studio` desactiva la copia incluida.

## Compilar los instaladores

| Archivos | Comando | Requiere |
| -------- | ------- | -------- |
| Linux `.tar.gz`, `.deb`, `.rpm` | `docker build -f packaging/linux/Dockerfile --output dist/linux .` | Docker |
| Instalador `.exe` de Windows | `cd studio && npm ci && npm run build`, luego `cargo build --release -p grafeno-server`, luego `iscc /DAppVersion=0.1.0 packaging\windows\grafeno.iss` | Node 22+, Rust (MSVC), Inno Setup 6 |
| Grafeno Desktop (los instaladores del sistema donde se ejecuta: `.exe` y `.msi`, o `.deb`, `.rpm` y `.AppImage`) | `npm ci --prefix studio`, `npm ci --prefix apps/desktop`, luego `cd apps/desktop && npx tauri build` | Node 22+, Rust; en Linux, WebKitGTK (ver [apps/desktop/README.es.md](../apps/desktop/README.es.md)) |

Todos los iconos salen de `docs/assets/logo.svg`: `npm run icons` en
`apps/desktop` genera los iconos de la aplicación de escritorio,
`packaging/windows/grafeno.ico` (el ejecutable del servidor, su instalador y sus
accesos directos) y `packaging/linux/icons/` (la entrada de menú de los paquetes
del servidor). `.github/workflows/release.yml` los compila todos y publica una
versión en GitHub cuando se sube una etiqueta `v*`.
