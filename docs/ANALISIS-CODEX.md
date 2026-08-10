# Análisis del proyecto con Codex

**Proyecto:** GemaPDF  
**Fecha del análisis:** 2026-08-01  
**Alcance:** estructura del workspace, API pública, documentación, pruebas,
CI, empaquetado, seguridad y mantenibilidad.

## Resumen ejecutivo

GemaPDF tiene una base estructural saludable. El workspace separa correctamente
el motor (`gema-core`), la interfaz de línea de comandos (`gema-cli`) y los
bindings WebAssembly (`gema-wasm`). El núcleo contiene una cantidad considerable
de pruebas y distingue bien entre el procesamiento de PDF, la optimización de
imágenes, la reescritura y las capas de adaptación.

Durante la revisión se ejecutaron satisfactoriamente:

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

El resultado fue de **168 pruebas aprobadas**, sin fallos de formato ni de
Clippy. Por tanto, las oportunidades principales no son consecuencia de una
base rota, sino de inconsistencias entre comportamiento y documentación,
problemas de distribución y algunos riesgos futuros de mantenimiento.

Las tres acciones más importantes son:

1. Unificar la política predeterminada para documentos firmados y documentarla
   correctamente.
2. Reparar el empaquetado de `gema-cli` y `gema-wasm`.
3. Convertir la restricción de licencias sin AGPL en una verificación automática
   de CI.

## Estado de implementación

Las recomendaciones de contrato, empaquetado, licencias, reproducibilidad,
pruebas de frontera, API pública, limpieza del paquete y base de fuzzing fueron
implementadas el 2026-08-01. Los apartados siguientes se conservan como registro
del diagnóstico original y de sus criterios de aceptación.

La decisión de producto confirmada es usar `SignaturePolicy::Flatten` por
defecto: la apariencia de firmas y sellos debe sobrevivir en el PDF comprimido,
aunque la validez criptográfica se pierda. `Strict` queda disponible de forma
explícita para devolver documentos firmados sin modificarlos.

Permanecen como trabajo de diseño futuro los presupuestos configurables de
memoria/trabajo y el control de concurrencia basado en memoria estimada; no se
introdujeron límites arbitrarios sin medirlos contra el corpus real.

## Estructura actual

```text
gemapdf/
├── Cargo.toml
├── Cargo.lock
├── README.md
├── TODO-v2.md
├── crates/
│   ├── gema-core/
│   │   ├── src/
│   │   │   ├── image_opt/
│   │   │   ├── pipeline.rs
│   │   │   ├── rewrite.rs
│   │   │   ├── signatures.rs
│   │   │   └── ...
│   │   ├── tests/
│   │   └── examples/
│   ├── gema-cli/
│   └── gema-wasm/
├── docs/
├── hooks/
├── scripts/
└── .github/workflows/
```

### Aspectos positivos

- La separación por crates mantiene el motor independiente de CLI y navegador.
- `gema-core` evita dependencias de UI o plataforma en su API principal.
- El procesamiento por imagen está dividido en etapas de carga, decodificación,
  transformación, codificación y escritura.
- La mutación del documento se mantiene separada del trabajo paralelizable.
- Las dependencias pesadas del modo perceptual son opcionales y quedan fuera
  del grafo WASM.
- Hay pruebas unitarias, de integración y de caracterización de uso.
- CI comprueba formato, Clippy, tests y construcción WASM.
- El repositorio mantiene `target` y los paquetes WASM generados fuera de Git.

## Hallazgos y oportunidades de mejora

## P0 — Alinear la política predeterminada de firmas

### Situación

Durante el análisis se detectó que la documentación y el comportamiento no
expresaban un contrato único. La política de producto fue aclarada posteriormente:

```rust
signatures: SignaturePolicy::Flatten,
```

Core, CLI y WASM usan ahora `Flatten` cuando no se proporciona una política.

### Riesgo

`Flatten` modifica el documento y sacrifica la validez criptográfica, pero
conserva la apariencia visible como contenido de página. Esto debe permanecer
explícito para que ningún consumidor confunda preservación visual con validez
criptográfica.

