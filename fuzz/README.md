# Fuzzing

Los targets ejercitan las entradas públicas que aceptan bytes PDF no
confiables: `analyze`, `compress` y `compress_with_control` en
`gema-compress`, y `edit_text` (borrado y reemplazo de texto) en
`gema-edit`. Requieren `cargo-fuzz` y una toolchain nightly:

```sh
cargo install cargo-fuzz --locked
cargo +nightly fuzz run analyze
cargo +nightly fuzz run compress -- -max_len=4194304
cargo +nightly fuzz run edit_text
```

Sin semilla, el fuzzer gasta el tiempo descubriendo que sus bytes no son un
PDF. Sembrar con el fixture del repo antes de correr:

```sh
mkdir -p fuzz/corpus/edit_text
cp crates/gema-edit/tests/fixtures/editor-fuentes.pdf fuzz/corpus/edit_text/
```

CI corre los cuatro targets una vez por día (job `fuzz`, 120 s cada uno) y
sube como artefacto cualquier entrada que rompa.

Todo crash minimizado debe convertirse además en un fixture de regresión del
crate que corresponda (`gema-compress` o `gema-edit`). Los corpus y artefactos locales no se versionan.
