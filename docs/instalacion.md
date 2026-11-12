# Instalar Grafeno

**Español** | [English](install.md)

Esta guía instala **el motor**: el programa `grafeno`, como un servicio que
arranca con el sistema. No tiene interfaz gráfica, como no la tiene un
servidor de bases de datos: se trabaja con él desde su consola
(`grafeno shell`), su API HTTP, MCP o cualquier cliente. **Grafeno Studio**,
la interfaz gráfica, es un producto aparte con sus propios instaladores, que
se conecta a los motores mediante su API.

## El servidor Grafeno

Cualquier forma de instalar el servidor da el mismo programa `grafeno`: el
servidor HTTP, el servidor MCP, la consola y los comandos de administración,
y nada más. Los instaladores además lo ejecutan como un servicio que arranca
con el sistema.

| Sistema | Archivo | Qué deja preparado |
| ------- | ------- | ------------------ |
| Windows 10/11, Server 2019+ (x64) | `grafeno-<version>-windows-x64-setup.exe`, o `winget install GrafenoDB.Grafeno` | `C:\Program Files\Grafeno`, el servicio `Grafeno`, el PATH, accesos en el menú Inicio |
| Debian, Ubuntu y derivados (amd64, arm64) | El repositorio APT de Grafeno, o `grafeno_<version>-1_<arch>.deb` | `/usr/bin/grafeno`, el usuario `grafeno`, la instancia por defecto y su servicio systemd |
| Fedora, RHEL, Rocky, Alma, Oracle Linux, Amazon Linux (x86_64, aarch64) | El repositorio DNF de Grafeno, o `grafeno-<version>-1.<arch>.rpm` | lo mismo |
| Cualquier Linux (x86_64, aarch64) | `grafeno-<arch>-unknown-linux-musl.tar.gz`, o `install.sh` | el binario; el servicio con `--service` |

El binario de Linux es estático (musl): el mismo archivo funciona en
cualquier distribución, sea cual sea su versión de glibc.

Después de instalarlo, el motor atiende en `127.0.0.1:7810`. Su consola, un
cliente o Grafeno Studio se conectan con el token que generó el instalador:

```bash
grafeno config token
```

(En Linux, como root: `sudo grafeno config token`.) `grafeno doctor` revisa
la instalación: la configuración y sus permisos, la carpeta de datos, la
versión de formato de la base, el espacio libre en disco, el puerto y el
servicio.

### El instalador de una línea: cualquier Linux, AlmaLinux y RHEL incluidos

```bash
curl -fsSL https://raw.githubusercontent.com/grafenodb/grafeno/main/install.sh | sudo sh
```

Como root, reconoce la distribución en `/etc/os-release` y el procesador
(x86_64 o aarch64), agrega el repositorio firmado de Grafeno e instala el
paquete con el gestor de paquetes del sistema, así las actualizaciones llegan
con las del sistema (`dnf upgrade`, `apt upgrade`). El resultado es el mismo
que instalar el paquete a mano: el usuario `grafeno`, la instancia por
defecto y el servicio systemd, en marcha.

| Distribución | Qué instala |
| --- | --- |
| RHEL, AlmaLinux, Rocky, Oracle Linux, CentOS Stream, Fedora, Amazon Linux | el repositorio DNF y el `.rpm`, con `dnf` (o `yum`) |
| Debian, Ubuntu y sus derivados | el repositorio APT y el `.deb`, con `apt` |
| Cualquier otra | el binario estático en `/usr/local/bin` (agrega `--service` para el servicio) |

Si no puede llegar al repositorio (o con `--no-repo`), instala en su lugar
el archivo del paquete del release. Sin root (sin `sudo`), o con `--binary`,
instala el binario en `~/.local/bin`; `--service` agrega entonces un servicio
systemd **de usuario**, con su configuración en
`~/.config/grafeno/grafeno.toml` y sus datos en `~/.local/share/grafeno`:

```bash
curl -fsSL https://raw.githubusercontent.com/grafenodb/grafeno/main/install.sh | sh -s -- --service
```