### Decisión adoptada

- **Default:** `Flatten`, porque el requisito principal es que firmas y sellos
  continúen visibles en el resultado comprimido.
- **Opt-in:** `Strict`, cuando conservar la validez criptográfica sea prioritario;
  en ese caso no se modifica ni comprime el PDF firmado.
- **Avanzado:** `Ignore`, que permite modificar el documento sin aplanar los
  widgets y no garantiza su apariencia en todos los visores.

### Implementación realizada

1. `CompressOptions::default()` usa `Flatten`.
2. La ausencia de `signatures` en WASM se traduce a `Flatten`.
3. La CLI expone:

   ```text
   --signatures strict|ignore|flatten
   ```

4. README y changelog distinguen preservación visual de validez criptográfica.
5. Las pruebas fijan el default y mantienen cobertura explícita de `Strict`.

### Criterios de aceptación

- El README y el código describen el mismo valor predeterminado.
- Core, CLI y WASM tienen defaults deliberados y cubiertos por pruebas.
- `gema --help` explica si una política invalida la firma criptográfica.
- Existe una prueba end-to-end con un documento marcado como firmado.

## P0 — Reparar el empaquetado de los crates

### Situación

Los siguientes comandos fallan:

```sh
cargo package -p gema-cli --allow-dirty --no-verify
cargo package -p gema-wasm --allow-dirty --no-verify
```

Cargo informa que la dependencia `gema-core` no tiene un requisito de versión.
Los manifests contienen solamente una ruta local:

```toml
gema-core = { path = "../gema-core" }
```

### Riesgo

Los manifests aparentan estar preparados para publicación, pero los paquetes no
se pueden construir para un registry. Esto también dificulta automatizar releases
o verificar que los artefactos publicados sean reproducibles.

### Implementación sugerida

Si los crates se publicarán:

```toml
gema-core = { version = "0.3.0", path = "../gema-core" }
```

En `gema-cli` se conservaría además:

```toml
features = ["perceptual"]
```

Para evitar repetir la versión puede declararse la dependencia interna en
`[workspace.dependencies]` y heredarla desde cada crate.

Si un crate no debe publicarse, declararlo explícitamente:

```toml
publish = false
```

También conviene completar metadatos de paquete cuando corresponda:

- `readme`;
- `rust-version`;
- `documentation` o `homepage`;
- autores, si se desean en el registro.

### Criterios de aceptación

```sh
cargo package -p gema-core
cargo package -p gema-cli
cargo package -p gema-wasm
```

Todos los crates publicables deben empaquetarse y verificarse correctamente.
Los no publicables deben fallar de forma intencional mediante `publish = false`.

## P0 — Verificar automáticamente las licencias

### Situación

La ausencia de AGPL es una promesa central del proyecto, pero no existe una
política automatizada que inspeccione todas las licencias del grafo de
dependencias. CI comprueba que `ssimulacra2` no entre en el árbol de
`gema-wasm`, lo cual protege el tamaño y la separación de features, pero no
garantiza por sí solo la política completa de licencias.

### Implementación sugerida

1. Incorporar `cargo-deny`.
2. Crear `deny.toml` con una lista explícita de licencias permitidas.
3. Revisar por separado dependencias opcionales, de desarrollo y por target.
4. Bloquear fuentes Git o registries desconocidos si no son necesarias.
5. Añadir a CI:

   ```sh
   cargo deny check licenses bans sources
   ```

6. Opcionalmente ejecutar `cargo audit` para vulnerabilidades conocidas.

### Criterios de aceptación

- Una dependencia con licencia no permitida hace fallar CI.
- La política incluye el grafo nativo, perceptual y WASM.
- Las excepciones, si existen, tienen justificación y alcance concreto.

## P1 — Reducir la superficie pública de `gema-core`

### Situación

`gema-core/src/lib.rs` declara como públicos varios módulos completos:

```rust
pub mod analyze;
pub mod image_opt;
pub mod rewrite;
pub mod progress;
pub mod pipeline;
```

