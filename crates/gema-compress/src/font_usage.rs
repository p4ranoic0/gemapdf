//! Recolección conservadora de GID usados por fuentes Type0/CIDFontType2.

use lopdf::{content::Content, Dictionary, Document, Object, ObjectId};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AbstainReason {
    AcroForm,
    DefaultAppearance,
    UnsupportedEncoding,
    UnsupportedCidMap,
    MalformedContent,
    UnresolvedResource,
    UntraversedResource,
}

impl AbstainReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::AcroForm => "acroform_dr",
            Self::DefaultAppearance => "default_appearance",
            Self::UnsupportedEncoding => "unsupported_encoding",
            Self::UnsupportedCidMap => "unsupported_cid_map",
            Self::MalformedContent => "malformed_content",
            Self::UnresolvedResource => "unresolved_resource",
            Self::UntraversedResource => "untraversed_resource",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct FontUse {
    pub font_id: ObjectId,
    pub descendant_id: ObjectId,
    pub program_id: ObjectId,
    pub gids: BTreeSet<u16>,
    pub abstain: Option<AbstainReason>,
    cid_map: CidMap,
}

#[derive(Debug, Clone)]
pub(crate) struct ProgramUse {
    pub program_id: ObjectId,
    pub gids: BTreeSet<u16>,
    pub abstain: Option<AbstainReason>,
}

pub(crate) fn union_program_usage(fonts: &[FontUse]) -> Vec<ProgramUse> {
    let mut grouped: BTreeMap<ObjectId, ProgramUse> = BTreeMap::new();
    for font in fonts {
        let entry = grouped
            .entry(font.program_id)
            .or_insert_with(|| ProgramUse {
                program_id: font.program_id,
                gids: BTreeSet::new(),
                abstain: None,
            });
        entry.gids.extend(&font.gids);
        entry.abstain = entry.abstain.or(font.abstain);
    }
    grouped.into_values().collect()
}

#[derive(Debug, Clone)]
enum CidMap {
    Identity,
    Stream(Vec<u8>),
    Unsupported,
}

fn resolved<'a>(doc: &'a Document, obj: &'a Object) -> Option<&'a Object> {
    doc.dereference(obj).ok().map(|(_, object)| object)
}

fn resolved_dict<'a>(doc: &'a Document, obj: &'a Object) -> Option<&'a Dictionary> {
    resolved(doc, obj)?.as_dict().ok()
}

fn name_is(dict: &Dictionary, key: &[u8], value: &[u8]) -> bool {
    dict.get(key)
        .and_then(Object::as_name)
        .is_ok_and(|name| name == value)
}

#[cfg_attr(not(test), allow(dead_code))] // Se conecta al recolector en la tarea siguiente.
fn da_font_name(bytes: &[u8]) -> Option<Vec<u8>> {
    let content = Content::decode_strict(bytes).ok()?;
    let tf = content
        .operations
        .iter()
        .rev()
        .find(|op| op.operator == "Tf")?;
    if tf.operands.len() != 2 || !matches!(tf.operands[1], Object::Integer(_) | Object::Real(_)) {
        return None;
    }
    Some(tf.operands[0].as_name().ok()?.to_vec())
}

fn candidate(doc: &Document, font_id: ObjectId, font: &Dictionary) -> Option<FontUse> {
    if !name_is(font, b"Subtype", b"Type0") {
        return None;
    }
    let descendant = font
        .get(b"DescendantFonts")
        .ok()?
        .as_array()
        .ok()?
        .first()?;
    let descendant_id = descendant.as_reference().ok()?;
    let cid = resolved_dict(doc, descendant)?;
    if !name_is(cid, b"Subtype", b"CIDFontType2") {
        return None;
    }
    let descriptor = resolved_dict(doc, cid.get(b"FontDescriptor").ok()?)?;
    let file = descriptor.get(b"FontFile2").ok()?;
    let program_id = file.as_reference().ok()?;
    doc.get_object(program_id).ok()?.as_stream().ok()?;
    let mut result = FontUse {
        font_id,
        descendant_id,
        program_id,
        gids: BTreeSet::from([0]),
        abstain: None,
        cid_map: CidMap::Unsupported,
    };
    let encoding = font.get(b"Encoding").and_then(Object::as_name).ok();
    if !matches!(encoding, Some(b"Identity-H" | b"Identity-V")) {
        result.abstain = Some(AbstainReason::UnsupportedEncoding);
    }
    result.cid_map = match cid
        .get(b"CIDToGIDMap")
        .ok()
        .and_then(|obj| resolved(doc, obj))
    {
        // ISO 32000-1, 9.7.4.2: si falta, el mapa por defecto es Identity.
        None => CidMap::Identity,
        Some(Object::Name(name)) if name == b"Identity" => CidMap::Identity,
        Some(Object::Stream(stream)) => stream
            .decompressed_content()
            .ok()
            .filter(|bytes| bytes.len() % 2 == 0)
            .map_or(CidMap::Unsupported, CidMap::Stream),
        _ => CidMap::Unsupported,
    };
    if matches!(result.cid_map, CidMap::Unsupported) && result.abstain.is_none() {
        result.abstain = Some(AbstainReason::UnsupportedCidMap);
    }
    Some(result)
}