Cada archivo que descarga se comprueba contra el SHA-256 publicado con el
release, y contra su firma minisign cuando se conoce una clave
(`GRAFENO_MINISIGN_KEY`, con `minisign` instalado); si no coinciden, se
detiene. En sistemas con SELinux etiqueta el binario que instala para que
systemd pueda ejecutarlo. Opciones, como flags o variables de entorno (para
instalaciones desatendidas): `--version 0.2.1` (`GRAFENO_VERSION`), `--binary`
(`GRAFENO_METHOD=binary`), `--no-repo` (`GRAFENO_REPO=0`), `--prefix DIR`
(`GRAFENO_PREFIX`), `--service`, `--no-start`; `GRAFENO_BASE_URL` y
`GRAFENO_REPO_URL` lo apuntan a espejos.

### Debian y Ubuntu

Desde el repositorio (lo que hace el instalador de una línea):

```bash
sudo install -d /etc/apt/keyrings && curl -fsSL https://grafenodb.github.io/grafeno/grafeno.asc | sudo tee /etc/apt/keyrings/grafeno.asc >/dev/null
```

```bash
echo "deb [signed-by=/etc/apt/keyrings/grafeno.asc] https://grafenodb.github.io/grafeno/deb stable main" | sudo tee /etc/apt/sources.list.d/grafeno.list
```

```bash
sudo apt update && sudo apt install grafeno
```

O un paquete descargado:

```bash
sudo apt install ./grafeno_0.2.1-1_amd64.deb
```

### Fedora, RHEL y derivados

Desde el repositorio (lo que hace el instalador de una línea):

```bash
sudo curl -fsSL https://grafenodb.github.io/grafeno/grafeno.repo -o /etc/yum.repos.d/grafeno.repo
```

```bash
sudo dnf install grafeno
```

Los paquetes y los metadatos del repositorio están firmados; `dnf` pide
confiar en la clave la primera vez (`gpgkey` en `grafeno.repo`). Un paquete
descargado no está firmado (la copia del repositorio sí): instálalo con
`--nogpgcheck` después de comprobar su SHA-256:

```bash
sudo dnf install --nogpgcheck ./grafeno-0.2.1-1.x86_64.rpm
```

#### Qué hacen los paquetes

- Crean el usuario de sistema `grafeno` y `/var/lib/grafeno` (sus datos).
- En una primera instalación, crean la **instancia por defecto**
  (`grafeno setup`): `/etc/grafeno/grafeno.toml` con un token nuevo, legible
  solo por root y el grupo `grafeno`, y el servicio systemd `grafeno`,
  habilitado y en marcha. Las actualizaciones conservan la configuración y
  reinician cada instancia en marcha.
- Ejecutan el servicio aislado, de modo que puede escribir su carpeta de
  datos y nada más (`systemd-analyze security grafeno` le da 1.3, "OK"), y
  confinado: el `.deb` carga un perfil de **AppArmor** (aplicado); el `.rpm`
  instala un módulo de **SELinux** (el dominio `grafeno_t` y las etiquetas
  del programa, la configuración, los datos y los puertos 7810-7819; por
  ahora el dominio es permisivo: registra y no deniega nada).
- No instalan nada gráfico: ni entrada de menú, ni icono, ni dependencias de
  un entorno de escritorio.

```bash
systemctl status grafeno
```

```bash
journalctl -u grafeno -f
```

Quitar el paquete detiene todas las instancias y conserva `/etc/grafeno` y
`/var/lib/grafeno`. Purgar el `.deb` (`apt purge grafeno`) los elimina, como
espera Debian: haz antes un `grafeno backup` si los datos importan. Con el
`.rpm` (que no tiene purga), `grafeno instances drop <nombre> --purge`
elimina los datos de una instancia antes de quitar el paquete.

### Instancias: `grafeno setup`

Una instancia es un archivo de configuración, una carpeta de datos y un
servicio: un servidor de bases de datos entre otros en la misma máquina. Los
paquetes crean la instancia por defecto; `grafeno setup` crea otras, y
pregunta lo que sus flags dejan abierto:

```bash
sudo grafeno setup --instance team
```

Pregunta la carpeta de datos (`/var/lib/grafeno/team`), el puerto (el
siguiente libre después del 7810), si otros equipos pueden conectarse, y el
primer administrador (un superusuario que inicia sesión con contraseña);
luego escribe `/etc/grafeno/team.toml` (con un token nuevo), crea la base y
el administrador, y habilita e inicia `grafeno@team.service`. En modo
desatendido, cada respuesta es un flag, o una línea de un archivo de
respuestas:

```bash
sudo grafeno setup --unattended --instance team --data /srv/grafeno-team --port 7820 --network --admin root --admin-password-file /root/pw
```

