//! Tests del pipeline de compresión, agrupados por dominio.
//!
//! Los constructores de PDF sintéticos viven en `fixtures`; cada módulo de
//! dominio los importa con `use super::fixtures::*`.

mod fixtures;

mod compression;
mod limits;
mod masks;
mod memory;
mod progress;
mod signatures;
