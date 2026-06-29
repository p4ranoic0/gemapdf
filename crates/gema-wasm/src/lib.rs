use gema_core::{compress as core_compress, CompressOptions, Profile};
use wasm_bindgen::prelude::*;

/// Comprime un PDF. `profile`: "screen" | "ebook" | "printer".
/// Devuelve los bytes del PDF optimizado.
#[wasm_bindgen]
pub fn compress(input: &[u8], profile: &str) -> Result<Vec<u8>, JsError> {
    let profile = match profile {
        "screen" => Profile::Screen,
        "printer" => Profile::Printer,
        _ => Profile::Ebook,
    };
    let opts = CompressOptions { profile, ..Default::default() };
    let res = core_compress(input, &opts).map_err(|e| JsError::new(&e.to_string()))?;
    Ok(res.output)
}

#[cfg(test)]
mod tests {
    use gema_core::Profile;

    /// Réplica del mapeo de `compress` para poder testearlo sin un PDF real.
    fn map(profile: &str) -> Profile {
        match profile {
            "screen" => Profile::Screen,
            "printer" => Profile::Printer,
            _ => Profile::Ebook,
        }
    }

    #[test]
    fn maps_profile_strings() {
        assert_eq!(map("screen"), Profile::Screen);
        assert_eq!(map("printer"), Profile::Printer);
        assert_eq!(map("ebook"), Profile::Ebook);
        // valor desconocido cae en el default (Ebook)
        assert_eq!(map("otro"), Profile::Ebook);
    }
}