Esto expone funciones de bajo nivel como reescritura, serialización y tipos del
pipeline de imagen. Cada elemento público pasa a ser parte potencial del contrato
de compatibilidad semántica del crate.

### Riesgo

- Refactors internos pueden convertirse en breaking changes.
- Los consumidores pueden depender accidentalmente de funciones no diseñadas
  como API estable.
- La documentación pública queda más extensa y ambigua.

### Implementación sugerida

Mantener privados los módulos internos y reexportar solo la API soportada:

```rust
mod analyze;
mod pipeline;
mod rewrite;
mod image_opt;

pub use analyze::analyze;
pub use pipeline::{compress, compress_with_progress, CompressResult};
```

La API pública inicial podría limitarse a:

- `compress`;
- `compress_with_progress`;
- `analyze`;
- `CompressOptions` y tipos relacionados;
- `CompressResult`, `Report`, `Warning` y `Phase`.

Si la optimización de imágenes debe ser extensible, conviene diseñar una API
específica para extensiones en vez de exponer todo `image_opt`.

### Criterios de aceptación

- `cargo doc --no-deps` muestra solo la API que se desea soportar.
- Los ejemplos del README siguen compilando.
- Las funciones internas pueden moverse sin romper consumidores.
- Opcionalmente se activa `#![warn(missing_docs)]` para elementos públicos.

## P1 — Resolver el desfase de documentación y roadmap

### Situación

`TODO-v2.md` mantiene como pendientes funcionalidades que ya existen:

- selección de calidad perceptual;
- procesamiento paralelo mediante Rayon.

El README también enumera la selección perceptual entre las funciones aún no
implementadas, aunque el crate ya contiene el feature `perceptual`, su módulo,
pruebas y soporte CLI.

### Riesgo

- Una persona nueva no puede determinar con confianza qué está implementado.
- Se pueden planificar de nuevo trabajos ya terminados.
- Las promesas de producto pueden no corresponder a la versión publicada.

### Implementación sugerida

Separar los documentos por función:

- `README.md`: comportamiento de la versión actual.
- `CHANGELOG.md`: funcionalidades entregadas por versión.
- `docs/ROADMAP.md`: trabajo futuro real y decisiones pendientes.
- `TODO-v2.md`: eliminarlo, archivarlo o convertirlo en una lista sincronizada.
- Análisis de corpus: mantenerlos como registros históricos con versión,
  fecha, commit, parámetros y limitaciones.

### Criterios de aceptación

- Ninguna función entregada aparece bajo una sección `Remaining`.
- Las capacidades experimentales están marcadas como tales.
- Las cifras de compresión indican versión y corpus.
- Existe una única fuente principal para el estado del roadmap.

## P1 — Probar las fronteras reales del producto

### Situación

La cobertura interna del core es buena, pero las interfaces de usuario tienen
menos cobertura:

- `gema-cli` ejecuta cero pruebas.
- Los tests de `gema-wasm` verifican mapeadores en host, pero no ejercitan las
  exportaciones en un runtime WebAssembly.
- CI construye el paquete con `wasm-pack`, pero no llama la API generada desde
  JavaScript.

### Implementación sugerida

#### CLI

Agregar tests de integración con `assert_cmd`, `predicates` y `tempfile`:

- `gema analyze fixture.pdf`;
- `gema compress input.pdf output.pdf`;
- perfil desconocido;
- archivo inexistente;
- opciones de calidad fuera de rango;
- política de firma;
- el output existe y vuelve a parsearse.

Para facilitarlo, puede moverse el parsing y la ejecución a `gema-cli/src/lib.rs`,
dejando `main.rs` como adaptador pequeño.

#### WASM

Agregar `wasm-bindgen-test` o un smoke test sobre el paquete generado:

- invocar `analyze`;
- invocar `compress`;
- validar errores JS;
- validar la forma de `report`;
- verificar el orden y throttling del callback de progreso;
- confirmar que el resultado sea un `Uint8Array` parseable.

### Criterios de aceptación

- CLI tiene pruebas de comandos exitosos y fallidos.
- La API WASM se ejecuta en al menos un runtime real en CI.
- Los contratos mostrados en el README están cubiertos por tests.

