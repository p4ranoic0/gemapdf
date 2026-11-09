//! Pre-flight de gris: sólo recursos de imagen, sin interpretar operadores de color.
use lopdf::{Dictionary, Document, Object, ObjectId};
use std::collections::{BTreeSet, HashMap, HashSet};

#[derive(Default)]
pub(crate) struct GrayPlan {
    pub(crate) eligible: HashSet<ObjectId>,
    pub(crate) residual: HashMap<ObjectId, &'static str>,
    pub(crate) residual_pages_inline: BTreeSet<u32>,
}

fn resolve<'a>(doc: &'a Document, value: &'a Object) -> Option<&'a Object> {
    doc.dereference(value).ok().map(|(_, value)| value)
}
fn dict<'a>(doc: &'a Document, value: &'a Object) -> Option<&'a Dictionary> {
    match resolve(doc, value)? {
        Object::Dictionary(d) => Some(d),
        Object::Stream(s) => Some(&s.dict),
        _ => None,
    }
}

pub(crate) fn is_gray(doc: &Document, cs: &Object) -> bool {
    match resolve(doc, cs) {
        Some(Object::Name(n)) => matches!(n.as_slice(), b"DeviceGray" | b"G" | b"CalGray"),
        Some(Object::Array(a)) => match a.first().and_then(|o| o.as_name().ok()) {
            Some(b"CalGray") => true,
            Some(b"ICCBased") => {
                a.get(1)
                    .and_then(|o| dict(doc, o))
                    .and_then(|d| d.get(b"N").ok())
                    .and_then(|o| o.as_i64().ok())
                    == Some(1)
            }
            Some(b"Indexed" | b"I") => a.get(1).is_some_and(|o| is_gray(doc, o)),
            _ => false,
        },
        _ => false,
    }
}

fn identity_decode(doc: &Document, d: &Dictionary) -> bool {
    let Ok(value) = d.get(b"Decode") else {
        return true;
    };
    let Some(Object::Array(a)) = resolve(doc, value) else {
        return false;
    };
    !a.is_empty()
        && a.len() % 2 == 0
        && a.iter()
            .enumerate()
            .all(|(i, v)| v.as_float().ok() == Some(if i % 2 == 0 { 0.0 } else { 1.0 }))
}

struct Walker<'a> {
    doc: &'a Document,
    active: HashSet<ObjectId>,
    images: HashSet<ObjectId>,
    inline_color: bool,
}
impl<'a> Walker<'a> {
    fn new(doc: &'a Document) -> Self {
        Self {
            doc,
            active: HashSet::new(),
            images: HashSet::new(),
            inline_color: false,
        }
    }
    fn form(&mut self, value: &'a Object, inherited: Option<&'a Dictionary>, depth: usize) {
        if depth >= 32 {
            return;
        }
        let Ok((id, obj)) = self.doc.dereference(value) else {
            return;
        };
        if let Some(id) = id {
            if !self.active.insert(id) {
                return;
            }
        }
        if let Ok(stream) = obj.as_stream() {
            if let Ok(bytes) = stream.get_plain_content() {
                self.inline_color |= super::inline::scan_inline_images(&bytes).color_count > 0;
            }
            let res = stream
                .dict
                .get(b"Resources")
                .ok()
                .and_then(|o| dict(self.doc, o))
                .or(inherited);
            if let Some(res) = res {
                self.resources(res, depth + 1);
            }
        }
        if let Some(id) = id {
            self.active.remove(&id);
        }
    }
    fn resources(&mut self, res: &'a Dictionary, depth: usize) {
        if depth >= 32 {
            return;
        }
        if let Some(xobjects) = res.get(b"XObject").ok().and_then(|o| dict(self.doc, o)) {
            for (_, v) in xobjects.iter() {
                let Ok((id, obj)) = self.doc.dereference(v) else {
                    continue;
                };
                let Ok(s) = obj.as_stream() else {
                    continue;
                };
                match s.dict.get(b"Subtype").and_then(Object::as_name).ok() {
                    Some(b"Image") => {
                        if let Some(id) = id {
                            self.images.insert(id);
                        }
                    }
                    Some(b"Form") => self.form(v, Some(res), depth),
                    _ => {}
                }
            }
        }
        if let Some(states) = res.get(b"ExtGState").ok().and_then(|o| dict(self.doc, o)) {
            for (_, v) in states.iter() {
                self.mask(v, Some(res), depth);
            }
        }
    }
    fn mask(&mut self, value: &'a Object, inherited: Option<&'a Dictionary>, depth: usize) {
        let Some(g) = dict(self.doc, value)
            .and_then(|d| d.get(b"SMask").ok())
            .and_then(|o| dict(self.doc, o))
            .and_then(|d| d.get(b"G").ok())
        else {
            return;
        };
        self.form(g, inherited, depth);
    }
}

