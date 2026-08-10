/// Preajuste de compresión. Fija el DPI objetivo y la calidad JPEG base; los
/// campos homónimos de [`CompressOptions`] los sobreescriben.
///
/// El set de variantes es cerrado a propósito: [`Profile::Custom`] es el punto
/// de extensión para llamadores que quieran fijar sus propios valores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Profile {
    /// Lectura en pantalla: 72 dpi, calidad 40.
    Screen,
    /// Default del producto: 150 dpi, calidad 65.
    Ebook,
    /// Impresión: 300 dpi, calidad 80.
    Printer,
    /// Usa valores base tipo Ebook; pensado para sobreescribirse vía
    /// `image_dpi`/`jpeg_quality` en `CompressOptions`.
    Custom,
}

impl std::fmt::Display for Profile {
    /// Nombre canónico del perfil; es la entrada que acepta
    /// [`Profile::from_str`], de modo que `parse` y `to_string` son inversos.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Profile::Screen => "screen",
            Profile::Ebook => "ebook",
            Profile::Printer => "printer",
            Profile::Custom => "custom",
        })
    }
}

impl std::str::FromStr for Profile {
    type Err = String;

    /// Acepta los nombres canónicos en minúsculas. Existe para que la CLI y el
    /// binding WASM no repitan el mapeo ni el texto del error.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "screen" => Ok(Profile::Screen),
            "ebook" => Ok(Profile::Ebook),
            "printer" => Ok(Profile::Printer),
            "custom" => Ok(Profile::Custom),
            other => Err(format!(
                "perfil desconocido: {other} (usa screen|ebook|printer|custom)"
            )),
        }
    }
}

/// Qué hacer cuando el documento trae una firma criptográfica.
///
/// El set de variantes es cerrado a propósito: los consumidores hacen `match`
/// exhaustivo para explicarle al usuario qué se conservó y qué no.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SignaturePolicy {
    /// Si el documento está firmado criptográficamente, no se recomprime nada.
    Strict,
    /// Comprime de todos modos (rompe la firma criptográfica).
    Ignore,
    /// Aplana las firmas/sellos visibles al contenido de página y comprime.
    /// Universalmente visible (Acrobat OK); sacrifica la validez criptográfica
    /// (que la compresión ya rompe).
    Flatten,
}

impl std::fmt::Display for SignaturePolicy {
    /// Nombre canónico de la política; inverso de
    /// [`SignaturePolicy::from_str`].
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SignaturePolicy::Strict => "strict",
            SignaturePolicy::Ignore => "ignore",
            SignaturePolicy::Flatten => "flatten",
        })
    }
}

impl std::str::FromStr for SignaturePolicy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "strict" => Ok(SignaturePolicy::Strict),
            "ignore" => Ok(SignaturePolicy::Ignore),
            "flatten" => Ok(SignaturePolicy::Flatten),
            other => Err(format!(
                "política de firmas desconocida: {other} (usa strict|ignore|flatten)"
            )),
        }
    }
}

/// Parámetros resueltos de un perfil.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfileParams {
    /// DPI objetivo del downsampling de imágenes.
    pub image_dpi: u32,
    /// Calidad JPEG (1-100) del path de recompresión con pérdida.
    pub jpeg_quality: u8,
}

/// Configuración de una compresión.
///
/// Se construye con literal de struct sobre [`CompressOptions::default`]; a
/// diferencia de los tipos de reporte, es deliberadamente exhaustiva porque el
/// llamador la arma. Añadir un campo es un cambio incompatible y se trata como
/// tal en el versionado.
#[derive(Debug, Clone)]
pub struct CompressOptions {
    /// Preajuste base de DPI y calidad.
    pub profile: Profile,
    /// Sobreescribe el DPI objetivo del perfil. `None` = usar el del perfil.
    pub image_dpi: Option<u32>,
    /// Sobreescribe la calidad JPEG del perfil. `None` = usar la del perfil.
    pub jpeg_quality: Option<u8>,
    /// Modo perceptual opt-in (spec 2026-07-11): si es `Some(τ)` con τ∈(0,100),
    /// cada imagen JPEG busca la MENOR q cuyo SSIMULACRA2 ≥ τ, en vez de usar
    /// la q fija del perfil (`jpeg_quality` se ignora para el path JPEG).
    /// Requiere el cargo feature `perceptual`; sin él, degrada a q fija con un
    /// warning en el reporte. `None` (default) = comportamiento clásico.
    pub quality_target: Option<f32>,
    /// DPI objetivo SOLO para imágenes que llegan sin pérdida (raster en Flate)
    /// y se transcodifican a JPEG. `None` = usar `image_dpi`/perfil.
    ///
    /// Existe porque son dos regímenes distintos, medido el 2026-07-26: la
    /// fuente de primera generación está intacta, así que rinde más gastar
    /// bytes en resolución que en cuantización; una que YA venía en JPEG carga
    /// artefactos de anillo que la cuantización extra compone. Sobre
    /// `doc-B2`, 110/q30 se ve mejor que 90/q65 y pesa medio mega
    /// menos; sobre `doc-A` (ya en JPEG) el mismo cambio sólo agrega 9.6% sin
    /// ganancia visible.
    pub transcode_dpi: Option<u32>,
    /// Calidad JPEG SOLO para transcodificados de primera generación (fuente
    /// sin pérdida). `None` = usar `jpeg_quality`/perfil. Va de la mano de
    /// [`CompressOptions::transcode_dpi`]: la asignación medida gasta los bytes
    /// en resolución y afloja la cuantización.
    pub transcode_quality: Option<u8>,
    /// Presupuesto aproximado para el trabajo de imágenes que puede estar vivo
    /// al mismo tiempo. El pipeline forma lotes ordenados según dimensiones y
    /// modo de encoding; no cambia la salida, sólo cuántas imágenes prepara en
    /// paralelo. `None` desactiva el presupuesto (default, para conservar el
    /// rendimiento nativo histórico). Servidores deberían fijarlo según su RAM.
    ///
    /// Es un límite de planificación, no un tope exacto del allocator: crates
    /// de codecs pueden hacer buffers internos que GemaPDF no controla.
    pub max_memory_bytes: Option<u64>,
    /// Máximo de imágenes preparadas simultáneamente en builds nativos.
    /// `None` deja que el presupuesto de memoria y Rayon determinen el límite.
    /// `Some(0)` se normaliza a una imagen.
    pub max_parallel_images: Option<usize>,
    /// Máximo estimado de bytes decodificados para una sola imagen. Las que lo
    /// superan se preservan sin recomprimir y se reportan como omitidas.
    /// `None` conserva el límite interno histórico del decoder.
    pub max_image_bytes: Option<u64>,
    /// Reduce la resolución de las imágenes cuyo DPI efectivo en página supera
    /// el objetivo del perfil.
    pub downsample: bool,
    /// Recomprime los streams no-imagen del documento (`reflate_streams`).
    pub recompress_streams: bool,
    /// Elimina metadata del documento (`/Info`, `/Metadata`) en la reescritura.
    pub remove_metadata: bool,
    /// Deduplica XObjects de imagen byte-idénticos con semántica de render
    /// equivalente. Excluye firmas/sellos preservados y máscaras de
    /// transparencia. Es opt-in para medir su beneficio sobre cada corpus.
    pub dedupe_images: bool,
    /// Qué hacer si el documento está firmado criptográficamente.
    pub signatures: SignaturePolicy,
}