## P1 — Hacer reproducible el entorno de compilación

### Situación

El hook local y CI utilizan la toolchain `stable`, que cambia con el tiempo.
Además, CI instala `wasm-pack` sin fijar una versión. Los manifests no declaran
una versión mínima de Rust.

### Riesgo

- Una nueva versión de Rust puede introducir lints o cambios de formato sin que
  haya cambiado el repositorio.
- Una nueva versión de `wasm-pack` puede modificar o romper el artefacto.
- Los usuarios no saben qué MSRV soporta el proyecto.

### Implementación sugerida

1. Crear `rust-toolchain.toml` con canal y componentes concretos.
2. Declarar `rust-version` en `[workspace.package]` y heredarlo en los crates.
3. Instalar una versión específica de `wasm-pack`.
4. Añadir caché de Cargo a CI.
5. Si existe una política MSRV, probarla en un job separado.

Ejemplo conceptual:

```toml
[toolchain]
channel = "<versión acordada>"
components = ["rustfmt", "clippy"]
targets = ["wasm32-unknown-unknown"]
profile = "minimal"
```

### Criterios de aceptación

- Desarrollo local, hook y CI utilizan la misma toolchain.
- La versión de `wasm-pack` no flota.
- El MSRV está declarado y comprobado.
- Los jobs de CI reutilizan el caché sin comprometer verificaciones.

## P2 — Separar ejemplos públicos de herramientas experimentales

### Situación

`gema-core/examples` incluye nueve binarios, varios de ellos orientados a
diagnóstico, comparación de encoders o experimentación:

- `diag_buckets`;
- `diag_gray`;
- `enc_compare`;
- `enc_mozjpeg`;
- `mrc_spike`;
- `ssim2_cmp`;
- entre otros.

`cargo package -p gema-core --list` muestra que todos se incluyen en el paquete
de `gema-core`.

### Implementación sugerida

- Conservar en `examples/` solo programas pensados para usuarios del crate.
- Mover diagnósticos y experimentos a `tools/`, `experiments/` o un crate
  `xtask` excluido del workspace publicado.
- Alternativamente, usar `include`/`exclude` en el manifest si deben permanecer
  en el repositorio pero no en el paquete.
- Documentar en un README interno cómo ejecutar los experimentos que dependan de
  corpus externos.

### Criterios de aceptación

- `cargo package -p gema-core --list` contiene solo archivos necesarios para los
  consumidores.
- Los ejemplos publicados son estables, comprensibles y documentados.
- Las herramientas internas siguen siendo ejecutables desde el repositorio.

## P2 — Fuzzing y presupuestos de recursos

### Situación

El proyecto procesa PDFs potencialmente no confiables. Ya existen varias
defensas contra dimensiones malformadas, overflow y filtros corruptos. No
obstante, el límite de decodificación permite aproximadamente 805 MB por imagen,
y el procesamiento nativo puede preparar varias imágenes en paralelo.

### Riesgo

- Alto consumo de memoria en documentos construidos de forma adversarial.
- Multiplicación del consumo por concurrencia.
- Combinaciones de filtros o diccionarios PDF no cubiertas por fixtures manuales.

### Implementación sugerida

1. Añadir targets de `cargo-fuzz` para:

   - cadenas de filtros;
   - predictores PNG/TIFF;
   - interpretación de colorspaces;
   - análisis de documentos;
   - compresión y reparseo del resultado.

2. Definir un presupuesto total por documento, no solo por imagen.
3. Limitar la concurrencia por estimación de memoria o permitir configurarla.
4. Mantener los crashes minimizados como fixtures de regresión.
5. Considerar límites configurables para servidor, escritorio y navegador.

### Criterios de aceptación

- Entradas arbitrarias no provocan `panic` en los targets principales.
- Existe un límite documentado de memoria o trabajo por documento.
- La concurrencia no puede multiplicar el consumo sin control.
- Los hallazgos del fuzzer se convierten en pruebas permanentes.

## P2 — Mejorar la matriz de CI

### Observaciones