fn page_walk(doc: &Document, page: ObjectId) -> Walker<'_> {
    let mut walker = Walker::new(doc);
    if let Ok((inline, ids)) = doc.get_page_resources(page) {
        if let Some(res) = inline {
            walker.resources(res, 0);
        } else if let Some(res) = ids
            .first()
            .and_then(|id| doc.get_object(*id).ok())
            .and_then(|o| dict(doc, o))
        {
            walker.resources(res, 0);
        }
    }
    if let Ok(bytes) = doc.get_page_content(page) {
        walker.inline_color |= super::inline::scan_inline_images(&bytes).color_count > 0;
    }
    walker
}

pub(crate) fn pages_using(doc: &Document, ids: &HashSet<ObjectId>) -> BTreeSet<u32> {
    if ids.is_empty() {
        return BTreeSet::new();
    }
    doc.get_pages()
        .into_iter()
        .filter_map(|(n, id)| (!page_walk(doc, id).images.is_disjoint(ids)).then_some(n))
        .collect()
}

// Los ExtGState también pueden ser diccionarios directos dentro de Resources.
fn collect_nested_masks<'a>(value: &'a Object, walker: &mut Walker<'a>, depth: usize) {
    if depth >= 32 {
        return;
    }
    walker.mask(value, None, 0);
    match value {
        Object::Dictionary(d) => {
            for (_, v) in d.iter() {
                if !matches!(v, Object::Reference(_)) {
                    collect_nested_masks(v, walker, depth + 1);
                }
            }
        }
        Object::Stream(s) => {
            for (_, v) in s.dict.iter() {
                if !matches!(v, Object::Reference(_)) {
                    collect_nested_masks(v, walker, depth + 1);
                }
            }
        }
        Object::Array(a) => {
            for v in a {
                if !matches!(v, Object::Reference(_)) {
                    collect_nested_masks(v, walker, depth + 1);
                }
            }
        }
        _ => {}
    }
}

pub(crate) fn gray_plan(
    doc: &Document,
    preserve: &HashSet<ObjectId>,
    smask_ids: &HashSet<ObjectId>,
) -> GrayPlan {
    let mut plan = GrayPlan::default();
    let mut masks = Walker::new(doc);
    for obj in doc.objects.values() {
        collect_nested_masks(obj, &mut masks, 0);
    }
    for (&id, obj) in &doc.objects {
        let Ok(s) = obj.as_stream() else {
            continue;
        };
        let d = &s.dict;
        if d.get(b"Subtype").and_then(Object::as_name).ok() != Some(b"Image")
            || d.get(b"ImageMask")
                .and_then(Object::as_bool)
                .unwrap_or(false)
        {
            continue;
        }
        if d.get(b"ColorSpace").ok().is_some_and(|cs| is_gray(doc, cs)) {
            continue;
        }
        let reason = if preserve.contains(&id) {
            Some("preserved_stamp")
        } else if smask_ids.contains(&id) || masks.images.contains(&id) {
            Some("mask_group")
        } else if !identity_decode(doc, d) {
            Some("decode")
        } else if d
            .get(b"Mask")
            .ok()
            .and_then(|o| resolve(doc, o))
            .is_some_and(|o| matches!(o, Object::Array(_)))
        {
            Some("color_key_mask")
        } else {
            None
        };
        if let Some(reason) = reason {
            plan.residual.insert(id, reason);
        } else {
            plan.eligible.insert(id);
        }
    }
    for (n, id) in doc.get_pages() {
        if page_walk(doc, id).inline_color {
            plan.residual_pages_inline.insert(n);
        }
    }
    plan
}
