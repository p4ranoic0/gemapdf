# gema-wasm — rama de distribución (generada)

**Esta rama NO tiene código fuente.** Es el paquete npm de GemaPDF: exactamente
la salida de `wasm-pack build --target web`, con el `package.json` en la raíz
para que se pueda instalar como dependencia de git.

El código fuente vive en `main` (Rust). Esta rama existe para que los
consumidores (p. ej. el portfolio) traten a GemaPDF como una librería de
terceros, sin copiar artefactos entre carpetas hermanas.

## Consumir

```json
{ "dependencies": { "gema-wasm": "github:p4ranoic0/gemapdf#wasm-v0.1.0" } }
```

```js
import init, { compress_with_report } from 'gema-wasm'
await init()
```

Pinear siempre un **tag** (`#wasm-v0.1.0`), no la rama: la rama se mueve en cada
release.

## Publicar un release nuevo

Desde la raíz del repo, en la rama fuente que se quiera liberar (normalmente
`main`, que es la que corre en producción):

```bash
cd crates/gema-wasm && wasm-pack build --target web && cd ../..
git worktree add -B npm /tmp/gema-npm main
cd /tmp/gema-npm
git rm -rq .
cp ../../crates/gema-wasm/pkg/{gema_wasm.js,gema_wasm.d.ts,gema_wasm_bg.wasm,gema_wasm_bg.wasm.d.ts,package.json} .
# (volver a copiar este README)
git add -A && git commit --no-verify -m "release: gema-wasm vX.Y.Z desde <sha>"
git tag wasm-vX.Y.Z && git push origin npm --force && git push origin wasm-vX.Y.Z
cd - && git worktree remove /tmp/gema-npm
```

El commit usa `--no-verify` a propósito: el hook `pre-commit` corre gates de
Rust (fmt/tests/clippy) que no aplican en una rama sin Cargo.toml.
