//! Aplicación opt-in del subsetting a programas compartidos.

use crate::font_subset::subset_truetype_keep_gids;
use crate::font_usage::{collect_font_usage, union_program_usage};
use crate::report::Warning;
use lopdf::{Document, ObjectId};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Default)]
pub(crate) struct FontSubsetSummary {
    pub subsetted: usize,
    pub before_bytes: u64,
    pub after_bytes: u64,
    pub abstained: BTreeMap<&'static str, usize>,
}

impl FontSubsetSummary {
    fn abstain(&mut self, reason: &'static str) {
        *self.abstained.entry(reason).or_default() += 1;
    }

    pub(crate) fn warning(self, output_kept: bool) -> Warning {
        let mut abstained = self.abstained;
        if !output_kept && self.subsetted > 0 {
            *abstained.entry("output_not_kept").or_default() += self.subsetted;
        }
        let abstained_by_reason = abstained.into_iter().collect();
        Warning::FontSubsetting {
            subsetted: if output_kept { self.subsetted } else { 0 },
            before_bytes: if output_kept { self.before_bytes } else { 0 },
            after_bytes: if output_kept { self.after_bytes } else { 0 },
            abstained_by_reason,
        }
    }
}

fn shared_with_unsupported(
    doc: &Document,
    program_id: ObjectId,
    candidates: &BTreeSet<ObjectId>,
) -> bool {
    doc.objects.iter().any(|(&font_id, object)| {
        let Ok(font) = object.as_dict() else {
            return false;
        };
        let Some(descriptor) = font
            .get(b"FontDescriptor")
            .ok()
            .and_then(|o| doc.dereference(o).ok().map(|(_, value)| value))
            .and_then(|o| o.as_dict().ok())
        else {
            return false;
        };
        let same_program = descriptor
            .get(b"FontFile2")
            .ok()
            .and_then(|o| o.as_reference().ok())
            == Some(program_id);
        same_program && !candidates.contains(&font_id)
    })
}

pub(crate) fn subset_document_fonts(doc: &mut Document) -> FontSubsetSummary {
    let fonts = collect_font_usage(doc);
    let candidate_descendants: BTreeSet<_> = fonts.iter().map(|f| f.descendant_id).collect();
    let programs = union_program_usage(&fonts);
    let mut summary = FontSubsetSummary::default();
    for program in programs {
        if let Some(reason) = program.abstain {
            summary.abstain(reason.as_str());
            continue;
        }
        if shared_with_unsupported(doc, program.program_id, &candidate_descendants) {
            summary.abstain("shared_with_unsupported");
            continue;
        }
        let original = doc
            .get_object(program.program_id)
            .ok()
            .and_then(|o| o.as_stream().ok())
            .and_then(|s| s.get_plain_content().ok());
        let Some(original) = original else {
            summary.abstain("decode_failed");
            continue;
        };
        let Some(subset) = subset_truetype_keep_gids(&original, &program.gids) else {
            summary.abstain("invalid_or_no_savings");
            continue;
        };
        let Some(stream) = doc
            .get_object_mut(program.program_id)
            .ok()
            .and_then(|o| o.as_stream_mut().ok())
        else {
            summary.abstain("rewrite_failed");
            continue;
        };
        stream.set_plain_content(subset);
        stream.dict.set("Length1", stream.content.len() as i64);
        summary.subsetted += 1;
        summary.before_bytes += original.len() as u64;
        summary.after_bytes += stream.content.len() as u64;
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font_subset::tests::fixture as ttf_fixture;
    use crate::font_usage::tests::fixture as pdf_fixture;

    #[test]
    fn replaces_eligible_font_stream_and_reports_raw_bytes() {
        let (mut doc, _, program, _) = pdf_fixture(b"BT /F1 12 Tf <0001> Tj ET");
        let original = ttf_fixture();
        doc.get_object_mut(program)
            .unwrap()
            .as_stream_mut()
            .unwrap()
            .set_plain_content(original.clone());
        let summary = subset_document_fonts(&mut doc);
        let stream = doc.get_object(program).unwrap().as_stream().unwrap();
        let subset = stream.get_plain_content().unwrap();
        assert_eq!(summary.subsetted, 1);
        assert_eq!(summary.before_bytes, original.len() as u64);
        assert_eq!(summary.after_bytes, subset.len() as u64);
        assert!(subset.len() < original.len());
        assert_eq!(
            stream.dict.get(b"Length1").unwrap().as_i64().unwrap(),
            subset.len() as i64
        );
    }

    #[test]
    fn pipeline_only_reports_subsetting_when_opted_in() {
        let (mut doc, _, program, _) = pdf_fixture(b"BT /F1 12 Tf <0001> Tj ET");
        doc.get_object_mut(program)
            .unwrap()
            .as_stream_mut()
            .unwrap()
            .set_plain_content(ttf_fixture());
        let mut input = Vec::new();
        doc.save_to(&mut input).unwrap();
        let regular = crate::compress(&input, &crate::CompressOptions::default()).unwrap();
        assert!(!regular
            .report
            .warnings
            .iter()
            .any(|w| matches!(w, crate::Warning::FontSubsetting { .. })));
        let opt = crate::CompressOptions {
            subset_fonts: true,
            ..Default::default()
        };
        let subset = crate::compress(&input, &opt).unwrap();
        assert!(subset
            .report
            .warnings
            .iter()
            .any(|w| matches!(w, crate::Warning::FontSubsetting { .. })));
    }
}
