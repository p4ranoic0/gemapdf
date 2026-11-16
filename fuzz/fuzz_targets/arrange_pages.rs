#![no_main]

use gema_edit::{arrange_pages, EditOptions, PageSpec, Rotation};
use libfuzzer_sys::fuzz_target;

// Mismo patrón que `edit_text`: los primeros bytes gobiernan el plan y el
// buffer entero se pasa igual como PDF, así el corpus de PDFs reales sirve sin
// preparación. Los índices pueden repetirse o salirse de rango a propósito:
// esos rechazos también tienen que salir sin pánico.
fuzz_target!(|data: &[u8]| {
    let byte = |i: usize| data.get(i).copied().unwrap_or(0);
    let len = usize::from(byte(0) % 9);
    let plan: Vec<PageSpec> = (0..len)
        .map(|i| PageSpec {
            source: u32::from(byte(1 + i) % 32),
            rotate: match byte(10 + i) % 4 {
                0 => Rotation::R0,
                1 => Rotation::R90,
                2 => Rotation::R180,
                _ => Rotation::R270,
            },
        })
        .collect();
    let _ = arrange_pages(data, &plan, &EditOptions::default());
});