```toml
# answers.toml, for grafeno setup --answers answers.toml --unattended
instance = "team"
data = "/srv/grafeno-team"
port = 7820
network = true
durability = "eventual"
memory_mb = 1024
admin = "root"
admin_password_file = "/root/pw"
```

Una carpeta de datos fuera de `/var/lib/grafeno` se agrega a los
`ReadWritePaths` del servicio (un drop-in), al perfil de AppArmor
(`local/grafeno`) y a las etiquetas de SELinux. `--no-admin` deja solo el
token (hasta que `grafeno users create` cree una cuenta); `--no-start`
habilita el servicio sin iniciarlo.

```bash
grafeno instances
```

```text
NAME         ADDRESS                SERVICE                  DATABASE
default      127.0.0.1:7810         running                  /var/lib/grafeno/grafeno.redb
team         0.0.0.0:7820           running                  /srv/grafeno-team/grafeno.redb
```

Cada instancia tiene su propia configuración, datos, puerto y registros
(`journalctl -u grafeno@team`). `sudo grafeno instances drop team` detiene y
elimina una (su servicio y su configuración; con `--purge`, también sus
datos).

### Las actualizaciones protegen los datos

Cuando una versión nueva abre una base cuya versión de formato es anterior
(una actualización que cambia el formato del archivo), primero copia el
archivo, tal como está, a `grafeno.layout-<anterior>.redb` a su lado, y solo
entonces lo actualiza. Si no puede hacer la copia (no hay espacio en el
disco), el servidor no arranca y el archivo queda intacto; `grafeno doctor`
avisa con tiempo cuando falta espacio libre. Se conserva una copia por
instancia (la de la siguiente actualización la reemplaza); `grafeno doctor`
la muestra con su tamaño, para borrarla cuando ya no haga falta.
`copy_before_upgrade = false` en la configuración la desactiva.

Para volver a la versión anterior después de una actualización así:

```bash
sudo grafeno instances rollback default
```

```bash
sudo dnf downgrade grafeno
```

(o `sudo apt install grafeno=<version>-1`). El primer comando detiene el
servicio y restaura la copia (lo escrito desde entonces se guarda aparte
como `grafeno.before-rollback-<momento>.redb`); instalar la versión anterior
lo vuelve a iniciar.

### Windows

Ejecuta `grafeno-<version>-windows-x64-setup.exe` (pide permisos de
administrador), o `winget install GrafenoDB.Grafeno`. Pregunta:

- la **carpeta del programa** (`C:\Program Files\Grafeno` por defecto);
- la **carpeta de datos**, donde el servicio guarda su base
  (`C:\ProgramData\Grafeno\data` por defecto; otro disco sirve);
- el **puerto** (7810 por defecto);
- **quién puede conectarse**: solo este equipo (`127.0.0.1`), o también otros
  equipos (todas las interfaces de red, con una regla de firewall para el
  puerto); en ambos casos cada petición necesita el token o una cuenta;
- la **cuenta con la que corre el servicio**: `NT SERVICE\Grafeno`, una
  cuenta virtual que solo puede usar las carpetas de Grafeno (la opción por
  defecto), o LocalSystem;
- el **primer administrador**: un nombre y una contraseña (o ninguno, y
  mientras tanto sirve el token);
- si **ejecutar Grafeno como servicio de Windows** (arranca con Windows) y
  **agregar grafeno al PATH del sistema**.

Escribe `C:\ProgramData\Grafeno\grafeno.toml` (dirección, datos, un token
nuevo) e inicia el servicio `Grafeno`. El menú Inicio recibe *Grafeno Shell*
(conectada al servicio) y *Grafeno settings*. El servicio escribe sus eventos
en el Visor de eventos (Registros de Windows → Aplicación, origen `Grafeno`)
y en `C:\ProgramData\Grafeno\grafeno.log`, que pasa a `grafeno.log.1` al
llegar a 10 MiB.

En modo desatendido (scripts, aprovisionamiento), las mismas respuestas son
parámetros:

```powershell
.\grafeno-0.2.1-windows-x64-setup.exe /VERYSILENT /SUPPRESSMSGBOXES /DIR="C:\Grafeno" /DATADIR="D:\GrafenoData" /PORT=7810 /NETWORK=0 /ACCOUNT=virtual /ADMIN=admin /ADMINPASSWORD="..." /TASKS="service,path"
```

