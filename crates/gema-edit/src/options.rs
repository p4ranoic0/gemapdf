//! Límites de trabajo de una edición.

/// Límites que acotan cuánto trabajo hace una llamada de edición.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditOptions {
    /// Tamaño máximo del PDF de entrada, en bytes.
    pub max_input_bytes: usize,
    /// Cantidad máxima de regiones por llamada.
    pub max_regions: usize,
    /// Bytes descomprimidos máximos del contenido de una página.
    pub max_decompressed_bytes: usize,
    /// Operadores máximos en el contenido de una página.
    pub max_content_operations: usize,
    /// Presupuesto de resolución de objetos.
    pub budget: ObjectBudget,
}

/// Presupuesto de resolución de objetos indirectos, compartido por toda la llamada.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectBudget {
    /// Streams que se abren (y descomprimen) como máximo.
    pub max_streams: usize,
    /// Saltos de referencia que se siguen para llegar a un objeto directo.
    pub max_reference_depth: usize,
    /// Objetos indirectos que se resuelven como máximo.
    pub max_inspected_objects: usize,
    /// Bytes descomprimidos máximos sumando todas las páginas de la llamada.
    pub max_total_decompressed_bytes: usize,
}

/// 256 MiB de entrada.
pub const DEFAULT_MAX_INPUT_BYTES: usize = 256 * 1024 * 1024;
/// Mil regiones por llamada.
pub const DEFAULT_MAX_REGIONS: usize = 1_000;
/// 64 MiB descomprimidos por página.
pub const DEFAULT_MAX_DECOMPRESSED_BYTES: usize = 64 * 1024 * 1024;
/// Un millón de operadores por página.
pub const DEFAULT_MAX_CONTENT_OPERATIONS: usize = 1_000_000;
/// 4096 streams por llamada.
pub const DEFAULT_MAX_STREAMS: usize = 4_096;
/// 32 saltos de referencia.
pub const DEFAULT_MAX_REFERENCE_DEPTH: usize = 32;
/// 65 536 objetos resueltos por llamada.
pub const DEFAULT_MAX_INSPECTED_OBJECTS: usize = 65_536;
/// 256 MiB descomprimidos en toda la llamada.
pub const DEFAULT_MAX_TOTAL_DECOMPRESSED_BYTES: usize = 256 * 1024 * 1024;

impl Default for EditOptions {
    fn default() -> Self {
        EditOptions {
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            max_regions: DEFAULT_MAX_REGIONS,
            max_decompressed_bytes: DEFAULT_MAX_DECOMPRESSED_BYTES,
            max_content_operations: DEFAULT_MAX_CONTENT_OPERATIONS,
            budget: ObjectBudget::default(),
        }
    }
}

impl Default for ObjectBudget {
    fn default() -> Self {
        ObjectBudget {
            max_streams: DEFAULT_MAX_STREAMS,
            max_reference_depth: DEFAULT_MAX_REFERENCE_DEPTH,
            max_inspected_objects: DEFAULT_MAX_INSPECTED_OBJECTS,
            max_total_decompressed_bytes: DEFAULT_MAX_TOTAL_DECOMPRESSED_BYTES,
        }
    }
}

/// Contador vivo de un presupuesto. Interno: el llamador sólo ve los límites.
#[allow(dead_code)] // se consume desde la Task 4
#[derive(Debug)]
pub(crate) struct BudgetMeter {
    limits: ObjectBudget,
    streams: usize,
    objects: usize,
    bytes: usize,
}

#[allow(dead_code)] // se consume desde la Task 4
impl BudgetMeter {
    pub(crate) fn new(limits: &ObjectBudget) -> Self {
        BudgetMeter {
            limits: limits.clone(),
            streams: 0,
            objects: 0,
            bytes: 0,
        }
    }

    /// Bytes descomprimidos que todavía caben en la llamada.
    pub(crate) fn remaining_bytes(&self) -> usize {
        self.limits
            .max_total_decompressed_bytes
            .saturating_sub(self.bytes)
    }

    /// Cobra `n` bytes descomprimidos. `Err` sin consumir si no caben.
    pub(crate) fn charge_bytes(&mut self, n: usize) -> Result<(), crate::LimitKind> {
        if n > self.remaining_bytes() {
            return Err(crate::LimitKind::TotalDecompressedBytes);
        }
        self.bytes += n;
        Ok(())
    }

    /// Un stream más. `Err` si ya se abrieron `max_streams`.
    pub(crate) fn open_stream(&mut self) -> Result<(), crate::LimitKind> {
        if self.streams >= self.limits.max_streams {
            return Err(crate::LimitKind::Streams);
        }
        self.streams += 1;
        Ok(())
    }

    /// Un objeto indirecto más. `Err` si ya se resolvieron `max_inspected_objects`.
    pub(crate) fn touch_object(&mut self) -> Result<(), crate::LimitKind> {
        if self.objects >= self.limits.max_inspected_objects {
            return Err(crate::LimitKind::InspectedObjects);
        }
        self.objects += 1;
        Ok(())
    }

    /// `Err` si `depth` supera `max_reference_depth`.
    pub(crate) fn check_depth(&self, depth: usize) -> Result<(), crate::LimitKind> {
        if depth > self.limits.max_reference_depth {
            return Err(crate::LimitKind::ReferenceDepth);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LimitKind;

    #[test]
    fn meter_accumulates_decompressed_bytes_across_the_call() {
        let budget = ObjectBudget {
            max_total_decompressed_bytes: 10,
            ..ObjectBudget::default()
        };
        let mut meter = BudgetMeter::new(&budget);
        assert_eq!(meter.remaining_bytes(), 10);
        assert_eq!(meter.charge_bytes(6), Ok(()));
        assert_eq!(meter.remaining_bytes(), 4);
        assert_eq!(
            meter.charge_bytes(5),
            Err(LimitKind::TotalDecompressedBytes)
        );
        assert_eq!(meter.remaining_bytes(), 4);
        assert_eq!(
            meter.charge_bytes(usize::MAX),
            Err(LimitKind::TotalDecompressedBytes)
        );
    }

    #[test]
    fn meter_refuses_the_stream_after_the_budget() {
        let budget = ObjectBudget {
            max_streams: 2,
            ..ObjectBudget::default()
        };
        let mut meter = BudgetMeter::new(&budget);
        assert_eq!(meter.open_stream(), Ok(()));
        assert_eq!(meter.open_stream(), Ok(()));
        assert_eq!(meter.open_stream(), Err(LimitKind::Streams));
    }

    #[test]
    fn meter_counts_objects_and_depth() {
        let budget = ObjectBudget {
            max_inspected_objects: 1,
            max_reference_depth: 3,
            ..ObjectBudget::default()
        };
        let mut meter = BudgetMeter::new(&budget);
        assert_eq!(meter.touch_object(), Ok(()));
        assert_eq!(meter.touch_object(), Err(LimitKind::InspectedObjects));
        assert_eq!(meter.check_depth(3), Ok(()));
        assert_eq!(meter.check_depth(4), Err(LimitKind::ReferenceDepth));
    }
}