#[derive(Default, Clone)]
struct Resources {
    fonts: BTreeMap<Vec<u8>, ObjectId>,
    xobjects: BTreeMap<Vec<u8>, ObjectId>,
    patterns: BTreeMap<Vec<u8>, ObjectId>,
}

fn extend_resources(doc: &Document, object: &Object, resources: &mut Resources) -> bool {
    let Some(dict) = resolved_dict(doc, object) else {
        return false;
    };
    for (key, target) in [
        (b"Font".as_slice(), &mut resources.fonts),
        (b"XObject".as_slice(), &mut resources.xobjects),
        (b"Pattern".as_slice(), &mut resources.patterns),
    ] {
        if let Ok(section) = dict.get(key) {
            let Some(entries) = resolved_dict(doc, section) else {
                return false;
            };
            for (name, value) in entries {
                let Ok(id) = value.as_reference() else {
                    return false;
                };
                target.insert(name.clone(), id);
            }
        }
    }
    true
}

struct Collector<'a> {
    doc: &'a Document,
    uses: Vec<FontUse>,
    by_id: BTreeMap<ObjectId, usize>,
    visited_owners: BTreeSet<ObjectId>,
}

impl Collector<'_> {
    fn abstain(&mut self, font_id: ObjectId, reason: AbstainReason) {
        if let Some(&i) = self.by_id.get(&font_id) {
            if self.uses[i].abstain.is_none() {
                self.uses[i].abstain = Some(reason);
            }
        }
    }

    fn abstain_resources(&mut self, resources: &Resources, reason: AbstainReason) {
        for &id in resources.fonts.values() {
            self.abstain(id, reason);
        }
    }

    fn page_resources(&mut self, page_id: ObjectId) -> Resources {
        let mut chain = Vec::new();
        let mut current = Some(page_id);
        let mut seen = BTreeSet::new();
        while let Some(id) = current {
            if !seen.insert(id) {
                break;
            }
            chain.push(id);
            current = self
                .doc
                .get_dictionary(id)
                .ok()
                .and_then(|d| d.get(b"Parent").ok())
                .and_then(|o| o.as_reference().ok());
        }
        let mut resources = Resources::default();
        for id in chain.into_iter().rev() {
            self.visited_owners.insert(id);
            if let Ok(value) = self
                .doc
                .get_dictionary(id)
                .and_then(|d| d.get(b"Resources"))
            {
                if let Ok(resource_id) = value.as_reference() {
                    self.visited_owners.insert(resource_id);
                }
                if !extend_resources(self.doc, value, &mut resources) {
                    self.abstain_resources(&resources, AbstainReason::UnresolvedResource);
                }
            }
        }
        resources
    }

    fn child_resources(
        &mut self,
        owner_id: ObjectId,
        dict: &Dictionary,
        parent: &Resources,
    ) -> Resources {
        self.visited_owners.insert(owner_id);
        let mut resources = parent.clone();
        if let Ok(value) = dict.get(b"Resources") {
            if let Ok(resource_id) = value.as_reference() {
                self.visited_owners.insert(resource_id);
            }
            if !extend_resources(self.doc, value, &mut resources) {
                self.abstain_resources(&resources, AbstainReason::UnresolvedResource);
            }
        }
        resources
    }

    fn map_codes(&mut self, font_id: ObjectId, bytes: &[u8]) {
        let Some(&index) = self.by_id.get(&font_id) else {
            return;
        };
        if !bytes.len().is_multiple_of(2) {
            self.abstain(font_id, AbstainReason::MalformedContent);
            return;
        }
        for code in bytes.chunks_exact(2) {
            let cid = u16::from_be_bytes([code[0], code[1]]);
            let gid = match &self.uses[index].cid_map {
                CidMap::Identity => cid,
                CidMap::Stream(map) => {
                    let offset = cid as usize * 2;
                    let Some(bytes) = map.get(offset..offset + 2) else {
                        self.abstain(font_id, AbstainReason::UnsupportedCidMap);
                        return;
                    };
                    u16::from_be_bytes([bytes[0], bytes[1]])
                }
                CidMap::Unsupported => return,
            };
            self.uses[index].gids.insert(gid);
        }
    }

    fn show(&mut self, font: Option<ObjectId>, value: &Object, resources: &Resources) {
        let Some(font_id) = font else {
            self.abstain_resources(resources, AbstainReason::UnresolvedResource);
            return;
        };
        match value {
            Object::String(bytes, _) => self.map_codes(font_id, bytes),
            Object::Array(parts) => {
                for part in parts {
                    if let Object::String(bytes, _) = part {
                        self.map_codes(font_id, bytes);
                    } else if !matches!(part, Object::Integer(_) | Object::Real(_)) {
                        self.abstain(font_id, AbstainReason::MalformedContent);
                    }
                }
            }
            _ => self.abstain(font_id, AbstainReason::MalformedContent),
        }
    }

    fn stream(
        &mut self,
        bytes: &[u8],
        resources: &Resources,
        inherited_font: Option<ObjectId>,
        stack: &mut BTreeSet<ObjectId>,
    ) {
        let Ok(content) = Content::decode(bytes) else {
            self.abstain_resources(resources, AbstainReason::MalformedContent);
            return;
        };
        let mut font = inherited_font;
        let mut saved = Vec::new();
        for op in content.operations {
            match op.operator.as_str() {
                "q" => saved.push(font),
                "Q" => {
                    if let Some(previous) = saved.pop() {
                        font = previous;
                    }
                }
                "Tf" => {
                    font = op
                        .operands
                        .first()
                        .and_then(|o| o.as_name().ok())
                        .and_then(|name| resources.fonts.get(name))
                        .copied();
                }
                "Tj" | "'" | "\"" => {
                    if let Some(value) = op.operands.last() {
                        self.show(font, value, resources);
                    } else {
                        self.abstain_resources(resources, AbstainReason::MalformedContent);
                    }
                }
                "TJ" => {
                    if let Some(value) = op.operands.first() {
                        self.show(font, value, resources);
                    } else {
                        self.abstain_resources(resources, AbstainReason::MalformedContent);
                    }
                }
                "Do" => {
                    if let Some(id) = op
                        .operands
                        .first()
                        .and_then(|o| o.as_name().ok())
                        .and_then(|name| resources.xobjects.get(name))
                        .copied()
                    {
                        self.nested_stream(id, resources, font, stack, false);
                    }
                }
                "scn" | "SCN" => {
                    if let Some(id) = op
                        .operands
                        .last()
                        .and_then(|o| o.as_name().ok())
                        .and_then(|name| resources.patterns.get(name))
                        .copied()
                    {
                        self.nested_stream(id, resources, font, stack, true);
                    }
                }
                _ => {}
            }
        }
    }

    fn nested_stream(
        &mut self,
        id: ObjectId,
        parent: &Resources,
        font: Option<ObjectId>,
        stack: &mut BTreeSet<ObjectId>,
        pattern: bool,
    ) {
        if !stack.insert(id) {
            self.abstain_resources(parent, AbstainReason::UnresolvedResource);
            return;
        }
        let Some(stream) = self
            .doc
            .get_object(id)
            .ok()
            .and_then(|o| o.as_stream().ok())
        else {
            self.abstain_resources(parent, AbstainReason::UnresolvedResource);
            stack.remove(&id);
            return;
        };
        let expected = if pattern {
            name_is(&stream.dict, b"PatternType", b"1")
                || stream
                    .dict
                    .get(b"PatternType")
                    .and_then(Object::as_i64)
                    .ok()
                    == Some(1)
        } else {
            name_is(&stream.dict, b"Subtype", b"Form")
        };
        if expected {
            let resources = self.child_resources(id, &stream.dict, parent);
            match stream.decompressed_content() {
                Ok(bytes) => self.stream(&bytes, &resources, font, stack),
                Err(_) => self.abstain_resources(&resources, AbstainReason::MalformedContent),
            }
        }
        stack.remove(&id);
    }

    fn appearances(&mut self, page_id: ObjectId, page_resources: &Resources) {
        let Some(annots) = self
            .doc
            .get_dictionary(page_id)
            .ok()
            .and_then(|d| d.get(b"Annots").ok())
            .and_then(|o| resolved(self.doc, o))
            .and_then(|o| o.as_array().ok())
        else {
            return;
        };
        for annot_ref in annots {
            let Some(annot) = resolved_dict(self.doc, annot_ref) else {
                continue;
            };
            if annot.get(b"DA").is_ok() {
                self.abstain_resources(page_resources, AbstainReason::DefaultAppearance);
            }
            let Some(ap) = annot
                .get(b"AP")
                .ok()
                .and_then(|o| resolved_dict(self.doc, o))
            else {
                continue;
            };
            for (_, value) in ap {
                self.appearance_value(value, page_resources);
            }
        }
    }

    fn appearance_value(&mut self, value: &Object, resources: &Resources) {
        match resolved(self.doc, value) {
            Some(Object::Stream(_)) => {
                if let Ok(id) = value.as_reference() {
                    self.nested_stream(id, resources, None, &mut BTreeSet::new(), false);
                } else {
                    self.abstain_resources(resources, AbstainReason::UnresolvedResource);
                }
            }
            Some(Object::Dictionary(dict)) => {
                for (_, child) in dict {
                    self.appearance_value(child, resources);
                }
            }
            _ => self.abstain_resources(resources, AbstainReason::UnresolvedResource),
        }
    }

    fn acroform(&mut self) {
        let Some(acro) = self
            .doc
            .catalog()
            .ok()
            .and_then(|c| c.get(b"AcroForm").ok())
            .and_then(|o| resolved_dict(self.doc, o))
        else {
            return;
        };
        if let Some(dr) = acro
            .get(b"DR")
            .ok()
            .and_then(|o| resolved_dict(self.doc, o))
        {
            if let Some(fonts) = dr
                .get(b"Font")
                .ok()
                .and_then(|o| resolved_dict(self.doc, o))
            {
                for (_, value) in fonts {
                    if let Ok(id) = value.as_reference() {
                        self.abstain(id, AbstainReason::AcroForm);
                    }
                }
            }
        }
        if acro.get(b"DA").is_ok() {
            for use_ in &mut self.uses {
                if use_.abstain.is_none() {
                    use_.abstain = Some(AbstainReason::DefaultAppearance);
                }
            }
        }
    }

    fn untraversed_resources(&mut self) {
        for (&id, object) in &self.doc.objects {
            if self.visited_owners.contains(&id) {
                continue;
            }
            let dict = match object {
                Object::Dictionary(d) => d,
                Object::Stream(s) => &s.dict,
                _ => continue,
            };
            if dict.get(b"DA").is_ok() {
                for use_ in &mut self.uses {
                    if use_.abstain.is_none() {
                        use_.abstain = Some(AbstainReason::DefaultAppearance);
                    }
                }
            }
            let mut resources = Resources::default();
            let source = dict.get(b"Resources").ok().cloned().or_else(|| {
                dict.get(b"Font")
                    .ok()
                    .map(|_| Object::Dictionary(dict.clone()))
            });
            if source
                .as_ref()
                .is_some_and(|obj| extend_resources(self.doc, obj, &mut resources))
            {
                self.abstain_resources(&resources, AbstainReason::UntraversedResource);
            }
        }
    }
}