`/NETWORK=1` escucha en todas las interfaces y abre el puerto en el
firewall; `/ACCOUNT=system` ejecuta el servicio como LocalSystem;
`/LOG=setup.log` guarda un registro de la instalación.

Administra el servicio como cualquier otro (`services.msc`, o PowerShell como
administrador):

```powershell
Restart-Service Grafeno
```

Para actualizar, ejecuta el instalador nuevo sobre el anterior (o
`winget upgrade GrafenoDB.Grafeno`): detiene el servicio, reemplaza el
programa, conserva la configuración, los datos y la cuenta del servicio (no
vuelve a preguntar por ellos) e inicia el servicio. Desinstalar
(Configuración → Aplicaciones) elimina el programa, el servicio, la regla de
firewall y la entrada del PATH, y pregunta si eliminar también la
configuración y los datos (por defecto, no); en modo desatendido,
`unins000.exe /VERYSILENT` los conserva y
`unins000.exe /VERYSILENT /PURGE=1` los elimina.

Sin el instalador, `grafeno-x86_64-pc-windows-msvc.zip` trae solo
`grafeno.exe`; como administrador, `grafeno setup` crea la configuración, el
primer administrador y el servicio en un solo paso, o a mano:

```powershell
grafeno config init
```

```powershell
grafeno service install --account "NT SERVICE\Grafeno"
```

(La cuenta virtual necesita entonces permiso de modificación sobre
`C:\ProgramData\Grafeno` y la carpeta de datos, que el instalador concede.)

### Dónde queda cada cosa

| Qué | Linux (paquetes) | Windows (instalador) |
| --- | ---------------- | -------------------- |
| El programa | `/usr/bin/grafeno` | `C:\Program Files\Grafeno\grafeno.exe` |
| Configuración, instancia por defecto | `/etc/grafeno/grafeno.toml` | `C:\ProgramData\Grafeno\grafeno.toml` |
| Configuración, otras instancias | `/etc/grafeno/<nombre>.toml` | (una instancia) |
| Datos, instancia por defecto | `/var/lib/grafeno/grafeno.redb` | `C:\ProgramData\Grafeno\data\grafeno.redb` |
| Datos, otras instancias | `/var/lib/grafeno/<nombre>/grafeno.redb` | |
| Copia guardada antes de actualizar el formato | `grafeno.layout-<anterior>.redb` junto a la base | lo mismo |
| El servicio | `grafeno.service`, `grafeno@<nombre>.service` | el servicio `Grafeno` |
| Registros | journald (`journalctl -u grafeno`) | Visor de eventos (Aplicación, `Grafeno`), `C:\ProgramData\Grafeno\grafeno.log` |
| Confinamiento | aislamiento de systemd; AppArmor `/etc/apparmor.d/grafeno`; módulo SELinux `grafeno` | la cuenta virtual `NT SERVICE\Grafeno` |
| Scripts del paquete | `/usr/share/grafeno/setup.sh`, `stop-all.sh` | |

## Un clúster de servidores

Tres (o cinco) servidores pueden tener los mismos datos y seguir atendiendo
cuando uno falla (fases 13.2 y 13.3). Cada uno recibe la misma sección
`[cluster]` en su `grafeno.toml`, con su propio `id` y su propia `addr`:

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

Inicia los tres: eligen un líder en cerca de un segundo. Envía peticiones a
cualquier miembro: un seguidor reenvía las escrituras al líder (con `api` en
`peers`; sin ella, responde 421 `not_leader` con el nombre del líder), y las
lecturas las atiende el propio miembro. Una lectura puede pedir más con la
cabecera `x-grafeno-consistency`: `bounded` va al líder cuando el miembro
está muy atrasado, `linearizable` ve cada escritura confirmada antes de ella.
Cada respuesta lleva `x-grafeno-position`; una petición con
`x-grafeno-after: N` espera hasta que su miembro haya aplicado N (leer tus
propias escrituras). Una escritura GQ enviada con la cabecera
`Idempotency-Key` se ejecuta una vez, por muchas veces que se reintente. La
consola (`--url` con varias direcciones, separadas por comas) y el SDK de
TypeScript (`url: [...]`) pasan a otro miembro cuando uno falla y leen sus
propias escrituras. Cuando el líder muere, las escrituras se reanudan en
cerca de medio segundo.