La configuración actual cubre los gates fundamentales, pero puede hacerse más
precisa:

- `cargo test --workspace` puede activar el feature perceptual a través de
  `gema-cli`, mientras que el segundo test vuelve a probar explícitamente ese
  feature.
- Conviene probar de forma explícita `gema-core` sin features y con
  `perceptual`.
- El build WASM comprueba compilación, no ejecución.
- No hay verificación de empaquetado ni licencias.

### Matriz sugerida

```sh
cargo fmt --all -- --check
cargo clippy -p gema-core --all-targets --no-default-features -- -D warnings
cargo clippy -p gema-core --all-targets --features perceptual -- -D warnings
cargo clippy -p gema-cli -p gema-wasm --all-targets -- -D warnings
cargo test -p gema-core --no-default-features
cargo test -p gema-core --features perceptual
cargo test -p gema-cli
cargo test -p gema-wasm
cargo package -p gema-core
cargo package -p gema-cli
cargo package -p gema-wasm
cargo deny check licenses bans sources
```

A esto se sumarían el build WASM y su smoke test de JavaScript.

## Orden recomendado de implementación

### Fase 1 — Contrato y distribución

1. Decidir el default de firmas.
2. Alinear core, CLI, WASM y README.
3. Reparar los manifests para `cargo package` o marcar crates no publicables.
4. Actualizar `TODO-v2.md` y README para reflejar el estado real.

### Fase 2 — Automatización

1. Incorporar `cargo-deny`.
2. Fijar Rust y `wasm-pack`.
3. Mejorar la matriz CI.
4. Añadir verificación de paquetes.

### Fase 3 — Interfaces y mantenibilidad

1. Añadir tests de CLI.
2. Añadir tests WASM reales.
3. Reducir la superficie pública de `gema-core`.
4. Separar ejemplos de experimentos internos.

### Fase 4 — Robustez avanzada

1. Introducir fuzzing.
2. Definir presupuestos de memoria y trabajo.
3. Controlar la concurrencia en función del coste estimado.

## Checklist de seguimiento

- [x] Decidir y documentar el default de firmas.
- [x] Exponer la política de firmas en CLI.
- [x] Añadir tests del default en core, CLI y WASM.
- [x] Añadir versión a dependencias internas por `path`.
- [x] Conseguir que `cargo package` pase para `gema-core` y validar los
  manifests de sus consumidores antes de publicar el core en el registry.
- [x] Crear `deny.toml` e integrar `cargo-deny`.
- [x] Crear `rust-toolchain.toml`.
- [x] Declarar `rust-version`.
- [x] Fijar la versión de `wasm-pack`.
- [x] Actualizar README, TODO y roadmap.
- [x] Incorporar `CHANGELOG.md`.
- [x] Revisar y reducir la API pública de `gema-core`.
- [x] Añadir tests de integración del CLI.
- [x] Ejecutar la API WASM en un runtime real dentro de CI.
- [x] Excluir herramientas experimentales del paquete publicado.
- [x] Diseñar targets iniciales de fuzzing.
- [ ] Definir presupuestos de memoria y concurrencia.

## Comandos de verificación final

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p gema-core --all-targets --features perceptual -- -D warnings
cargo test -p gema-core --no-default-features
cargo test -p gema-core --features perceptual
cargo test -p gema-cli
cargo test -p gema-wasm
cargo package -p gema-core
cargo package -p gema-cli
cargo package -p gema-wasm
```

Cuando se incorporen las herramientas correspondientes:

```sh
cargo deny check licenses bans sources
cargo audit
```

## Conclusión

GemaPDF no necesita una reorganización completa. La división en tres crates y
la arquitectura interna del motor son razonables. Las mejoras más valiosas son
hacer explícito el contrato de seguridad, asegurar que la distribución funcione,
automatizar las restricciones que el proyecto promete y cubrir las interfaces
reales además de la lógica interna.

Una vez resueltos esos puntos, la reducción de API pública, la limpieza de
herramientas experimentales y el fuzzing ayudarán a que el proyecto pueda crecer
sin aumentar innecesariamente su superficie de mantenimiento o riesgo.