pub(crate) fn collect_font_usage(doc: &Document) -> Vec<FontUse> {
    let uses: Vec<_> = doc
        .objects
        .iter()
        .filter_map(|(&id, obj)| candidate(doc, id, obj.as_dict().ok()?))
        .collect();
    let by_id = uses
        .iter()
        .enumerate()
        .map(|(i, use_)| (use_.font_id, i))
        .collect();
    let mut collector = Collector {
        doc,
        uses,
        by_id,
        visited_owners: BTreeSet::new(),
    };
    for (_, page_id) in doc.get_pages() {
        let resources = collector.page_resources(page_id);
        match doc.get_page_content(page_id) {
            Ok(bytes) => collector.stream(&bytes, &resources, None, &mut BTreeSet::new()),
            Err(_) => collector.abstain_resources(&resources, AbstainReason::MalformedContent),
        }
        collector.appearances(page_id, &resources);
    }
    collector.acroform();
    collector.untraversed_resources();
    collector.uses
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use lopdf::{dictionary, Dictionary, Object, Stream};

    pub(crate) fn fixture(page_content: &[u8]) -> (Document, ObjectId, ObjectId, ObjectId) {
        let mut doc = Document::with_version("1.7");
        let program = doc.add_object(Stream::new(
            dictionary! { "Length1" => 4 },
            vec![0, 1, 2, 3],
        ));
        let descriptor =
            doc.add_object(dictionary! { "Type" => "FontDescriptor", "FontFile2" => program });
        let cid = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "CIDFontType2", "FontDescriptor" => descriptor,
            "CIDToGIDMap" => "Identity"
        });
        let font = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type0", "Encoding" => "Identity-H",
            "DescendantFonts" => vec![Object::Reference(cid)]
        });
        let content = doc.add_object(Stream::new(Dictionary::new(), page_content.to_vec()));
        let pages = doc.new_object_id();
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages, "Contents" => content,
            "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
            "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } }
        });
        doc.objects.insert(
            pages,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![Object::Reference(page)], "Count" => 1
            }),
        );
        let root = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
        doc.trailer.set("Root", root);
        (doc, font, program, page)
    }

    #[test]
    fn da_parser_reads_last_tf_font_name() {
        assert_eq!(da_font_name(b"/Helv 0 Tf 0 g"), Some(b"Helv".to_vec()));
        assert_eq!(
            da_font_name(b"0 g /F1 9 Tf /F2 12 Tf"),
            Some(b"F2".to_vec())
        );
    }

    #[test]
    fn da_parser_abstains_without_readable_tf() {
        assert_eq!(da_font_name(b"0 g 0 w"), None);
        assert_eq!(da_font_name(b"/F1 12 Tf ("), None);
    }

    #[test]
    fn collects_page_text_and_tj_with_numbers() {
        let (doc, font, program, _) = fixture(b"BT /F1 12 Tf <0001> Tj [<0002> -120 <0003>] TJ ET");
        let uses = collect_font_usage(&doc);
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].font_id, font);
        assert_eq!(uses[0].program_id, program);
        assert_eq!(uses[0].gids, BTreeSet::from([0, 1, 2, 3]));
        assert_eq!(uses[0].abstain, None);
    }

    #[test]
    fn collects_nested_forms_and_annotation_appearances() {
        let (mut doc, font, _, page) = fixture(b"/Outer Do");
        let inner = doc.add_object(Stream::new(dictionary! {
            "Type" => "XObject", "Subtype" => "Form", "BBox" => vec![0.into(), 0.into(), 10.into(), 10.into()],
            "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } }
        }, b"BT /F1 12 Tf <0004> ' ET".to_vec()));
        let outer = doc.add_object(Stream::new(dictionary! {
            "Type" => "XObject", "Subtype" => "Form", "BBox" => vec![0.into(), 0.into(), 10.into(), 10.into()],
            "Resources" => dictionary! { "XObject" => dictionary! { "Inner" => inner } }
        }, b"/Inner Do".to_vec()));
        let ap = doc.add_object(Stream::new(
            dictionary! {
                "Subtype" => "Form", "BBox" => vec![0.into(), 0.into(), 10.into(), 10.into()],
                "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } }
            },
            b"BT /F1 12 Tf 0 0 <0005> \" ET".to_vec(),
        ));
        let annot =
            doc.add_object(dictionary! { "Type" => "Annot", "AP" => dictionary! { "N" => ap } });
        let page_dict = doc.get_object_mut(page).unwrap().as_dict_mut().unwrap();
        page_dict.set(
            "Resources",
            dictionary! { "XObject" => dictionary! { "Outer" => outer } },
        );
        page_dict.set("Annots", vec![Object::Reference(annot)]);
        let uses = collect_font_usage(&doc);
        assert_eq!(uses.len(), 1);
        assert!(uses[0].gids.contains(&4));
        assert!(uses[0].gids.contains(&5));
        assert_eq!(uses[0].abstain, None);
    }

    #[test]
    fn acroform_default_resources_force_abstention() {
        let (mut doc, font, _, _) = fixture(b"BT /F1 12 Tf <0001> Tj ET");
        let acro = doc.add_object(
            dictionary! { "DR" => dictionary! { "Font" => dictionary! { "F1" => font } } },
        );
        doc.catalog_mut().unwrap().set("AcroForm", acro);
        let uses = collect_font_usage(&doc);
        assert_eq!(uses[0].abstain, Some(AbstainReason::AcroForm));
    }

    #[test]
    fn shared_program_unions_gids_from_two_type0_fonts() {
        let (mut doc, font, program, page) =
            fixture(b"BT /F1 12 Tf <0001> Tj /F2 12 Tf <0006> Tj ET");
        let descendant = doc
            .get_dictionary(font)
            .unwrap()
            .get(b"DescendantFonts")
            .unwrap()
            .as_array()
            .unwrap()[0]
            .as_reference()
            .unwrap();
        let second = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type0", "Encoding" => "Identity-H",
            "DescendantFonts" => vec![Object::Reference(descendant)]
        });
        doc.get_dictionary_mut(page).unwrap().set(
            "Resources",
            dictionary! {
                "Font" => dictionary! { "F1" => font, "F2" => second }
            },
        );
        let fonts = collect_font_usage(&doc);
        let programs = union_program_usage(&fonts);
        assert_eq!(fonts.len(), 2);
        assert_eq!(programs.len(), 1);
        assert_eq!(programs[0].program_id, program);
        assert_eq!(programs[0].gids, BTreeSet::from([0, 1, 6]));
    }

    #[test]
    fn missing_cid_to_gid_map_defaults_to_identity() {
        // ISO 32000-1, 9.7.4.2: CIDToGIDMap ausente en una CIDFontType2 = Identity.
        let (mut doc, font, _, _) = fixture(b"BT /F1 12 Tf <0001> Tj ET");
        let descendant = doc
            .get_dictionary(font)
            .unwrap()
            .get(b"DescendantFonts")
            .unwrap()
            .as_array()
            .unwrap()[0]
            .as_reference()
            .unwrap();
        doc.get_dictionary_mut(descendant)
            .unwrap()
            .remove(b"CIDToGIDMap");
        let fonts = collect_font_usage(&doc);
        assert_eq!(fonts[0].gids, BTreeSet::from([0, 1]));
        assert_eq!(fonts[0].abstain, None);
    }

    #[test]
    fn cid_to_gid_stream_is_interpreted() {
        let (mut doc, font, _, _) = fixture(b"BT /F1 12 Tf <0001> Tj ET");
        let descendant = doc
            .get_dictionary(font)
            .unwrap()
            .get(b"DescendantFonts")
            .unwrap()
            .as_array()
            .unwrap()[0]
            .as_reference()
            .unwrap();
        let mapping = doc.add_object(Stream::new(Dictionary::new(), vec![0, 0, 0, 9]));
        doc.get_dictionary_mut(descendant)
            .unwrap()
            .set("CIDToGIDMap", mapping);
        let fonts = collect_font_usage(&doc);
        assert_eq!(fonts[0].gids, BTreeSet::from([0, 9]));
        assert_eq!(fonts[0].abstain, None);
    }

    #[test]
    fn tiling_pattern_text_is_collected() {
        let (mut doc, font, _, page) = fixture(b"/P1 scn");
        let pattern = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "Pattern", "PatternType" => 1,
                "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } }
            },
            b"BT /F1 12 Tf <0007> Tj ET".to_vec(),
        ));
        doc.get_dictionary_mut(page).unwrap().set(
            "Resources",
            dictionary! {
                "Pattern" => dictionary! { "P1" => pattern }
            },
        );
        assert!(collect_font_usage(&doc)[0].gids.contains(&7));
    }
}
