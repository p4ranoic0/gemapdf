//! Intérprete interno de texto PDF con geometría por glifo.
//!
//! Esta fase sólo observa. Si una fuente o un código no tiene métricas fiables,
//! registra el motivo y no inventa posiciones que G2 pudiera usar para borrar.

use std::collections::{HashMap, HashSet};

use lopdf::content::Content;
use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::matrix::Matrix;

#[path = "core14_metrics.rs"]
mod core14_metrics;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PageText {
    pub glyphs: Vec<Glyph>,
    pub unsupported: Vec<Unsupported>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Glyph {
    pub op_index: usize,
    pub operand_index: usize,
    pub byte_offset: usize,
    pub code: u32,
    pub code_len: u8,
    pub origin: (f64, f64),
    pub advance: f64,
    pub font_size_eff: f64,
    pub font_res_name: Vec<u8>,
    /// Modo de pintado (`Tr`). De 4 a 7 el glifo también recorta: quitarlo
    /// cambiaría cómo se ve el contenido posterior.
    pub render_mode: i64,
    /// Dirección unitaria de la línea base en espacio de página.
    pub baseline_dir: (f64, f64),
    /// Dirección unitaria "hacia arriba" del glifo en espacio de página.
    pub ascent_dir: (f64, f64),
    /// Número de `TJ` que desplaza exactamente lo mismo que este glifo, incluidos
    /// `Tc` y `Tw`. `None` si `Tfs·Th` es cero y no hay número equivalente.
    pub tj_adjustment: Option<f64>,
    /// Ancho declarado por la fuente, en unidades de milésimas de em.
    pub font_width: f64,
    /// Avance en el espacio de texto, antes de la matriz de página.
    pub text_advance: f64,
    /// Parámetros de texto necesarios para medir un código reutilizado.
    pub font_size: f64,
    pub horizontal_scale: f64,
    pub char_spacing: f64,
    pub word_spacing: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unsupported {
    pub op_index: usize,
    pub reason: UnsupportedReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UnsupportedReason {
    /// Una cadena anterior del mismo objeto de texto no se pudo medir, así que
    /// la posición de ésta ya no es fiable hasta el próximo `Tm`/`Td`/`T*`.
    UnknownPosition,
    NoMetrics {
        font: Vec<u8>,
    },
    Type3 {
        font: Vec<u8>,
    },
    CMap {
        name: Vec<u8>,
    },
    FormXObject {
        name: Vec<u8>,
    },
    FontNotFound {
        font: Vec<u8>,
    },
    MalformedText {
        font: Vec<u8>,
    },
    Encoding {
        name: Vec<u8>,
    },
    SymbolicCore14 {
        font: Vec<u8>,
    },
}

#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoreFont {
    Courier,
    CourierBold,
    CourierOblique,
    CourierBoldOblique,
    Helvetica,
    HelveticaBold,
    HelveticaOblique,
    HelveticaBoldOblique,
    TimesRoman,
    TimesBold,
    TimesItalic,
    TimesBoldItalic,
}

impl CoreFont {
    fn from_base_name(name: &[u8]) -> Option<Self> {
        match name {
            b"Courier" => Some(Self::Courier),
            b"Courier-Bold" => Some(Self::CourierBold),
            b"Courier-Oblique" => Some(Self::CourierOblique),
            b"Courier-BoldOblique" => Some(Self::CourierBoldOblique),
            b"Helvetica" => Some(Self::Helvetica),
            b"Helvetica-Bold" => Some(Self::HelveticaBold),
            b"Helvetica-Oblique" => Some(Self::HelveticaOblique),
            b"Helvetica-BoldOblique" => Some(Self::HelveticaBoldOblique),
            b"Times-Roman" => Some(Self::TimesRoman),
            b"Times-Bold" => Some(Self::TimesBold),
            b"Times-Italic" => Some(Self::TimesItalic),
            b"Times-BoldItalic" => Some(Self::TimesBoldItalic),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct SimpleEncoding {
    glyphs: [Option<u16>; 256],
}

impl SimpleEncoding {
    fn from_base(base: &[u16; 256]) -> Self {
        let mut glyphs = [None; 256];
        for (slot, &unicode) in glyphs.iter_mut().zip(base) {
            if unicode != 0 {
                *slot = Some(unicode);
            }
        }
        Self { glyphs }
    }

    fn glyph(&self, code: u8) -> Option<u16> {
        self.glyphs[usize::from(code)]
    }
}

#[derive(Debug, Clone)]
enum FontMetrics {
    Simple {
        first_char: i64,
        widths: Vec<f64>,
        missing_width: f64,
    },
    Type0 {
        widths: HashMap<u16, f64>,
        default_width: f64,
    },
    Core14 {
        font: CoreFont,
        encoding: Box<SimpleEncoding>,
    },
    Unsupported(UnsupportedReason),
}

impl FontMetrics {
    fn code_len(&self) -> usize {
        if matches!(self, Self::Type0 { .. }) {
            2
        } else {
            1
        }
    }

    fn width(&self, code: u32) -> Option<f64> {
        match self {
            Self::Simple {
                first_char,
                widths,
                missing_width,
            } => {
                let index = i64::from(code) - *first_char;
                if index >= 0 {
                    widths
                        .get(usize::try_from(index).ok()?)
                        .copied()
                        .or(Some(*missing_width))
                } else {
                    Some(*missing_width)
                }
            }
            Self::Type0 {
                widths,
                default_width,
            } => Some(
                u16::try_from(code)
                    .ok()
                    .and_then(|cid| widths.get(&cid).copied())
                    .unwrap_or(*default_width),
            ),
            Self::Core14 { font, encoding } => {
                let glyph = encoding.glyph(u8::try_from(code).ok()?)?;
                core14_metrics::width(*font, glyph)
            }
            Self::Unsupported(_) => None,
        }
    }
}

#[derive(Debug, Clone)]
struct TextParameters {
    font_name: Option<Vec<u8>>,
    font_size: f64,
    char_spacing: f64,
    word_spacing: f64,
    horizontal_scale: f64,
    leading: f64,
    rise: f64,
    render_mode: i64,
}

impl Default for TextParameters {
    fn default() -> Self {
        Self {
            font_name: None,
            font_size: 0.0,
            char_spacing: 0.0,
            word_spacing: 0.0,
            horizontal_scale: 1.0,
            leading: 0.0,
            rise: 0.0,
            render_mode: 0,
        }
    }
}

#[derive(Debug, Clone)]
struct TextState {
    params: TextParameters,
    matrix: Matrix,
    line_matrix: Matrix,
    active: bool,
    /// `false` desde que una cadena no se pudo medir: la matriz de texto no avanzó
    /// lo que debía. Vuelve a `true` con cualquier operador que parte de la
    /// matriz de línea (`BT`, `Tm`, `Td`, `TD`, `T*`, `'`, `"`).
    position_known: bool,
}

impl Default for TextState {
    fn default() -> Self {
        Self {
            params: TextParameters::default(),
            matrix: Matrix::IDENTITY,
            line_matrix: Matrix::IDENTITY,
            active: false,
            position_known: true,
        }
    }
}

fn number(object: &Object) -> Option<f64> {
    object.as_float().ok().map(f64::from)
}

fn resolved<'a>(doc: &'a Document, object: &'a Object) -> Option<&'a Object> {
    doc.dereference(object).ok().map(|(_, value)| value)
}

fn resolved_array<'a>(doc: &'a Document, object: &'a Object) -> Option<&'a [Object]> {
    resolved(doc, object)?.as_array().ok().map(Vec::as_slice)
}

fn resolved_dict<'a>(doc: &'a Document, object: &'a Object) -> Option<&'a Dictionary> {
    resolved(doc, object)?.as_dict().ok()
}

fn matrix_from_operands(operands: &[Object]) -> Option<Matrix> {
    if operands.len() < 6 {
        return None;
    }
    let values: Option<Vec<f32>> = operands[..6]
        .iter()
        .map(|value| value.as_float().ok())
        .collect();
    let values = values?;
    Some(Matrix {
        a: values[0],
        b: values[1],
        c: values[2],
        d: values[3],
        e: values[4],
        f: values[5],
    })
}

fn translation(x: f64, y: f64) -> Matrix {
    Matrix {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: x as f32,
        f: y as f32,
    }
}

fn transform_point(matrix: &Matrix, x: f64, y: f64) -> (f64, f64) {
    (
        x * f64::from(matrix.a) + y * f64::from(matrix.c) + f64::from(matrix.e),
        x * f64::from(matrix.b) + y * f64::from(matrix.d) + f64::from(matrix.f),
    )
}

fn move_text(matrix: &mut Matrix, tx: f64) {
    *matrix = translation(tx, 0.0).mul(matrix);
}

fn next_line(state: &mut TextState) {
    state.line_matrix = translation(0.0, -state.params.leading).mul(&state.line_matrix);
    state.matrix = state.line_matrix;
    state.position_known = true;
}

fn move_line(state: &mut TextState, tx: f64, ty: f64) {
    state.line_matrix = translation(tx, ty).mul(&state.line_matrix);
    state.matrix = state.line_matrix;
    state.position_known = true;
}

/// Tabla código → glifo (como Unicode) de una Core14 a partir de `/Encoding`:
/// un nombre, o un diccionario con `/BaseEncoding` y `/Differences`.
///
/// Los anchos AFM son del glifo pintado, así que manda la codificación declarada
/// y nunca `/ToUnicode`, que describe el texto extraíble. Tampoco se usa
/// `get_font_encoding` de lopdf: prefiere `/ToUnicode` y arrastra su lista de
/// nombres de glifo entera, que en WASM de depuración supera el máximo de
/// variables locales de V8.
fn simple_encoding(
    doc: &Document,
    font: &[u8],
    dict: &Dictionary,
) -> Result<SimpleEncoding, UnsupportedReason> {
    let invalid = || UnsupportedReason::Encoding {
        name: font.to_vec(),
    };
    let base_of = |name: &[u8]| {
        core14_metrics::base_encoding(name).ok_or_else(|| UnsupportedReason::Encoding {
            name: name.to_vec(),
        })
    };
    let Some(object) = dict.get(b"Encoding").ok() else {
        return Ok(SimpleEncoding::from_base(base_of(b"StandardEncoding")?));
    };
    let encoding = resolved(doc, object).ok_or_else(invalid)?;
    if let Ok(name) = encoding.as_name() {
        return Ok(SimpleEncoding::from_base(base_of(name)?));
    }
    let encoding_dict = encoding.as_dict().map_err(|_| invalid())?;
    let base = match encoding_dict.get(b"BaseEncoding") {
        Ok(base) => base_of(base.as_name().map_err(|_| invalid())?)?,
        Err(_) => base_of(b"StandardEncoding")?,
    };
    let mut table = SimpleEncoding::from_base(base);
    let Some(differences) = encoding_dict.get(b"Differences").ok() else {
        return Ok(table);
    };
    let items = resolved_array(doc, differences).ok_or_else(invalid)?;
    let mut next_code: Option<usize> = None;
    for item in items {
        match item {
            Object::Integer(code) => {
                next_code = Some(
                    usize::try_from(*code)
                        .ok()
                        .filter(|code| *code <= 255)
                        .ok_or_else(invalid)?,
                );
            }
            Object::Name(name) => {
                let code = next_code.filter(|code| *code <= 255).ok_or_else(invalid)?;
                // Un nombre sin ancho conocido (p. ej. `.notdef`) deja el código
                // sin métricas: sólo falla si ese código llega a pintarse.
                table.glyphs[code] = core14_metrics::glyph_unicode(name);
                next_code = Some(code + 1);
            }
            _ => return Err(invalid()),
        }
    }
    Ok(table)
}

fn missing_width(doc: &Document, dict: &Dictionary) -> f64 {
    dict.get(b"FontDescriptor")
        .ok()
        .and_then(|value| resolved_dict(doc, value))
        .and_then(|descriptor| descriptor.get(b"MissingWidth").ok())
        .and_then(number)
        .unwrap_or(0.0)
}

fn type0_widths(doc: &Document, descendant: &Dictionary) -> Option<HashMap<u16, f64>> {
    let mut widths = HashMap::new();
    let Some(items) = descendant
        .get(b"W")
        .ok()
        .and_then(|value| resolved_array(doc, value))
    else {
        return Some(widths);
    };
    let mut index = 0;
    while index < items.len() {
        let first = u16::try_from(items.get(index)?.as_i64().ok()?).ok()?;
        index += 1;
        match items.get(index)? {
            Object::Array(values) => {
                for (offset, value) in values.iter().enumerate() {
                    let cid = first.checked_add(u16::try_from(offset).ok()?)?;
                    widths.insert(cid, number(value)?);
                }
                index += 1;
            }
            last_object => {
                let last = u16::try_from(last_object.as_i64().ok()?).ok()?;
                let width = number(items.get(index + 1)?)?;
                for cid in first..=last {
                    widths.insert(cid, width);
                }
                index += 2;
            }
        }
    }
    Some(widths)
}

fn metrics_for_font(doc: &Document, resource_name: &[u8], dict: &Dictionary) -> FontMetrics {
    let subtype = dict
        .get(b"Subtype")
        .and_then(Object::as_name)
        .unwrap_or_default();
    if subtype == b"Type3" {
        return FontMetrics::Unsupported(UnsupportedReason::Type3 {
            font: resource_name.to_vec(),
        });
    }
    if subtype == b"Type0" {
        let cmap = dict
            .get(b"Encoding")
            .ok()
            .and_then(|value| resolved(doc, value))
            .and_then(|value| value.as_name().ok())
            .unwrap_or_default();
        if cmap != b"Identity-H" {
            return FontMetrics::Unsupported(UnsupportedReason::CMap {
                name: cmap.to_vec(),
            });
        }
        let Some(descendant) = dict
            .get(b"DescendantFonts")
            .ok()
            .and_then(|value| resolved_array(doc, value))
            .and_then(|values| values.first())
            .and_then(|value| resolved_dict(doc, value))
        else {
            return FontMetrics::Unsupported(UnsupportedReason::NoMetrics {
                font: resource_name.to_vec(),
            });
        };
        let Some(widths) = type0_widths(doc, descendant) else {
            return FontMetrics::Unsupported(UnsupportedReason::NoMetrics {
                font: resource_name.to_vec(),
            });
        };
        let default_width = descendant
            .get(b"DW")
            .ok()
            .and_then(number)
            .unwrap_or(1000.0);
        return FontMetrics::Type0 {
            widths,
            default_width,
        };
    }

    if subtype == b"Type1" || subtype == b"TrueType" || subtype == b"MMType1" {
        if let (Some(first_char), Some(widths)) = (
            dict.get(b"FirstChar")
                .ok()
                .and_then(|value| value.as_i64().ok()),
            dict.get(b"Widths")
                .ok()
                .and_then(|value| resolved_array(doc, value)),
        ) {
            let Some(widths) = widths.iter().map(number).collect() else {
                return FontMetrics::Unsupported(UnsupportedReason::NoMetrics {
                    font: resource_name.to_vec(),
                });
            };
            return FontMetrics::Simple {
                first_char,
                widths,
                missing_width: missing_width(doc, dict),
            };
        }

        let base_font = dict
            .get(b"BaseFont")
            .and_then(Object::as_name)
            .unwrap_or_default();
        if base_font == b"Symbol" || base_font == b"ZapfDingbats" {
            return FontMetrics::Unsupported(UnsupportedReason::SymbolicCore14 {
                font: base_font.to_vec(),
            });
        }
        if let Some(font) = CoreFont::from_base_name(base_font) {
            return match simple_encoding(doc, resource_name, dict) {
                Ok(encoding) => FontMetrics::Core14 {
                    font,
                    encoding: Box::new(encoding),
                },
                Err(reason) => FontMetrics::Unsupported(reason),
            };
        }
    }

    FontMetrics::Unsupported(UnsupportedReason::NoMetrics {
        font: resource_name.to_vec(),
    })
}

fn page_font_metrics(
    doc: &Document,
    page_id: ObjectId,
) -> lopdf::Result<HashMap<Vec<u8>, FontMetrics>> {
    let mut fonts: HashMap<_, _> = doc
        .get_page_fonts(page_id)?
        .into_iter()
        .map(|(name, dict)| {
            let metrics = metrics_for_font(doc, &name, dict);
            (name, metrics)
        })
        .collect();

    // lopdf 0.43 recorre recursos heredados cuando /Resources es una referencia,
    // pero no cuando el diccionario está incrustado directamente en /Pages.
    // Completar ese caso evita perder fuentes válidas sin cambiar la precedencia
    // (la definición más cercana ya fue insertada primero por get_page_fonts).
    let mut current = Some(page_id);
    let mut visited = HashSet::new();
    while let Some(id) = current {
        if !visited.insert(id) {
            break;
        }
        let node = doc.get_dictionary(id)?;
        if let Ok(resources) = node.get(b"Resources").and_then(Object::as_dict) {
            if let Ok(font_object) = resources.get(b"Font") {
                if let Some(font_dict) = resolved_dict(doc, font_object) {
                    for (name, object) in font_dict {
                        if fonts.contains_key(name) {
                            continue;
                        }
                        if let Some(dict) = resolved_dict(doc, object) {
                            fonts.insert(name.clone(), metrics_for_font(doc, name, dict));
                        }
                    }
                }
            }
        }
        current = node
            .get(b"Parent")
            .ok()
            .and_then(|object| object.as_reference().ok());
    }
    Ok(fonts)
}

fn collect_form_names(doc: &Document, resources: &Dictionary, out: &mut HashSet<Vec<u8>>) {
    let Some(xobjects) = resources
        .get(b"XObject")
        .ok()
        .and_then(|value| resolved_dict(doc, value))
    else {
        return;
    };
    for (name, value) in xobjects {
        let Some(dict) = resolved(doc, value).and_then(|value| match value {
            Object::Stream(stream) => Some(&stream.dict),
            Object::Dictionary(dict) => Some(dict),
            _ => None,
        }) else {
            continue;
        };
        if dict
            .get(b"Subtype")
            .and_then(Object::as_name)
            .is_ok_and(|subtype| subtype == b"Form")
        {
            out.insert(name.clone());
        }
    }
}

fn page_form_names(doc: &Document, page_id: ObjectId) -> lopdf::Result<HashSet<Vec<u8>>> {
    let mut names = HashSet::new();
    let (inline, inherited) = doc.get_page_resources(page_id)?;
    if let Some(resources) = inline {
        collect_form_names(doc, resources, &mut names);
    }
    for id in inherited {
        if let Ok(resources) = doc.get_dictionary(id) {
            collect_form_names(doc, resources, &mut names);
        }
    }
    // Igual que con /Font, completar /Resources directo en un ancestro /Pages.
    let mut current = Some(page_id);
    let mut visited = HashSet::new();
    while let Some(id) = current {
        if !visited.insert(id) {
            break;
        }
        let node = doc.get_dictionary(id)?;
        if let Ok(resources) = node.get(b"Resources").and_then(Object::as_dict) {
            collect_form_names(doc, resources, &mut names);
        }
        current = node
            .get(b"Parent")
            .ok()
            .and_then(|object| object.as_reference().ok());
    }
    Ok(names)
}

fn show_string(
    bytes: &[u8],
    operand_index: usize,
    op_index: usize,
    state: &mut TextState,
    ctm: &Matrix,
    fonts: &HashMap<Vec<u8>, FontMetrics>,
    output: &mut PageText,
) {
    if !state.active {
        return;
    }
    // Cualquier salida sin medir deja la matriz de texto sin avanzar: lo que se
    // pinte después en este objeto de texto tendría una posición inventada.
    let mut unmeasured = |state: &mut TextState, reason: UnsupportedReason| {
        state.position_known = false;
        output.unsupported.push(Unsupported { op_index, reason });
    };
    if !state.position_known {
        unmeasured(state, UnsupportedReason::UnknownPosition);
        return;
    }
    let Some(font_name) = state.params.font_name.clone() else {
        unmeasured(state, UnsupportedReason::FontNotFound { font: Vec::new() });
        return;
    };
    let Some(metrics) = fonts.get(&font_name) else {
        unmeasured(state, UnsupportedReason::FontNotFound { font: font_name });
        return;
    };
    if let FontMetrics::Unsupported(reason) = metrics {
        unmeasured(state, reason.clone());
        return;
    }

    let code_len = metrics.code_len();
    if code_len == 2 && !bytes.len().is_multiple_of(2) {
        unmeasured(state, UnsupportedReason::MalformedText { font: font_name });
        return;
    }

    let mut measured = Vec::with_capacity(bytes.len() / code_len);
    for offset in (0..bytes.len()).step_by(code_len) {
        let code = if code_len == 2 {
            u32::from(u16::from_be_bytes([bytes[offset], bytes[offset + 1]]))
        } else {
            u32::from(bytes[offset])
        };
        let Some(width) = metrics.width(code) else {
            unmeasured(state, UnsupportedReason::NoMetrics { font: font_name });
            return;
        };
        measured.push((offset, code, width));
    }

    let unit = |x: f32, y: f32| {
        let (x, y) = (f64::from(x), f64::from(y));
        let length = x.hypot(y);
        if length > 0.0 {
            (x / length, y / length)
        } else {
            (0.0, 0.0)
        }
    };
    for (byte_offset, code, width) in measured {
        let combined = state.matrix.mul(ctm);
        let origin = transform_point(&combined, 0.0, state.params.rise);
        let baseline_scale = f64::from(combined.a).hypot(f64::from(combined.b));
        let vertical_scale = f64::from(combined.c).hypot(f64::from(combined.d));
        let word_spacing = if code_len == 1 && code == 32 {
            state.params.word_spacing
        } else {
            0.0
        };
        let advance_text =
            (width / 1000.0 * state.params.font_size + state.params.char_spacing + word_spacing)
                * state.params.horizontal_scale;
        // En un TJ, n desplaza −n/1000·Tfs·Th: el número que avanza lo mismo que
        // el glifo (con su Tc y su Tw) es n = −avance·1000/(Tfs·Th).
        let scale = state.params.font_size * state.params.horizontal_scale;
        let tj_adjustment = (scale.abs() > f64::EPSILON).then(|| -advance_text * 1000.0 / scale);
        output.glyphs.push(Glyph {
            op_index,
            operand_index,
            byte_offset,
            code,
            code_len: code_len as u8,
            origin,
            advance: advance_text * baseline_scale,
            font_size_eff: state.params.font_size.abs() * vertical_scale,
            font_res_name: font_name.clone(),
            render_mode: state.params.render_mode,
            baseline_dir: unit(combined.a, combined.b),
            ascent_dir: unit(combined.c, combined.d),
            tj_adjustment,
            font_width: width,
            text_advance: advance_text,
            font_size: state.params.font_size,
            horizontal_scale: state.params.horizontal_scale,
            char_spacing: state.params.char_spacing,
            word_spacing: state.params.word_spacing,
        });
        move_text(&mut state.matrix, advance_text);
    }
}

/// Interpreta `content` como el contenido de `page_id`. No lee streams: el
/// llamador ya los leyó acotados (o, en la verificación, los acaba de producir
/// en memoria).
pub(crate) fn interpret_content(
    doc: &Document,
    page_id: ObjectId,
    content: &Content,
) -> lopdf::Result<PageText> {
    let operations = &content.operations;
    let fonts = page_font_metrics(doc, page_id)?;
    let form_names = page_form_names(doc, page_id)?;
    let mut output = PageText {
        glyphs: Vec::new(),
        unsupported: Vec::new(),
    };
    let mut ctm = Matrix::IDENTITY;
    let mut graphics_stack: Vec<(Matrix, TextParameters)> = Vec::new();
    let mut text = TextState::default();

    for (op_index, operation) in operations.iter().enumerate() {
        match operation.operator.as_str() {
            "q" => graphics_stack.push((ctm, text.params.clone())),
            "Q" => {
                if let Some((saved_ctm, saved_params)) = graphics_stack.pop() {
                    ctm = saved_ctm;
                    text.params = saved_params;
                }
            }
            "cm" => {
                if let Some(matrix) = matrix_from_operands(&operation.operands) {
                    ctm = matrix.mul(&ctm);
                }
            }
            "BT" => {
                text.active = true;
                text.matrix = Matrix::IDENTITY;
                text.line_matrix = Matrix::IDENTITY;
                text.position_known = true;
            }
            "ET" => text.active = false,
            "Tf" => {
                if let (Some(name), Some(size)) = (
                    operation
                        .operands
                        .first()
                        .and_then(|value| value.as_name().ok()),
                    operation.operands.get(1).and_then(number),
                ) {
                    text.params.font_name = Some(name.to_vec());
                    text.params.font_size = size;
                }
            }
            "Tm" => {
                if let Some(matrix) = matrix_from_operands(&operation.operands) {
                    text.matrix = matrix;
                    text.line_matrix = matrix;
                    text.position_known = true;
                }
            }
            "Td" => {
                if let (Some(tx), Some(ty)) = (
                    operation.operands.first().and_then(number),
                    operation.operands.get(1).and_then(number),
                ) {
                    move_line(&mut text, tx, ty);
                }
            }
            "TD" => {
                if let (Some(tx), Some(ty)) = (
                    operation.operands.first().and_then(number),
                    operation.operands.get(1).and_then(number),
                ) {
                    text.params.leading = -ty;
                    move_line(&mut text, tx, ty);
                }
            }
            "T*" => next_line(&mut text),
            "TL" => {
                if let Some(value) = operation.operands.first().and_then(number) {
                    text.params.leading = value;
                }
            }
            "Tc" => {
                if let Some(value) = operation.operands.first().and_then(number) {
                    text.params.char_spacing = value;
                }
            }
            "Tw" => {
                if let Some(value) = operation.operands.first().and_then(number) {
                    text.params.word_spacing = value;
                }
            }
            "Tz" => {
                if let Some(value) = operation.operands.first().and_then(number) {
                    text.params.horizontal_scale = value / 100.0;
                }
            }
            "Ts" => {
                if let Some(value) = operation.operands.first().and_then(number) {
                    text.params.rise = value;
                }
            }
            "Tr" => {
                if let Some(value) = operation
                    .operands
                    .first()
                    .and_then(|value| value.as_i64().ok())
                {
                    text.params.render_mode = value;
                }
            }
            "Tj" => {
                if let Some(Object::String(bytes, _)) = operation.operands.first() {
                    show_string(bytes, 0, op_index, &mut text, &ctm, &fonts, &mut output);
                }
            }
            "TJ" => {
                if let Some(Object::Array(items)) = operation.operands.first() {
                    for (operand_index, item) in items.iter().enumerate() {
                        match item {
                            Object::String(bytes, _) => show_string(
                                bytes,
                                operand_index,
                                op_index,
                                &mut text,
                                &ctm,
                                &fonts,
                                &mut output,
                            ),
                            value => {
                                if let Some(adjustment) = number(value) {
                                    let tx = -adjustment / 1000.0
                                        * text.params.font_size
                                        * text.params.horizontal_scale;
                                    move_text(&mut text.matrix, tx);
                                }
                            }
                        }
                    }
                }
            }
            "'" => {
                next_line(&mut text);
                if let Some(Object::String(bytes, _)) = operation.operands.first() {
                    show_string(bytes, 0, op_index, &mut text, &ctm, &fonts, &mut output);
                }
            }
            "\"" => {
                if let (Some(word), Some(character)) = (
                    operation.operands.first().and_then(number),
                    operation.operands.get(1).and_then(number),
                ) {
                    text.params.word_spacing = word;
                    text.params.char_spacing = character;
                }
                next_line(&mut text);
                if let Some(Object::String(bytes, _)) = operation.operands.get(2) {
                    show_string(bytes, 2, op_index, &mut text, &ctm, &fonts, &mut output);
                }
            }
            "Do" => {
                if let Some(name) = operation
                    .operands
                    .first()
                    .and_then(|value| value.as_name().ok())
                {
                    if form_names.contains(name) {
                        output.unsupported.push(Unsupported {
                            op_index,
                            reason: UnsupportedReason::FormXObject {
                                name: name.to_vec(),
                            },
                        });
                    }
                }
            }
            _ => {}
        }
    }

    Ok(output)
}

#[cfg(test)]
pub(crate) fn interpret_page_text(doc: &Document, page_id: ObjectId) -> lopdf::Result<PageText> {
    let content = doc.get_and_decode_page_content(page_id)?;
    interpret_content(doc, page_id, &content)
}

#[cfg(test)]
mod tests {
    use lopdf::{dictionary, Dictionary, Document, Object, Stream};

    use super::{core14_metrics, interpret_page_text, CoreFont, UnsupportedReason};

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 0.01,
            "actual={actual}, esperado={expected}"
        );
    }

    fn font_with_widths(first: i64, widths: Vec<Object>) -> Object {
        Object::Dictionary(dictionary! {
            "Type" => "Font", "Subtype" => "TrueType", "BaseFont" => "Synthetic",
            "FirstChar" => first, "LastChar" => first + widths.len() as i64 - 1,
            "Widths" => widths,
        })
    }

    fn doc_with_resources(
        content: &[u8],
        fonts: Vec<(&str, Object)>,
        xobjects: Option<Dictionary>,
    ) -> (Document, lopdf::ObjectId) {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.to_vec()));
        let mut font_resources = Dictionary::new();
        for (name, font) in fonts {
            let id = doc.add_object(font);
            font_resources.set(name, id);
        }
        let mut resources = dictionary! { "Font" => font_resources };
        if let Some(xobjects) = xobjects {
            resources.set("XObject", xobjects);
        }
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
            "Resources" => resources,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        (doc, page_id)
    }

    #[test]
    fn simple_text_applies_width_spacing_scale_rise_and_tm() {
        let widths = (0..=33)
            .map(|index| if index == 0 { 250.into() } else { 500.into() })
            .collect();
        let font = font_with_widths(32, widths);
        let content = b"BT /F1 10 Tf 1 Tc 4 Tw 50 Tz 3 Ts 2 0 0 3 10 20 Tm (A A) Tj ET";
        let (doc, page_id) = doc_with_resources(content, vec![("F1", font)], None);
        let text = interpret_page_text(&doc, page_id).unwrap();

        assert_eq!(text.glyphs.len(), 3);
        // A: ((500/1000)*10 + Tc 1)*Tz .5 = 3 unidades de texto;
        // Tm escala x por 2, así que avanza 6 pt. Ts 3 pasa por escala y 3: y=29.
        assert_close(text.glyphs[0].origin.0, 10.0);
        assert_close(text.glyphs[0].origin.1, 29.0);
        assert_close(text.glyphs[0].advance, 6.0);
        assert_close(text.glyphs[0].font_size_eff, 30.0);
        // Espacio: ((250/1000)*10 + Tc 1 + Tw 4)*.5*2 = 7.5 pt.
        assert_close(text.glyphs[1].origin.0, 16.0);
        assert_close(text.glyphs[1].advance, 7.5);
        assert_close(text.glyphs[2].origin.0, 23.5);
        assert_eq!(text.glyphs[2].byte_offset, 2);
    }

    #[test]
    fn text_positioning_operators_keep_the_line_matrix_rules() {
        let font = font_with_widths(
            65,
            vec![
                500.into(),
                500.into(),
                500.into(),
                500.into(),
                500.into(),
                500.into(),
            ],
        );
        let content = b"BT /F1 10 Tf 12 TL 1 0 0 1 100 200 Tm (A) Tj T* (B) Tj 5 -20 Td (C) Tj 7 -30 TD (D) Tj (E) ' 2 3 (F) \" ET";
        let (doc, page_id) = doc_with_resources(content, vec![("F1", font)], None);
        let text = interpret_page_text(&doc, page_id).unwrap();
        let origins: Vec<_> = text.glyphs.iter().map(|glyph| glyph.origin).collect();

        // T* vuelve a la matriz de línea (no al avance de A); TD fija TL=30.
        for (actual, expected) in origins.iter().zip([
            (100.0, 200.0),
            (100.0, 188.0),
            (105.0, 168.0),
            (112.0, 138.0),
            (112.0, 108.0),
            (112.0, 78.0),
        ]) {
            assert_close(actual.0, expected.0);
            assert_close(actual.1, expected.1);
        }
        // " fija Tc=3 antes de pintar F: 500/1000*10 + 3 = 8.
        assert_close(text.glyphs[5].advance, 8.0);
    }

    #[test]
    fn tj_array_applies_both_signs_without_emitting_numeric_glyphs() {
        let font = font_with_widths(65, vec![500.into(), 500.into(), 500.into()]);
        let content = b"BT /F1 10 Tf 1 0 0 1 0 0 Tm [(A) -200 (B) 300 (C)] TJ ET";
        let (doc, page_id) = doc_with_resources(content, vec![("F1", font)], None);
        let text = interpret_page_text(&doc, page_id).unwrap();

        // A avanza 5; -200 resta -2 y por tanto suma 2: B=7.
        // B avanza 5; +300 resta 3: C=9.
        assert_eq!(text.glyphs.len(), 3);
        assert_close(text.glyphs[0].origin.0, 0.0);
        assert_close(text.glyphs[1].origin.0, 7.0);
        assert_close(text.glyphs[2].origin.0, 9.0);
        assert_eq!(text.glyphs[0].operand_index, 0);
        assert_eq!(text.glyphs[1].operand_index, 2);
        assert_eq!(text.glyphs[2].operand_index, 4);
    }

    #[test]
    fn nested_graphics_state_transforms_origins_and_restores_ctm() {
        let font = font_with_widths(65, vec![500.into(), 500.into(), 500.into()]);
        let content = b"q 2 0 0 2 10 20 cm q 3 0 0 3 1 1 cm BT /F1 10 Tf 1 0 0 1 5 6 Tm (A) Tj ET Q BT /F1 10 Tf 1 0 0 1 5 6 Tm (B) Tj ET Q BT /F1 10 Tf 1 0 0 1 5 6 Tm (C) Tj ET";
        let (doc, page_id) = doc_with_resources(content, vec![("F1", font)], None);
        let text = interpret_page_text(&doc, page_id).unwrap();

        // cm interno×externo = escala 6, traslación (12,22): (5,6) -> (42,58).
        // Cada Q restaura exactamente el CTM anterior.
        for (glyph, expected) in text
            .glyphs
            .iter()
            .zip([(42.0, 58.0), (20.0, 32.0), (5.0, 6.0)])
        {
            assert_close(glyph.origin.0, expected.0);
            assert_close(glyph.origin.1, expected.1);
        }
    }

    fn type0_font() -> Object {
        Object::Dictionary(dictionary! {
            "Type" => "Font", "Subtype" => "Type0", "BaseFont" => "SyntheticCID",
            "Encoding" => "Identity-H",
            "DescendantFonts" => vec![Object::Dictionary(dictionary! {
                "Type" => "Font", "Subtype" => "CIDFontType2", "BaseFont" => "SyntheticCID",
                "DW" => 900,
                "W" => vec![
                    1.into(), Object::Array(vec![500.into(), 600.into()]),
                    3.into(), 4.into(), 700.into(),
                ],
            })],
        })
    }

    #[test]
    fn identity_h_uses_both_w_forms_dw_and_never_word_spacing() {
        let content = b"BT /F0 10 Tf 1 Tc 100 Tw 1 0 0 1 0 0 Tm <000100020003000400050020> Tj ET";
        let (doc, page_id) = doc_with_resources(content, vec![("F0", type0_font())], None);
        let text = interpret_page_text(&doc, page_id).unwrap();

        assert_eq!(text.glyphs.len(), 6);
        assert_eq!(text.glyphs[0].code_len, 2);
        assert_eq!(text.glyphs[1].byte_offset, 2);
        // /W: 500,600 y rango 3..4=700; CID 5 usa /DW=900. Tc suma 1.
        // Tw=100 no aplica ni siquiera al CID 0x0020 en una fuente multibyte.
        for (glyph, (origin, advance)) in text.glyphs.iter().zip([
            (0.0, 6.0),
            (6.0, 7.0),
            (13.0, 8.0),
            (21.0, 8.0),
            (29.0, 10.0),
            (39.0, 10.0),
        ]) {
            assert_close(glyph.origin.0, origin);
            assert_close(glyph.advance, advance);
        }
    }

    #[test]
    fn core14_helvetica_uses_winansi_and_differences_without_widths() {
        let winansi = Object::Dictionary(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
            "Encoding" => "WinAnsiEncoding",
        });
        let differences = Object::Dictionary(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
            "Encoding" => Object::Dictionary(dictionary! {
                "Type" => "Encoding", "BaseEncoding" => "WinAnsiEncoding",
                "Differences" => vec![65.into(), Object::Name(b"space".to_vec())],
            }),
        });
        let content = b"BT /FW 10 Tf <8041> Tj /FD 10 Tf (A) Tj ET";
        let (doc, page_id) =
            doc_with_resources(content, vec![("FW", winansi), ("FD", differences)], None);
        let text = interpret_page_text(&doc, page_id).unwrap();