Una lectura costosa (con un costo estimado desde `split_cost` en
`[cluster]`, 200 000 por defecto, tal como lo muestra `EXPLAIN`) se reparte
entre los miembros: cada uno ejecuta una parte en la misma posición, y el
miembro consultado une las partes en las mismas filas que devuelve un solo
servidor. `x-grafeno-split: on` reparte una lectura siempre que su forma lo
permite, `off` nunca. Los miembros que pueden tomar partes necesitan `api` en
`peers`.

Las escrituras de un grupo pasan por un líder a la vez. Para escribir más,
usa más grupos, cada uno con su propio líder y su propio archivo de base
(`<db>.group-<id>.redb`) en sus votantes:

```toml
[cluster]
# … as above, then:
groups = [{ id = 1, voters = [1, 2, 3] }, { id = 2, voters = [1, 2, 3] }]
```

Los namespaces viven en el grupo 0 hasta que se colocan en otro:

```bash
curl -X POST http://10.0.0.1:7810/v1/cluster/placement -H "Authorization: Bearer …" -H 'content-type: application/json' -d '{"namespace": "vault", "group": 1}'
```

Un namespace que ya tiene datos se mueve sin dejar de atender: sus
escrituras se pausan unas decenas de milisegundos (responden que está
ocupado; los clientes reintentan) y no se pierde ninguna. Cualquier miembro
acepta peticiones para cualquier namespace. Respalda el archivo de cada grupo
(`GET /v1/backup?group=1`; los respaldos pueden hacerse en un seguidor).
`grafeno cluster status --url http://10.0.0.1:7810 --token …` (`--group 1`
para otro grupo) muestra la vista de un miembro, con elecciones, cambios de
líder, snapshots, el formato de entrada que escribe el grupo, qué ejecuta
cada miembro y, en el líder, el atraso de cada miembro; `/v1/metrics` tiene
lo mismo como series `grafeno_cluster_*`, y Grafeno Studio una vista
Cluster; `grafeno cluster voters 1 2 3 4` (en el líder) cambia los miembros,
con el nuevo listado en los `peers` de cada miembro e iniciado con
`voters = []`; `grafeno cluster compact` toma un snapshot de la base de un
miembro y recorta su log. Cada miembro guarda su estado Raft en `<db>.raft` y
sus snapshots en `<db>.snapshots/`.

Un clúster de tres miembros con compose, configuraciones con systemd y con
servicios de Windows, y los procedimientos para perder un miembro, perder la
mayoría, reemplazar un disco y actualizar están en
[cluster-runbooks.md](cluster-runbooks.md) (en inglés).

Por ahora: el tráfico entre miembros se autentica con el secreto pero no se
cifra (mantenlos en una red privada); las importaciones van a un solo
servidor antes de que inicie un clúster. Los webhooks y el trabajo en segundo
plano (extracción, resúmenes) se ejecutan en el líder, y un líder nuevo los
retoma.

## Cuentas

Un servidor nuevo está abierto para cualquiera que llegue a él (bien en tu
propio equipo) hasta que tenga cuentas, o un token en `grafeno.toml`. Crea la
primera cuenta, un superusuario, con el servidor detenido o a través de él:

```bash
grafeno users create admin --superuser --config /etc/grafeno/grafeno.toml
```

(En un servidor abierto, Grafeno Studio también ofrece crearla al
conectarse.) Luego crea las personas que inician sesión, con permisos por
namespace, y cuentas de servicio para agentes y aplicaciones, que usan
claves de API:

```bash
grafeno users create ana --grant notes:write --config /etc/grafeno/grafeno.toml
```

```bash
grafeno users create indexer --service --grant "team-*:write" --config /etc/grafeno/grafeno.toml
```

Los usuarios inician sesión desde Studio, la consola
(`grafeno shell --url … --user ana`) o la API (`POST /v1/auth/signin`), y
pueden crear sus propias claves de API, limitadas a algunos namespaces o a
lectura. `grafeno users password`, `disable`, `enable` y `delete` los
administran. Las claves de antes de la 14.1 siguieron funcionando: cada una
pasó a ser una cuenta de servicio con su nombre. El token de
`grafeno.toml`, si existe, sigue pudiendo todo: guárdalo para emergencias.
`[auth]` (más abajo) fija cuánto duran las sesiones y cuándo los inicios de
sesión fallidos bloquean una cuenta.

