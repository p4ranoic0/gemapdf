use gema_core::{compress as core_compress, CompressOptions, Profile};
use wasm_bindgen::prelude::*;

/// Comprime un PDF. `profile`: "screen" | "ebook" | "printer".
/// Devuelve los bytes del PDF optimizado.
#[wasm_bindgen]
pub fn compress(input: &[u8], profile: &str) -> Result<Vec<u8>, JsError> {
    let profile = match profile {
        "screen" => Profile::Screen,
        "ebook" => Profile::Ebook,
        "printer" => Profile::Printer,
        _ => return Err(JsError::new("perfil desconocido (usa screen|ebook|printer)")),
    };
    let opts = CompressOptions { profile, ..Default::default() };
    let res = core_compress(input, &opts).map_err(|e| JsError::new(&e.to_string()))?;
    Ok(res.output)
}

#[cfg(test)]
mod tests {
    use gema_core::Profile;

    /// Réplica del mapeo de `compress` para poder testearlo sin un PDF real.
    fn map(profile: &str) -> Option<Profile> {
        match profile {
            "screen" => Some(Profile::Screen),
            "ebook" => Some(Profile::Ebook),
            "printer" => Some(Profile::Printer),
            _ => None,
        }
    }

    #[test]
    fn maps_profile_strings() {
        assert_eq!(map("screen"), Some(Profile::Screen));
        assert_eq!(map("printer"), Some(Profile::Printer));
        assert_eq!(map("ebook"), Some(Profile::Ebook));
        // valor desconocido es un error explícito (no cae en un default)
        assert_eq!(map("otro"), None);
    }
}