        // Helvetica AFM: Euro=556, A=667 y Difference A→space=278.
        assert_close(text.glyphs[0].advance, 5.56);
        assert_close(text.glyphs[1].advance, 6.67);
        assert_close(text.glyphs[2].advance, 2.78);
        assert!(text.unsupported.is_empty());
    }

    #[test]
    fn inherited_font_resources_and_missing_width_are_respected() {
        let font = Object::Dictionary(dictionary! {
            "Type" => "Font", "Subtype" => "TrueType", "BaseFont" => "Synthetic",
            "FirstChar" => 65, "LastChar" => 65, "Widths" => vec![500.into()],
            "FontDescriptor" => Object::Dictionary(dictionary! { "MissingWidth" => 300 }),
        });
        let (mut doc, page_id) = doc_with_resources(
            b"BT /F1 10 Tf 1 0 0 1 0 0 Tm (AZ) Tj ET",
            vec![("F1", font)],
            None,
        );
        let parent_id = doc
            .get_dictionary(page_id)
            .unwrap()
            .get(b"Parent")
            .unwrap()
            .as_reference()
            .unwrap();
        let resources = doc
            .get_dictionary_mut(page_id)
            .unwrap()
            .remove(b"Resources")
            .unwrap();
        doc.get_dictionary_mut(parent_id)
            .unwrap()
            .set("Resources", resources);

        let text = interpret_page_text(&doc, page_id).unwrap();
        assert_eq!(text.glyphs.len(), 2);
        // A usa /Widths=500; Z queda fuera y usa /MissingWidth=300.
        assert_close(text.glyphs[0].advance, 5.0);
        assert_close(text.glyphs[1].origin.0, 5.0);
        assert_close(text.glyphs[1].advance, 3.0);
    }

    #[test]
    fn unsupported_inputs_are_reported_without_inventing_glyphs() {
        let type3 = Object::Dictionary(dictionary! { "Type" => "Font", "Subtype" => "Type3" });
        let cmap = Object::Dictionary(dictionary! {
            "Type" => "Font", "Subtype" => "Type0", "Encoding" => "Identity-V",
        });
        let no_metrics = Object::Dictionary(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "CustomFont",
        });
        let form = Stream::new(
            dictionary! { "Type" => "XObject", "Subtype" => "Form", "BBox" => vec![0.into(), 0.into(), 10.into(), 10.into()] },
            Vec::new(),
        );
        let mut xobjects = Dictionary::new();
        xobjects.set("Fm", form);
        // Cada caso en su propio BT: tras una cadena sin medir, lo que sigue en el
        // mismo objeto de texto ya no tiene posición fiable (último BT).
        let content = b"BT /T3 10 Tf (A) Tj ET BT /CM 10 Tf <0001> Tj ET BT /NM 10 Tf (A) Tj ET BT /Absent 10 Tf (A) Tj ET /Fm Do BT /NM 10 Tf 1 0 0 1 0 0 Tm (A) Tj /F1 10 Tf (A) Tj 0 -12 Td (A) Tj ET";
        let (page_doc, page_id) = doc_with_resources(
            content,
            vec![
                ("T3", type3),
                ("CM", cmap),
                ("NM", no_metrics),
                ("F1", font_with_widths(65, vec![500.into()])),
            ],
            Some(xobjects),
        );
        let text = interpret_page_text(&page_doc, page_id).unwrap();

        // Sólo la A tras `Td` recupera la posición; la de justo después de la
        // cadena sin medir se descarta aunque su fuente tenga /Widths.
        assert_eq!(text.glyphs.len(), 1);
        assert_close(text.glyphs[0].origin.0, 0.0);
        assert_close(text.glyphs[0].origin.1, -12.0);
        assert!(text
            .unsupported
            .iter()
            .any(|item| item.reason == UnsupportedReason::UnknownPosition));
        assert!(text
            .unsupported
            .iter()
            .any(|item| matches!(item.reason, UnsupportedReason::Type3 { .. })));
        assert!(text
            .unsupported
            .iter()
            .any(|item| matches!(item.reason, UnsupportedReason::CMap { .. })));
        assert!(text
            .unsupported
            .iter()
            .any(|item| matches!(item.reason, UnsupportedReason::NoMetrics { .. })));
        assert!(text
            .unsupported
            .iter()
            .any(|item| matches!(item.reason, UnsupportedReason::FontNotFound { .. })));
        assert!(text
            .unsupported
            .iter()
            .any(|item| matches!(item.reason, UnsupportedReason::FormXObject { .. })));
    }

    #[test]
    fn fixture_exercises_all_three_font_families_and_tj_kerning() {
        let doc =
            Document::load_mem(include_bytes!("../tests/fixtures/editor-fuentes.pdf")).unwrap();
        let page_id = doc.get_pages()[&1];
        let text = interpret_page_text(&doc, page_id).unwrap();
        assert!(text.unsupported.is_empty(), "{:?}", text.unsupported);

        for expected_y in [720.0, 620.0, 520.0] {
            assert!(
                text.glyphs
                    .iter()
                    .any(|glyph| (glyph.origin.1 - expected_y).abs() < 0.01),
                "no se encontró y={expected_y}; primeras posiciones: {:?}",
                text.glyphs
                    .iter()
                    .take(6)
                    .map(|glyph| glyph.origin)
                    .collect::<Vec<_>>()
            );
        }
        let line_b: Vec<_> = text
            .glyphs
            .iter()
            .filter(|glyph| (glyph.origin.1 - 620.0).abs() < 0.01)
            .collect();
        assert_close(line_b[0].origin.0, 60.0);

        let first_phrase = b"Frase que se cambia";
        let width: f64 = first_phrase
            .iter()
            .map(|code| {
                // El fixture usa WinAnsi; para ASCII el código coincide con Unicode.
                core14_metrics::width(CoreFont::Helvetica, u16::from(*code)).unwrap()
            })
            .sum();
        // AFM Helvetica: 9337 unidades = 130.718 pt a 14 pt; TJ -2500 agrega 35 pt.
        let expected_second_x = 60.0 + width / 1000.0 * 14.0 + 2500.0 / 1000.0 * 14.0;
        assert_close(line_b[first_phrase.len()].origin.0, expected_second_x);
    }
}
