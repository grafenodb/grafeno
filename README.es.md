# Grafeno

<p align="center">
  <img src="docs/assets/readme-banner-es.svg" width="1440" alt="Símbolo de Grafeno: tres anillos de carbono conectados a una red de nodos.">
</p>

<p align="center">
  <a href="README.md">English</a> · <strong>Español</strong><br>
  <a href="#instalar-el-motor">Empezar</a> ·
  <a href="docs/instalacion.md">Instalación</a> ·
  <a href="docs/query-language.md">Consultas GQ</a> ·
  <a href="CONTRIBUTING.es.md">Contribuir</a>
</p>

Grafeno es un **motor de grafos en Rust para memoria y conocimiento de agentes**.
Conecta entidades, documentos y hechos; conserva su procedencia e historial;
y combina búsqueda por texto, vectores y relaciones para recuperar contexto.

Grafeno es solo el motor, como lo es un servidor de bases de datos: un
servidor (API HTTP, MCP para agentes, clúster), una consola (`grafeno shell`)
y comandos de administración, sin interfaz gráfica. **Grafeno Studio**, la
interfaz, es un producto aparte que se instala por su cuenta y se conecta a los
motores a través de su API, como cualquier otro cliente. Grafeno también se
integra como biblioteca Rust.

## Qué puedes hacer

| Capacidad | Para qué sirve |
| --- | --- |
| **Conectar conocimiento** | Nodos con etiquetas y propiedades, relaciones dirigidas y namespaces para organizar los datos. |
| **Consultar el pasado** | Distinguir cuándo un hecho era válido y cuándo el sistema lo conoció mediante `VALID AT` y `KNOWN AT`. |
| **Encontrar contexto** | Combinar búsqueda BM25, vectores HNSW y expansión del grafo; preparar contexto con un presupuesto de tokens. |
| **Mantener la memoria** | Resolver entidades, revisar posibles duplicados, registrar fusiones reversibles y resumir comunidades sin un modelo. |
| **Consultar** | GQ, el lenguaje declarativo de consultas de grafos de Grafeno, desde la consola, la API HTTP, MCP o cualquier cliente. |
| **Operar la base** | Cuentas que inician sesión, roles con concesiones y denegaciones hasta etiquetas, propiedades y registros, claves de API, respaldos, importación y exportación, comprobaciones de integridad y reconstrucción de índices. |
| **Operar un clúster** | Varios servidores sincronizados con Raft: elección de líder, recuperación ante fallos, lecturas linealizables, lecturas grandes repartidas entre miembros y namespaces en varios grupos. |

## Instalar el motor

Windows (un instalador `.exe` con el servicio, o winget), Linux en x86_64 y
aarch64 (repositorios APT y DNF firmados, `.deb`, `.rpm` y un instalador de
una línea con `curl` que también sirve para AlmaLinux, Rocky y RHEL) y un
`.tar.gz`: consulta la [guía de instalación](docs/instalacion.md). El motor se
ejecuta como servicio en `127.0.0.1:7810`; su configuración está en
`grafeno.toml`. `grafeno setup` agrega instancias (varias bases de datos en
una máquina, cada una con su servicio), y `grafeno doctor` las revisa. Las versiones del motor se
etiquetan `v<versión>`; lo que cambia cada una está en
[CHANGELOG.md](CHANGELOG.md).

Desde el código, con Rust instalado:

```bash
cargo build --release -p grafeno-server
```

El binario queda en `target/release/grafeno`.

Un archivo `.redb` solo puede estar abierto en un proceso a la vez: detén el
servicio antes de abrir su archivo con la consola, o conecta la consola al
servicio (`grafeno shell --config …`).

## Una consulta, relaciones e historia

Con el binario `grafeno` instalado, abre una consola local:

```bash
grafeno shell --db ejemplo.redb -n demo
```

Crea una persona y una relación con fecha de inicio:

```text
CREATE (a:Person {name: 'Ana'})
  -[:works_at {@valid_from: '2024-01-01'}]->
  (h:Organization {name: 'Hospital Central'})
RETURN a.name, h.name;
```

Consulta qué era válido en una fecha:

```text
MATCH (p:Person)-[r:works_at]->(o:Organization)
VALID AT '2024-06-01'
RETURN p.name, o.name, r.@valid_from;
```

