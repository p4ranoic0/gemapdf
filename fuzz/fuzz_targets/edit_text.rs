#![no_main]

use gema_edit::{
    remove_text_glyphs_with, replace_text_glyphs, EditOptions, TextRegion, TextReplacement,
};
use libfuzzer_sys::fuzz_target;

fn input_byte(data: &[u8], index: usize) -> u8 {
    data.get(index).copied().unwrap_or(0)
}

// Mismo patrón que `compress_with_control`: los primeros bytes gobiernan la
// llamada y el buffer entero se pasa igual como PDF. Así un corpus de PDFs
// reales sirve sin preparación y el fuzzer puede además dirigir la región.
fuzz_target!(|data: &[u8]| {
    let word =
        |offset| u16::from_le_bytes([input_byte(data, offset), input_byte(data, offset + 1)]);
    // Cuartos de punto: cubre coordenadas fraccionarias, que es donde vive la
    // aritmética de la matriz de texto.
    let coord = |offset| f64::from(word(offset)) / 4.0;

    let region = TextRegion {
        id: String::from("fuzz"),
        page: u32::from(input_byte(data, 0)),
        x: coord(1),
        y: coord(3),
        // Ancho y alto deben ser mayores que cero; el cero se prueba aparte,
        // abajo, para no perder el camino de validación.
        width: coord(5).max(0.25),
        height: coord(7).max(0.25),
    };

    let opts = EditOptions {
        max_scan_pages: usize::from(input_byte(data, 9)),
        max_content_operations: usize::from(word(10)) * 16,
        max_width_delta_em: f64::from(input_byte(data, 12)) / 100.0,
        ..Default::default()
    };

    let _ = remove_text_glyphs_with(data, &[region.clone()], &opts);

    // El texto nuevo sale de los propios bytes de entrada, mapeados a ASCII
    // imprimible: el fuzzer lo dirige igual que al resto, y el camino que
    // importa —qué códigos de la fuente son reusables— depende de estos
    // caracteres, no de un literal fijo.
    let new_text: String = data
        .iter()
        .skip(13)
        .take(16)
        .map(|b| char::from(32 + b % 95))
        .collect();

    let expected_text = if input_byte(data, 12) % 2 == 0 {
        None
    } else {
        // La red de seguridad del llamador: pasar un esperado que casi nunca
        // coincide ejercita el camino de rechazo por selección obsoleta.
        Some(new_text.clone())
    };

    let _ = replace_text_glyphs(
        data,
        &[TextReplacement {
            region: region.clone(),
            new_text,
            expected_text,
        }],
        &opts,
    );

    // Región degenerada: ancho y alto en cero deben rechazarse sin pánico.
    let _ = remove_text_glyphs_with(
        data,
        &[TextRegion {
            width: 0.0,
            height: 0.0,
            ..region
        }],
        &opts,
    );
});
