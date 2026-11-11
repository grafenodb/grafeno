# Contribuir a Grafeno

[English](CONTRIBUTING.md) | **Español**

Esta guía prepara el motor para desarrollar y
verificar cambios. Ejecuta los comandos desde la raíz del repositorio salvo
que se indique otro directorio. La interfaz gráfica, Grafeno Studio, vive en
su propio repositorio ([Grafeno Studio](https://github.com/grafenodb/grafeno-studio)) desde Grafeno 15.

## Requisitos

- Rust estable mediante rustup. El workspace declara Rust 1.88 como mínimo;
  CI usa Rust estable y todavía no verifica la versión mínima.
- En Windows: Visual Studio Build Tools con las herramientas de C++ y la
  cadena MSVC de Rust.
- Node.js 24 y npm solo para ejecutar las pruebas del SDK de TypeScript
  contra el motor; el motor compila sin ellos.
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

## Comprobar los SDK contra tu motor

Los SDK viven en su propio repositorio, [grafeno-sdk](https://github.com/grafenodb/grafeno-sdk). Un cambio en
la API HTTP debería mantenerlos funcionando: clona ese repositorio junto a
este, y sus pruebas usan el motor compilado aquí:

```bash
git clone https://github.com/grafenodb/grafeno-sdk.git ../grafeno-sdk
```

```bash
npm ci --prefix ../grafeno-sdk/typescript
npm test --prefix ../grafeno-sdk/typescript
```

Las pruebas inician el servidor real desde `target/debug/grafeno`
(`grafeno.exe` en Windows), así que requieren la compilación del servidor
anterior (o `GRAFENO_BIN`). Usa los comandos de Cargo con `--locked` para
detectar cambios inesperados de dependencias.

## Construir los instaladores

Consulta [los instaladores](docs/install.md#building-the-installers) para el
instalador de Windows, los paquetes de Linux y el instalador de una línea, y
cómo se prueba cada uno en máquinas limpias.

## Comprobaciones de cada contribución

El workflow de [comprobaciones de contribuciones](.github/workflows/ci.yml) se
ejecuta en los pull requests, en los pushes a `main` y manualmente. Así los
pushes a una rama con un PR abierto no duplican las ejecuciones. Comprueba por
separado:

- Pruebas Rust del motor y del servidor.
- El SDK de TypeScript (de grafeno-sdk) compilado y probado contra el servidor
  recién compilado, y su contrato con el último release del motor.

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