GQ también incluye agregaciones, índices y algoritmos de caminos, centralidad,
comunidades y similitud. Consulta la [referencia de GQ](docs/query-language.md).

## Integrar aplicaciones y agentes

| Forma de acceso | Punto de entrada |
| --- | --- |
| Servicio HTTP, servidor MCP y consola | [Servidor Grafeno](crates/grafeno-server/README.md) |
| Una interfaz gráfica | Grafeno Studio, instalado aparte, mediante la API HTTP |
| Aplicaciones JavaScript y TypeScript | [`@grafenodb/client`](https://github.com/grafenodb/grafeno-sdk/tree/main/typescript), el SDK de TypeScript (`npm install @grafenodb/client`) |
| Biblioteca Rust embebida | [`grafeno`](crates/grafeno/src/lib.rs) y [`grafeno-memory`](crates/grafeno-memory/src/lib.rs) |

Para iniciar MCP con una base propia, después de instalar el binario:

```bash
grafeno mcp --db agente.redb
```

Para probar el servidor HTTP con datos temporales en memoria:

```bash
grafeno serve --memory
```

Escucha por defecto en `127.0.0.1:7810`. Los datos de `--memory` se pierden al cerrar el proceso. Para una configuración
persistente y autenticada, sigue la [guía del servidor](docs/instalacion.md#configuración).

## Cómo está organizado

| Directorio | Responsabilidad |
| --- | --- |
| `crates/grafeno-core` | Modelo de nodos, relaciones, identificadores y tiempo. |
| `crates/grafeno` | Almacenamiento redb, transacciones, índices, recorridos y algoritmos. |
| `crates/grafeno-memory` | Recuperación híbrida, contexto, extracción y mantenimiento de memoria. |
| `crates/grafeno-query` | Parser, planificación y ejecución de GQ. |
| `crates/grafeno-server` | API HTTP, MCP, consola y administración: el binario `grafeno`. |
| `crates/grafeno-raft`, `crates/grafeno-cluster` | Raft y el clúster. |
| `packaging` | Instalador de Windows, paquetes de Linux, la preparación del instalador de una línea y una compose de clúster. |

## Desarrollo y pruebas

La [guía de contribución en español](CONTRIBUTING.es.md) y su
[versión en inglés](CONTRIBUTING.md) incluyen los requisitos y una alternativa
que reduce el espacio temporal utilizado.

```bash
cargo test --workspace --locked
```

El [workflow de contribuciones](.github/workflows/ci.yml) verifica el motor y
el servidor, y ejecuta las pruebas del SDK de TypeScript (de
[grafeno-sdk](https://github.com/grafenodb/grafeno-sdk)) contra el binario que compila. Compilar no requiere
Node: solo lo usan esas pruebas.

## Diseño, rendimiento y alcance

- [Diseño y decisiones](DESIGN.md): arquitectura, almacenamiento y semántica temporal.
- [Fases](docs/phases/README.md) (en inglés): cada fase de Grafeno en su propio archivo, con sus pasos.
- [Rendimiento](docs/performance.md) y [benchmarks](docs/benchmarks.md): métodos, datos y límites de las mediciones del proyecto.
- [Lecturas por lotes (en inglés): contrato, pruebas y medición reproducible](docs/read-many.md).
- [Expansión del grafo](docs/expansion-grafo.md): vecindarios por lotes y protección ante respuestas tardías.
- [Historial de desarrollo](docs/development-history.md): detalle de las fases documentadas originalmente.

Los embeddings los proporciona el cliente. El presupuesto de contexto utiliza
una estimación de tokens por defecto; la biblioteca Rust permite proporcionar
el contador del modelo. El motor admite múltiples lectores y un escritor;
en un clúster, cada escritura se replica a sus miembros (fase 13).

## Licencia

Grafeno es software libre, con licencia [MIT](LICENSE-MIT) o
[Apache 2.0](LICENSE-APACHE), a tu elección. Salvo que digas lo contrario,
una contribución que envíes queda bajo las mismas licencias.

Creado por [Seerendo](https://github.com/Seerendo). La identidad gráfica representa
tres anillos de carbono fusionados de una red de grafeno.
