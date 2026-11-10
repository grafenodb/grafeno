# Contribuir a Grafeno

[English](CONTRIBUTING.md) | **Español**

Esta guía prepara el motor y su cliente TypeScript para desarrollar y
verificar cambios. Ejecuta los comandos desde la raíz del repositorio salvo
que se indique otro directorio. La interfaz gráfica, Grafeno Studio, vive en
su propio repositorio ([Grafeno Studio](https://github.com/grafenodb/grafeno-studio)) desde Grafeno 15.

## Requisitos

- Rust estable mediante rustup. El workspace declara Rust 1.88 como mínimo;
  CI usa Rust estable y todavía no verifica la versión mínima.
- En Windows: Visual Studio Build Tools con las herramientas de C++ y la
  cadena MSVC de Rust.
- Node.js 24 y npm solo para compilar y probar el cliente TypeScript; el
  motor compila sin ellos.
- Para construir los instaladores: Docker (el `.tar.gz`, el `.deb` y el
  `.rpm` de Linux) e Inno Setup 6 en Windows (el instalador `.exe`).

Comprueba las herramientas antes de empezar:

```bash
rustc --version
cargo --version
```

## Comprobar el motor

```bash
cargo test --workspace --locked
cargo build --locked -p grafeno-server
```

Si el almacenamiento temporal es limitado, en Linux puedes reducir los
artefactos de compilación con esta variante (también la usa CI):

```bash
CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 \
  cargo test --workspace --locked -j 2
```

Desactiva la información de depuración y la compilación incremental, pero
conserva las pruebas y las aserciones de depuración. No requiere cambiar
`Cargo.toml`.

## Comprobar el cliente

```bash
npm ci --prefix clients/typescript
npm run build --prefix clients/typescript
npm test --prefix clients/typescript
```

Las pruebas del cliente inician el servidor real desde
`target/debug/grafeno` (`grafeno.exe` en Windows), así que requieren la
compilación del servidor anterior. Define `GRAFENO_BIN` para usar otro
binario. Conserva el lockfile; usa `npm ci` y los comandos de Cargo con
`--locked` para detectar cambios inesperados de dependencias.

## Construir los instaladores

Consulta [los instaladores](docs/install.md#building-the-installers) para el
instalador de Windows, los paquetes de Linux y el instalador de una línea, y
cómo se prueba cada uno en máquinas limpias.

## Comprobaciones de cada contribución

El workflow de [comprobaciones de contribuciones](.github/workflows/ci.yml) se
ejecuta en los pull requests, en los pushes a `main` y manualmente. Así los
pushes a una rama con un PR abierto no duplican las ejecuciones. Comprueba por
separado:

- Tipos y compilación del cliente TypeScript.
- Pruebas Rust del motor y del servidor, más el cliente contra el servidor real.

Estas comprobaciones no sustituyen las pruebas de los instaladores en máquinas
limpias. Trabaja en una rama y abre un PR acotado. Describe el problema, el
comportamiento resultante y las comprobaciones ejecutadas, incluyendo las que
no pudiste realizar. Conserva los nombres técnicos y comandos del proyecto.

## Licencia

Grafeno tiene licencia MIT (`LICENSE-MIT`) o Apache 2.0 (`LICENSE-APACHE`),
a tu elección. Salvo que indiques lo contrario, toda contribución que envíes
para incluirla queda bajo las mismas licencias, sin términos ni condiciones
adicionales.

La documentación y el código describen Grafeno por sí mismo: sin nombrar
otras empresas, productos, aplicaciones cliente ni videojuegos (los sistemas
operativos y las dependencias de Grafeno sí se nombran).
`crates/grafeno/tests/names.rs` lo comprueba.
