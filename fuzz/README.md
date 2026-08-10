# Fuzzing

Los targets ejercitan las dos entradas públicas que aceptan bytes PDF no
confiables. Requieren `cargo-fuzz` y una toolchain nightly:

```sh
cargo install cargo-fuzz --locked
cargo +nightly fuzz run analyze
cargo +nightly fuzz run compress -- -max_len=4194304
```

Todo crash minimizado debe convertirse además en un fixture de regresión de
`gema-core`. Los corpus y artefactos locales no se versionan.