Los roles (14.2) agrupan privilegios: define uno con `PUT /v1/roles/{nombre}`
(por ejemplo, leer todos los namespaces `team-*` menos `team-secret`, o ver
solo las métricas, para un agente de monitorización) y dáselo a cuentas con
`PATCH /v1/accounts/{nombre}` y `{"roles": [...]}`. Un rol llamado `public`
es de todos. El README del servidor lista los privilegios.

Lo mismo en GQ (14.3), desde la consola o la vista Scripts de Studio, con
una cuenta que pueda administrar roles:

```text
CREATE ROLE team_reader;
GRANT read, export ON NAMESPACE 'team-*' TO team_reader;
DENY read ON NAMESPACE 'team-secret' TO team_reader;
GRANT ROLE team_reader TO ana;
```

o desde la línea de comandos
(`grafeno grants grant read --on namespace:team-* --to team_reader`,
`grafeno roles show team_reader`), y en la vista Access de Studio.

Los permisos llegan a etiquetas, propiedades y registros (14.4):
`DENY read ON NODES Salary IN crm TO team_reader` oculta esos nodos en todas
partes, `DENY read ON PROPERTY Person.email IN crm` oculta una propiedad, y
`RESTRICT team_reader ON NAMESPACE crm NODES Customer WHERE tenant = $account.tenant`
limita cada cuenta a los clientes de su organización
(`ALTER USER ana SET ATTRIBUTE tenant = 'acme'`). Lo oculto está ausente,
como si no existiera.

Cada evento de seguridad va al registro de auditoría (14.5): inicios de
sesión y sus fallos, bloqueos, claves, sesiones, cambios en cuentas y roles,
peticiones rechazadas. Léelo con una cuenta que tenga el privilegio `audit`
(`GET /v1/audit`, `?format=jsonl` para guardar una copia); `[audit]` fija
cuánto se conserva:

```toml
[audit]
max_entries = 10000000
max_age_days = 365
```

## Respaldos

Un respaldo es una copia consistente de todo el archivo de la base (cada
namespace, y las claves de API), hecha mientras el servicio sigue en marcha:

```bash
grafeno backup grafeno-backup.redb --config /etc/grafeno/grafeno.toml
```

(En Windows, la configuración está en `C:\ProgramData\Grafeno\grafeno.toml`;
`grafeno config path` muestra dónde.) Para restaurar, detén el servicio,
pon la copia donde apunta `db` en `grafeno.toml` y vuelve a iniciarlo. Antes
de usar un archivo que puede estar dañado (un disco que falla, una mala
copia), revísalo con el servicio detenido:

```bash
grafeno check --db grafeno-backup.redb
```

Verifica cada página contra su suma de comprobación, y luego los índices.
Las exportaciones (`grafeno export`) son el otro tipo de copia: JSON Lines,
legibles y portables entre versiones, pero sin las claves.

Para llevar un namespace a otras herramientas o traerlo de ellas (hojas de
cálculo, programas de dibujo y análisis de grafos, otras bases de datos),
usa CSV o GraphML. Guardan el conocimiento actual, sin los hechos retractados
ni la historia detrás de ellos, así que no reemplazan un respaldo:

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

`grafeno.toml` lo lee el servicio, y también `grafeno serve --config` y
`grafeno shell --config`. Reinicia el servicio después de cambiarlo.

```toml
db = "/var/lib/grafeno/grafeno.redb"
addr = "127.0.0.1:7810"     # "0.0.0.0:7810" listens on every network interface
token = "…"                 # every /v1 request needs `Authorization: Bearer <token>`
cors_origins = []           # origins allowed to call the API from a browser ("*": any)
durability = "immediate"    # or "eventual": faster writes, a crash loses at most 0.5 s
max_requests = 512          # served at once; more get 503 with Retry-After
copy_before_upgrade = true  # copy the database before a new version upgrades its layout

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

[events]                    # change events with values (off by default)
enabled = false             # true: CREATE EVENT SUBSCRIPTION works, and
                            # commits record the values subscriptions ask for

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

[transactions]              # BEGIN … COMMIT across requests
idle_timeout_secs = 30      # rolled back after this long without a request
max_secs = 600              # rolled back at this age, busy or not
wait_secs = 5               # how long BEGIN waits for the writing one before it
max_open = 256
```

Los flags y las variables de entorno (`GRAFENO_DB`, `GRAFENO_ADDR`,
`GRAFENO_TOKEN`) tienen prioridad sobre el archivo. Expón el puerto más allá
de `127.0.0.1` solo detrás de TLS (un proxy inverso) y con un token.

