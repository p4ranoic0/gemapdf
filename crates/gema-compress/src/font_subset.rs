//! Subsetting TrueType que conserva los GID originales.

use std::collections::BTreeSet;

fn u16_at(data: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes(
        data.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn u32_at(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        data.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn checksum(data: &[u8]) -> u32 {
    data.chunks(4).fold(0u32, |sum, chunk| {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        sum.wrapping_add(u32::from_be_bytes(word))
    })
}

fn component_gids(glyph: &[u8]) -> Option<Vec<u16>> {
    if glyph.len() < 10 {
        return None;
    }
    let contours = i16::from_be_bytes(glyph[0..2].try_into().ok()?);
    if contours >= 0 {
        return Some(Vec::new());
    }
    let mut pos = 10usize;
    let mut components = Vec::new();
    loop {
        let flags = u16_at(glyph, pos)?;
        let gid = u16_at(glyph, pos + 2)?;
        components.push(gid);
        pos = pos.checked_add(4)?;
        pos = pos.checked_add(if flags & 0x0001 != 0 { 4 } else { 2 })?;
        pos = pos.checked_add(if flags & 0x0008 != 0 {
            2
        } else if flags & 0x0040 != 0 {
            4
        } else if flags & 0x0080 != 0 {
            8
        } else {
            0
        })?;
        if pos > glyph.len() {
            return None;
        }
        if flags & 0x0020 == 0 {
            if flags & 0x0100 != 0 {
                let instructions = u16_at(glyph, pos)? as usize;
                pos = pos.checked_add(2)?.checked_add(instructions)?;
                if pos > glyph.len() {
                    return None;
                }
            }
            break;
        }
    }
    Some(components)
}

/// Elimina contornos no usados conservando sus GID y todas las demás tablas.
/// `None` significa fuente inválida, no compatible o sin ahorro real.
pub(crate) fn subset_truetype_keep_gids(
    font_bytes: &[u8],
    used_gids: &BTreeSet<u16>,
) -> Option<Vec<u8>> {
    if u32_at(font_bytes, 0)? != 0x0001_0000 {
        return None;
    }
    let table_count = u16_at(font_bytes, 4)? as usize;
    let directory_end = 12usize.checked_add(table_count.checked_mul(16)?)?;
    font_bytes.get(..directory_end)?;
    let mut tables = Vec::with_capacity(table_count);
    let mut tags = BTreeSet::new();
    for i in 0..table_count {
        let pos = 12 + i * 16;
        let tag: [u8; 4] = font_bytes[pos..pos + 4].try_into().ok()?;
        if !tags.insert(tag) {
            return None;
        }
        let offset = u32_at(font_bytes, pos + 8)? as usize;
        let length = u32_at(font_bytes, pos + 12)? as usize;
        let data = font_bytes.get(offset..offset.checked_add(length)?)?;
        tables.push((tag, data.to_vec()));
    }
    let index = |tag: &[u8; 4]| tables.iter().position(|(name, _)| name == tag);
    let (head_i, maxp_i, loca_i, glyf_i) = (
        index(b"head")?,
        index(b"maxp")?,
        index(b"loca")?,
        index(b"glyf")?,
    );
    let head = &tables[head_i].1;
    if head.len() < 54 || u32_at(head, 12)? != 0x5F0F_3CF5 {
        return None;
    }
    let glyph_count = u16_at(&tables[maxp_i].1, 4)? as usize;
    if glyph_count == 0 {
        return None;
    }
    let original_format = u16_at(head, 50)?;
    if original_format > 1 {
        return None;
    }
    let loca = &tables[loca_i].1;
    let glyf = &tables[glyf_i].1;
    let entry_size = if original_format == 0 { 2 } else { 4 };
    loca.get(..(glyph_count + 1).checked_mul(entry_size)?)?;
    let offsets = (0..=glyph_count)
        .map(|i| {
            let value = if original_format == 0 {
                (u16_at(loca, i * 2)? as u32) * 2
            } else {
                u32_at(loca, i * 4)?
            };
            (value as usize <= glyf.len()).then_some(value)
        })
        .collect::<Option<Vec<_>>>()?;
    if offsets.windows(2).any(|pair| pair[0] > pair[1]) {
        return None;
    }

    let mut keep = used_gids.clone();
    keep.insert(0);
    let mut pending: Vec<u16> = keep.iter().copied().collect();
    let mut seen = BTreeSet::new();
    while let Some(gid) = pending.pop() {
        let i = gid as usize;
        if i >= glyph_count {
            return None;
        }
        if !seen.insert(gid) {
            continue;
        }
        let glyph = glyf.get(offsets[i] as usize..offsets[i + 1] as usize)?;
        if glyph.is_empty() {
            continue;
        }
        for component in component_gids(glyph)? {
            if component as usize >= glyph_count {
                return None;
            }
            keep.insert(component);
            pending.push(component);
        }
    }

    let mut new_glyf = Vec::new();
    let mut new_offsets = Vec::with_capacity(glyph_count + 1);
    new_offsets.push(0u32);
    for gid in 0..glyph_count {
        if keep.contains(&(gid as u16)) {
            new_glyf.extend_from_slice(&glyf[offsets[gid] as usize..offsets[gid + 1] as usize]);
        }
        new_offsets.push(u32::try_from(new_glyf.len()).ok()?);
    }
    let short = original_format == 0
        && new_offsets
            .iter()
            .all(|v| v % 2 == 0 && v / 2 <= u16::MAX as u32);
    let mut new_loca = Vec::with_capacity(new_offsets.len() * if short { 2 } else { 4 });
    for offset in new_offsets {
        if short {
            new_loca.extend_from_slice(&u16::try_from(offset / 2).ok()?.to_be_bytes());
        } else {
            new_loca.extend_from_slice(&offset.to_be_bytes());
        }
    }
    tables[head_i].1[8..12].fill(0);
    tables[head_i].1[50..52].copy_from_slice(&(if short { 0u16 } else { 1u16 }).to_be_bytes());
    tables[loca_i].1 = new_loca;
    tables[glyf_i].1 = new_glyf;

    let mut output = font_bytes[..directory_end].to_vec();
    for (i, (_, data)) in tables.iter().enumerate() {
        while !output.len().is_multiple_of(4) {
            output.push(0);
        }
        let offset = u32::try_from(output.len()).ok()?;
        let length = u32::try_from(data.len()).ok()?;
        let pos = 12 + i * 16;
        output[pos + 4..pos + 8].copy_from_slice(&checksum(data).to_be_bytes());
        output[pos + 8..pos + 12].copy_from_slice(&offset.to_be_bytes());
        output[pos + 12..pos + 16].copy_from_slice(&length.to_be_bytes());
        output.extend_from_slice(data);
    }
    while !output.len().is_multiple_of(4) {
        output.push(0);
    }
    if output.len() >= font_bytes.len() {
        return None;
    }
    let head_pos = u32_at(&output, 12 + head_i * 16 + 8)? as usize;
    let adjustment = 0xB1B0_AFBAu32.wrapping_sub(checksum(&output));
    output
        .get_mut(head_pos + 8..head_pos + 12)?
        .copy_from_slice(&adjustment.to_be_bytes());
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<u8> {
        let mut head = vec![0u8; 54];
        head[0..4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        head[12..16].copy_from_slice(&0x5F0F_3CF5u32.to_be_bytes());
        head[18..20].copy_from_slice(&1000u16.to_be_bytes());
        head[50..52].copy_from_slice(&1u16.to_be_bytes());
        let mut hhea = vec![0u8; 36];
        hhea[0..4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        hhea[4..6].copy_from_slice(&800i16.to_be_bytes());
        hhea[6..8].copy_from_slice(&(-200i16).to_be_bytes());
        hhea[34..36].copy_from_slice(&5u16.to_be_bytes());
        let mut maxp = vec![0u8; 32];
        maxp[0..4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        maxp[4..6].copy_from_slice(&5u16.to_be_bytes());
        let mut hmtx = Vec::new();
        for gid in 0..5u16 {
            hmtx.extend_from_slice(&(500 + gid * 10).to_be_bytes());
            hmtx.extend_from_slice(&0u16.to_be_bytes());
        }
        let mut glyf = Vec::new();
        let mut offsets = vec![0u32];
        for gid in 0..5u16 {
            if gid == 3 {
                glyf.extend_from_slice(&(-1i16).to_be_bytes());
                glyf.extend_from_slice(&[0u8; 8]);
                glyf.extend_from_slice(&0x0021u16.to_be_bytes());
                glyf.extend_from_slice(&1u16.to_be_bytes());
                glyf.extend_from_slice(&[0u8; 4]);
                glyf.extend_from_slice(&0x0001u16.to_be_bytes());
                glyf.extend_from_slice(&2u16.to_be_bytes());
                glyf.extend_from_slice(&[0u8; 4]);
                glyf.extend_from_slice(&[0u8; 2]);
            } else {
                glyf.extend_from_slice(&0i16.to_be_bytes());
                glyf.extend_from_slice(&[gid as u8; 8]);
                glyf.extend_from_slice(&0u16.to_be_bytes());
            }
            offsets.push(glyf.len() as u32);
        }
        let loca = offsets
            .iter()
            .flat_map(|v| v.to_be_bytes())
            .collect::<Vec<_>>();
        let tables: Vec<([u8; 4], Vec<u8>)> = vec![
            (*b"head", head),
            (*b"hhea", hhea),
            (*b"maxp", maxp),
            (*b"hmtx", hmtx),
            (*b"loca", loca),
            (*b"glyf", glyf),
        ];
        let mut bytes = vec![0u8; 12 + 16 * tables.len()];
        bytes[0..4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        bytes[4..6].copy_from_slice(&(tables.len() as u16).to_be_bytes());
        for (i, (tag, data)) in tables.iter().enumerate() {
            let offset = bytes.len() as u32;
            let rec = 12 + i * 16;
            bytes[rec..rec + 4].copy_from_slice(tag);
            bytes[rec + 8..rec + 12].copy_from_slice(&offset.to_be_bytes());
            bytes[rec + 12..rec + 16].copy_from_slice(&(data.len() as u32).to_be_bytes());
            bytes.extend_from_slice(data);
            while !bytes.len().is_multiple_of(4) {
                bytes.push(0);
            }
        }
        bytes
    }

    fn table<'a>(bytes: &'a [u8], tag: &[u8; 4]) -> &'a [u8] {
        let count = u16::from_be_bytes(bytes[4..6].try_into().unwrap()) as usize;
        for i in 0..count {
            let p = 12 + i * 16;
            if &bytes[p..p + 4] != tag {
                continue;
            }
            let offset = u32::from_be_bytes(bytes[p + 8..p + 12].try_into().unwrap()) as usize;
            let len = u32::from_be_bytes(bytes[p + 12..p + 16].try_into().unwrap()) as usize;
            return &bytes[offset..offset + len];
        }
        panic!("missing test table")
    }

    fn glyph(bytes: &[u8], gid: usize) -> &[u8] {
        let loca = table(bytes, b"loca");
        let start = u32::from_be_bytes(loca[gid * 4..gid * 4 + 4].try_into().unwrap()) as usize;
        let end =
            u32::from_be_bytes(loca[(gid + 1) * 4..(gid + 1) * 4 + 4].try_into().unwrap()) as usize;
        &table(bytes, b"glyf")[start..end]
    }

    #[test]
    fn removes_unused_glyphs_but_preserves_gids_and_advances() {
        let original = fixture();
        let result = subset_truetype_keep_gids(&original, &BTreeSet::from([1])).unwrap();
        assert!(result.len() < original.len());
        assert_eq!(glyph(&result, 0), glyph(&original, 0));
        assert_eq!(glyph(&result, 1), glyph(&original, 1));
        for gid in 2..5 {
            assert!(glyph(&result, gid).is_empty());
        }
        assert_eq!(table(&result, b"hmtx"), table(&original, b"hmtx"));
        let face = ttf_parser::Face::parse(&result, 0).unwrap();
        assert_eq!(face.number_of_glyphs(), 5);
        for gid in 0..5 {
            assert_eq!(
                face.glyph_hor_advance(ttf_parser::GlyphId(gid)),
                Some(500 + gid * 10)
            );
        }
    }

    #[test]
    fn compound_glyph_keeps_all_components() {
        let original = fixture();
        let result = subset_truetype_keep_gids(&original, &BTreeSet::from([3])).unwrap();
        for gid in 0..4 {
            assert_eq!(glyph(&result, gid), glyph(&original, gid));
        }
        assert!(glyph(&result, 4).is_empty());
        assert!(ttf_parser::Face::parse(&result, 0).is_ok());
    }

    #[test]
    fn no_smaller_program_abstains() {
        let original = fixture();
        assert!(subset_truetype_keep_gids(&original, &BTreeSet::from([0, 1, 2, 3, 4])).is_none());
    }
}