impl Default for CompressOptions {
    fn default() -> Self {
        Self {
            profile: Profile::Ebook,
            image_dpi: None,
            jpeg_quality: None,
            quality_target: None,
            transcode_dpi: None,
            transcode_quality: None,
            max_memory_bytes: None,
            max_parallel_images: None,
            max_image_bytes: None,
            downsample: true,
            recompress_streams: true,
            remove_metadata: true,
            dedupe_images: false,
            // Política de producto: conservar la apariencia visible de firmas
            // y sellos dentro del PDF comprimido. La validez criptográfica se
            // pierde porque el documento cambia; `Strict` es opt-in.
            signatures: SignaturePolicy::Flatten,
        }
    }
}

impl CompressOptions {
    /// Resuelve DPI y calidad finales (override > perfil).
    pub fn resolved(&self) -> ProfileParams {
        let base = match self.profile {
            Profile::Screen => ProfileParams {
                image_dpi: 72,
                jpeg_quality: 40,
            },
            Profile::Ebook => ProfileParams {
                image_dpi: 150,
                jpeg_quality: 65,
            },
            Profile::Printer => ProfileParams {
                image_dpi: 300,
                jpeg_quality: 80,
            },
            Profile::Custom => ProfileParams {
                image_dpi: 150,
                jpeg_quality: 65,
            },
        };
        ProfileParams {
            image_dpi: self.image_dpi.unwrap_or(base.image_dpi),
            jpeg_quality: self.jpeg_quality.unwrap_or(base.jpeg_quality),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ebook_defaults() {
        let opts = CompressOptions::default();
        let p = opts.resolved();
        assert_eq!(p.image_dpi, 150);
        assert_eq!(p.jpeg_quality, 65);
        assert_eq!(opts.signatures, SignaturePolicy::Flatten);
    }
    #[test]
    fn override_wins_over_profile() {
        let opts = CompressOptions {
            profile: Profile::Screen,
            image_dpi: Some(120),
            ..Default::default()
        };
        assert_eq!(opts.resolved().image_dpi, 120);
        assert_eq!(opts.resolved().jpeg_quality, 40); // del perfil Screen
    }
    #[test]
    fn quality_target_defaults_to_none_and_is_settable() {
        assert!(
            CompressOptions::default().quality_target.is_none(),
            "el modo perceptual debe ser opt-in"
        );
        let opts = CompressOptions {
            quality_target: Some(68.0),
            ..Default::default()
        };
        assert_eq!(opts.quality_target, Some(68.0));
        // el resolved() de perfil no se ve afectado por el target
        assert_eq!(opts.resolved().jpeg_quality, 65);
    }

    #[test]
    fn profile_names_round_trip() {
        for profile in [
            Profile::Screen,
            Profile::Ebook,
            Profile::Printer,
            Profile::Custom,
        ] {
            let name = profile.to_string();
            assert_eq!(
                name.parse::<Profile>(),
                Ok(profile),
                "`{name}` debe volver al mismo perfil"
            );
        }
    }

    #[test]
    fn unknown_profile_name_lists_the_accepted_ones() {
        let err = "otro".parse::<Profile>().unwrap_err();
        assert!(err.contains("otro"), "{err}");
        assert!(err.contains("screen|ebook|printer|custom"), "{err}");
    }

    #[test]
    fn signature_policy_names_round_trip() {
        for policy in [
            SignaturePolicy::Strict,
            SignaturePolicy::Ignore,
            SignaturePolicy::Flatten,
        ] {
            let name = policy.to_string();
            assert_eq!(name.parse::<SignaturePolicy>(), Ok(policy), "{name}");
        }
        assert!("otro".parse::<SignaturePolicy>().is_err());
    }

    #[test]
    fn memory_limits_are_opt_in_and_do_not_skip_images_by_default() {
        let opts = CompressOptions::default();
        assert_eq!(opts.max_memory_bytes, None);
        assert_eq!(opts.max_parallel_images, None);
        assert_eq!(opts.max_image_bytes, None);
    }
}
