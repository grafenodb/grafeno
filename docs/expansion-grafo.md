# Expansión del grafo por lotes

**Español** | [English](graph-expansion.md)

Studio carga los vecindarios de las raíces expandidas en grupos de ocho. Para
18 raíces realiza tres solicitudes (8, 8 y 2), en lugar de 18. La vista conserva
el orden de las raíces, los filtros temporales y el límite visual anterior de
150 nodos nuevos por expansión. Los nodos y aristas compartidos se deduplican
al combinar los resultados, sin cambiar el orden de incorporación.

## API y consistencia

`POST /v1/{namespace}/neighborhoods` recibe `seeds`, `direction` y `filter`.
Devuelve una lista de subgrafos, uno por raíz y en el mismo orden. Cada elemento
equivale a `subgraph([raiz], {depth: 1, direction, filter})`, incluidas las aristas
entre miembros del vecindario. No se sustituye por la unión de todas las raíces,
porque esa unión perdería la separación necesaria para mantener los límites.

```ts
const around = await vault.neighborhoods([ana.id, bob.id], {
  filter: { valid_at: "2026-06-01" },
});
```

Se admiten de cero a ocho raíces, contando duplicados. La profundidad es siempre
uno; `depth` no forma parte de este contrato. Más de ocho raíces, IDs inválidos
y campos desconocidos producen 400. Una raíz inexistente produce 404 para el
lote, igual que la lectura individual anterior. Se requiere lectura del namespace.

Cada grupo utiliza una instantánea común. Una expansión con más de ocho raíces
utiliza varios grupos secuenciales y puede observar escrituras entre ellos; no
promete una instantánea global de todo el lienzo. El motor recorre las raíces
sin crear un pool de hilos por solicitud. `read-many` sigue siendo la operación
para registros cuyos IDs se conocen; `neighborhoods` calcula sus vecindarios.

## Respuestas pendientes y servidores anteriores

Cambiar raíces, fecha, namespace o revisión invalida la carga visual anterior.
Limpiar el lienzo también invalida búsquedas y listas de “Show all” pendientes.
Una respuesta tardía no publica nodos ni errores antiguos, ni inicia el siguiente
lote. Las solicitudes ya enviadas pueden terminar: esto descarta resultados y
frena trabajo futuro, sin prometer interrumpir el motor.

Si un servidor anterior devuelve un 404/405 genérico para el nuevo endpoint,
Studio utiliza `subgraph` individual con hasta cuatro llamadas concurrentes por
carga. Conserva el orden al combinarlas. No activa esta compatibilidad ante
401, 403, errores internos o un 404 estructurado de raíz ausente. Un fallo detiene
las raíces aún no enviadas. Solicitudes antiguas ya en ejecución pueden coincidir
con una carga nueva; cuatro no es un límite global de todas las cargas.

El límite de 150 es visual. No recorta el recorrido ni garantiza que el servidor
lea solo 150 registros: un nodo de gran grado puede devolver un vecindario mucho
mayor. Esta entrega conserva esa semántica anterior.

## Medición local

El benchmark existente compara ahora también vecindarios: un servidor privado
con 256 nodos y 256 aristas en anillo, tres calentamientos por estrategia y 25
rondas con orden rotatorio. Verifica igualdad con los subgrafos individuales
fuera del intervalo medido.

```bash
cargo build --locked -p grafeno-server
node ../grafeno-sdk/typescript/bench/read-many.mjs   # el SDK clonado junto al motor
```

La sección `neighborhoods` del JSON corresponde a esta comparación. `GRAFENO_BIN`
y `GRAFENO_BENCH_ROUNDS` permiten elegir binario y repeticiones. CI ejecuta una
ronda para verificar el script, sin umbrales de rendimiento.

Medianas locales en milisegundos, 6 de noviembre de 2026, Linux x64,
Intel i7-10750H, Node 24.22.2 y perfil `dev`:

| Raíces | Secuencial | Concurrencia 4 | Lotes de 8 | Solicitudes por lote |
| --- | ---: | ---: | ---: | ---: |
| 8 | 2,731 | 1,491 | 0,564 | 1 |
| 18 | 5,854 | 3,028 | 1,481 | 3 |
| 60 | 18,645 | 10,226 | 4,434 | 8 |

[Datos completos, p95 y entorno](data/graph-neighborhoods-local.json).

Se mide HTTP local y JSON sobre una base en memoria sin escrituras durante la
comparación. No mide renderizado, FPS, Tauri, disco, hubs de gran grado ni cargas
reales de usuarios. Los resultados de `read-many` son otra carga de trabajo;
no deben atribuirse al lienzo. Estos números no son garantías.

## Validación

```bash
npm test --prefix studio   # en el repositorio de Grafeno Studio
cargo test --locked -p grafeno-server --test http neighborhoods
cargo test --locked -p grafeno-server --test access
npm test --prefix ../grafeno-sdk/typescript
```

Las pruebas de Studio cubren agrupación, orden, límite por raíz, deduplicación,
compatibilidad, errores y cancelación. HTTP compara vecindarios individuales y
agrupados con direcciones y filtros temporales, y la matriz de permisos incluye
el nuevo endpoint. La prueba Tauri abre 18 raíces, comprueba tres solicitudes,
aplica una fecha y retiene respuestas reales para verificar la limpieza del
lienzo. Esa retención se inyecta solo en la prueba, sin comandos de prueba nuevos
en el producto. Las capturas se conservan en el artefacto habitual de escritorio.