### Memoria

Las cachés de Grafeno son la memoria principal que conserva: páginas del
archivo de la base (`cache_mb`), grafos proyectados para algoritmos
(`projection_cache_mb`) y vectores decodificados al insertar
(`vector_cache_mb`). Todo lo demás es por petición y está acotado por los
límites de `[query]`. Con 10 millones de aristas un servidor se mantiene
cerca de 300 MiB con los valores por defecto; en una máquina de 512 MiB fija
`cache_mb = 128` y `projection_cache_mb = 64` (los algoritmos de grafos sobre
un grafo así construyen entonces su proyección, de unos 140 MiB, cada vez, y
pueden necesitar un `timeout_secs` mayor). Cargar muchos embeddings va cerca
del doble de rápido con `vector_cache_mb = 1024` y transacciones de escritura
grandes, si la memoria alcanza. Las cifras están en
[benchmarks](benchmarks.md).

### Monitorización

`GET /v1/metrics` responde en el formato de texto de Prometheus: las
peticiones y su tiempo, las consultas GQ, los límites que alcanzan, la cola
del escritor, los grupos, el tiempo de commit y la latencia de escritura por
carril. Necesita un secreto, como el resto de `/v1`; sirve una clave con
cualquier permiso:

```yaml
scrape_configs:
  - job_name: grafeno
    metrics_path: /v1/metrics
    authorization:
      credentials: gfk_…
    static_configs:
      - targets: ["127.0.0.1:7810"]
```

Vigila `grafeno_write_queue` y `grafeno_write_seconds`: una cola que se
mantiene alta indica que las escrituras llegan más rápido de lo que un
escritor las confirma (cerca de 11 000 por segundo en un SSD con durabilidad
inmediata, más con `durability = "eventual"`);
`grafeno_write_rejected_total` cuenta los 503 por encima del límite de la
cola. Carga las exportaciones grandes con `?bulk=true`: así van en tramos
entre las escrituras de los agentes en lugar de antes que ellas.

## Navegadores y Grafeno Studio

El motor no sirve ninguna interfaz (antes de Grafeno 15 tenía Studio en
`/studio/`; esa dirección ahora indica adónde se fue). Grafeno Studio,
instalado por su cuenta, se conecta a la dirección del motor con su token.
Una página web que llama a la API desde un navegador (Studio alojado en un
servidor web, una aplicación propia) necesita que su origen esté permitido:

```bash
grafeno serve --config grafeno.toml --cors-origin https://studio.example.com
```

Grafeno Desktop, la aplicación de escritorio, no necesita ningún origen
permitido (envía las peticiones por sí misma). Para abrir un archivo de base
usa el motor instalado aquí: inicia `grafeno serve --stop-with-stdin` para
ese archivo en un puerto local con un token propio, y el motor se detiene
cuando se cierra el archivo o la aplicación. Instala primero el motor; la
aplicación lo encuentra en el `PATH` o donde lo dejan estos instaladores (o
en `GRAFENO_BIN`).

## Versiones y releases

El motor y Grafeno Studio / Grafeno Desktop son dos productos con sus propias
versiones, registros de cambios y releases: los del motor se etiquetan
`v<version>` en este repositorio ([CHANGELOG.md](../CHANGELOG.md)), los de la
aplicación de escritorio `desktop-v<version>` en el suyo. Cada uno se instala
por su cuenta y no toca al otro: los paquetes del motor no traen nada
gráfico, los del escritorio ni un servicio ni un motor, e instalar,
actualizar o quitar cualquiera de los dos conserva el otro y los datos del
motor. La aplicación de escritorio indica con qué versiones del motor
funciona (por la versión de la API de `GET /v1/about`, hoy 1).

## Compilar los instaladores

| Archivos | Comando | Necesita |
| -------- | ------- | -------- |
| Linux `.tar.gz`, `.deb`, `.rpm`, x86_64 y aarch64 | `docker build -f packaging/linux/Dockerfile --output dist/linux .` | Docker |
| Los repositorios APT y DNF | `GRAFENO_GPG_KEY=<key id> packaging/repo/build-repo.sh dist/linux site https://grafenodb.github.io/grafeno` | GnuPG, rpm, createrepo_c, apt-utils |
| Instalador `.exe` de Windows | `cargo build --release -p grafeno-server`, luego `iscc /DAppVersion=0.2.1 packaging\windows\grafeno.iss` | Rust (MSVC), Inno Setup 6 |
| Manifiestos de winget | `python packaging/windows/winget/make-manifests.py 0.2.1 dist\grafeno-0.2.1-windows-x64-setup.exe <url> dist\winget` | Python |

Ni Node ni un toolkit gráfico: el motor se compila solo con Rust. El binario
de aarch64 se compila de forma cruzada con Zig (`cargo zigbuild`), y el
módulo de SELinux se compila en una etapa AlmaLinux del mismo Dockerfile. Los
paquetes de Linux también salen con nombres sin la versión
(`grafeno-linux-x86_64.rpm`, `grafeno-linux-amd64.deb`,
`grafeno-linux-aarch64.rpm`, `grafeno-linux-arm64.deb`), que el instalador de
una línea pide al último release. `packaging/windows/grafeno.ico` es el
icono del ejecutable, del instalador y de sus accesos:
`packaging/windows/make-icon.sh` lo genera (de 16 a 256 píxeles, con Docker)
a partir de `packaging/windows/grafeno-mark.svg`, la marca del sitio de
Grafeno.

`.github/workflows/release.yml` los compila todos cuando se sube un tag
`v*` y publica un release en GitHub; agrega los paquetes a los repositorios
de la rama `gh-pages` (servida por GitHub Pages). Lo que firma viene de los
secretos del repositorio, cada uno opcional:

| Secreto | Qué firma |
| ------- | --------- |
| `MINISIGN_SECRET_KEY` | cada archivo del release (un `.minisig` junto a cada uno); el instalador de una línea los comprueba con la clave pública |
| `GPG_PRIVATE_KEY` | los metadatos de los repositorios y los archivos `.rpm` que contienen |
| `WINDOWS_CERTIFICATE`, `WINDOWS_CERTIFICATE_PASSWORD` | `grafeno.exe`, el instalador y su desinstalador (Authenticode) |

Crear las claves, una sola vez: `minisign -G -W` (publica la clave pública,
y pon `MINISIGN_KEY` en `install.sh` con ella), y
`gpg --quick-gen-key "Grafeno releases" rsa4096 sign never` sin frase de
paso, exportada con `gpg --armor --export-secret-keys`. GitHub Pages debe
servir la rama `gh-pages`. Los manifiestos de winget vienen con cada release
(`grafeno-<version>-winget.zip`), para enviarlos a `microsoft/winget-pkgs`.

Cómo se prueban en máquinas limpias:

- `packaging/linux/test-install.sh [--repo] [--systemd] [--arm64] [imagen ...]`
  sirve `dist/linux` como un release, firmado con claves desechables, y
  ejecuta el instalador de una línea, como root y como usuario, en
  contenedores limpios (AlmaLinux 9 y 8, Rocky 9, Fedora 42, Debian 12,
  Ubuntu 24.04 por defecto): la suma de comprobación y la firma (una clave
  equivocada se rechaza), la versión, el usuario, la configuración, el
  servidor respondiendo con ella, `grafeno doctor`, nada gráfico instalado,
  y la purga del `.deb`. `--repo` construye los repositorios con una clave
  GnuPG desechable e instala desde ellos, con los gestores de paquetes
  comprobando las firmas. `--arm64` lo repite en aarch64 (emulado).
  `--systemd` arranca systemd en un contenedor AlmaLinux: el servicio corre
  y está habilitado; `grafeno setup` crea una segunda instancia con un
  administrador que inicia sesión, y otra con sus datos en otro lugar que
  luego se elimina con sus datos; una base antigua se copia antes de
  actualizar su formato y `grafeno instances rollback` la restaura; una
  actualización reinicia las instancias en marcha; quitar el paquete
  conserva los datos.
- `.github/workflows/installers.yml` ejecuta eso, instala el `.deb` en el
  propio runner de Ubuntu con su perfil de AppArmor aplicado (el servicio y
  una instancia con sus datos en otro lugar, confinados, funcionando y sin
  ninguna denegación), y en un runner de Windows instala el instalador en
  modo desatendido (su propia carpeta de datos y puerto, la cuenta virtual,
  un administrador que inicia sesión, eventos legibles en el Visor de
  eventos, `grafeno doctor`), actualiza encima (conservando la
  configuración y la cuenta), desinstala conservando los datos, instala
  sobre los datos conservados, y desinstala con `/PURGE=1`.
