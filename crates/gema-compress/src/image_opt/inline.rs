//! Escáner inline compartido con telemetría; salta cadenas, comentarios y datos.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InlineScan {
    pub(crate) count: u64,
    pub(crate) data_bytes: u64,
    pub(crate) color_count: u64,
}

pub(crate) fn scan_inline_images(content: &[u8]) -> InlineScan {
    let mut scan = InlineScan::default();
    let mut cursor = 0;

    while let Some((token_start, token_end)) = next_token(content, cursor) {
        cursor = token_end;
        if &content[token_start..token_end] != b"BI" {
            continue;
        }

        let Some((data_start, after_id)) = find_inline_data_start(content, cursor) else {
            break;
        };
        let Some((data_end, after_ei)) = find_inline_data_end(content, data_start) else {
            break;
        };

        scan.count = scan.count.saturating_add(1);
        let mut header_cursor = token_end;
        let mut gray = false;
        let mut mask = false;
        while let Some((start, end)) = next_token(content, header_cursor) {
            if start >= data_start {
                break;
            }
            header_cursor = end;
            let token = &content[start..end];
            if matches!(token, b"/CS" | b"/ColorSpace" | b"/IM" | b"/ImageMask") {
                if let Some((vs, ve)) = next_token(content, header_cursor) {
                    header_cursor = ve;
                    if matches!(token, b"/IM" | b"/ImageMask") {
                        mask = &content[vs..ve] == b"true";
                    } else {
                        gray = matches!(&content[vs..ve], b"/G" | b"/DeviceGray");
                    }
                }
            }
        }
        if !gray && !mask {
            scan.color_count = scan.color_count.saturating_add(1);
        }
        scan.data_bytes = scan
            .data_bytes
            .saturating_add(u64::try_from(data_end.saturating_sub(data_start)).unwrap_or(u64::MAX));
        cursor = after_ei.max(after_id);
    }

    scan
}

fn find_inline_data_start(content: &[u8], mut cursor: usize) -> Option<(usize, usize)> {
    while let Some((token_start, token_end)) = next_token(content, cursor) {
        cursor = token_end;
        if &content[token_start..token_end] == b"ID" {
            let data_start = consume_inline_separator(content, token_end)?;
            return Some((data_start, token_end));
        }
        if &content[token_start..token_end] == b"BI" {
            return None;
        }
    }
    None
}

fn find_inline_data_end(content: &[u8], data_start: usize) -> Option<(usize, usize)> {
    let mut cursor = data_start;
    while cursor < content.len() {
        if is_pdf_whitespace(content[cursor]) {
            let operator_start = cursor.saturating_add(1);
            let operator_end = operator_start.saturating_add(2);
            if content.get(operator_start..operator_end) == Some(b"EI")
                && content
                    .get(operator_end)
                    .is_none_or(|byte| is_pdf_whitespace(*byte) || is_pdf_delimiter(*byte))
            {
                return Some((cursor, operator_end));
            }
        }
        cursor = cursor.saturating_add(1);
    }
    None
}

fn next_token(content: &[u8], mut cursor: usize) -> Option<(usize, usize)> {
    loop {
        while content
            .get(cursor)
            .is_some_and(|byte| is_pdf_whitespace(*byte))
        {
            cursor = cursor.saturating_add(1);
        }
        if content.get(cursor) == Some(&b'%') {
            cursor = skip_comment(content, cursor);
            continue;
        }
        break;
    }

    let first = *content.get(cursor)?;
    let start = cursor;
    let end = match first {
        b'(' => skip_literal_string(content, cursor),
        b'<' if content.get(cursor.saturating_add(1)) != Some(&b'<') => {
            skip_hex_string(content, cursor)
        }
        b'/' => skip_regular_token(content, cursor.saturating_add(1)),
        byte if is_pdf_delimiter(byte) => cursor.saturating_add(
            if matches!(byte, b'<' | b'>') && content.get(cursor.saturating_add(1)) == Some(&byte) {
                2
            } else {
                1
            },
        ),
        _ => skip_regular_token(content, cursor),
    };
    Some((start, end.min(content.len())))
}

fn skip_comment(content: &[u8], mut cursor: usize) -> usize {
    while content
        .get(cursor)
        .is_some_and(|byte| !matches!(*byte, b'\r' | b'\n'))
    {
        cursor = cursor.saturating_add(1);
    }
    cursor
}

fn skip_literal_string(content: &[u8], mut cursor: usize) -> usize {
    let mut depth = 0_u64;
    while let Some(byte) = content.get(cursor) {
        cursor = cursor.saturating_add(1);
        match byte {
            b'\\' => cursor = cursor.saturating_add(1).min(content.len()),
            b'(' => depth = depth.saturating_add(1),
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
    }
    cursor
}

fn skip_hex_string(content: &[u8], mut cursor: usize) -> usize {
    cursor = cursor.saturating_add(1);
    while let Some(byte) = content.get(cursor) {
        cursor = cursor.saturating_add(1);
        if *byte == b'>' {
            break;
        }
    }
    cursor
}

fn skip_regular_token(content: &[u8], mut cursor: usize) -> usize {
    while content
        .get(cursor)
        .is_some_and(|byte| !is_pdf_whitespace(*byte) && !is_pdf_delimiter(*byte))
    {
        cursor = cursor.saturating_add(1);
    }
    cursor
}

fn consume_inline_separator(content: &[u8], cursor: usize) -> Option<usize> {
    match content.get(cursor) {
        Some(b'\r') if content.get(cursor.saturating_add(1)) == Some(&b'\n') => {
            Some(cursor.saturating_add(2))
        }
        Some(byte) if is_pdf_whitespace(*byte) => Some(cursor.saturating_add(1)),
        _ => None,
    }
}

fn is_pdf_whitespace(byte: u8) -> bool {
    matches!(byte, 0x00 | b'\t' | b'\n' | 0x0c | b'\r' | b' ')
}

fn is_pdf_delimiter(byte: u8) -> bool {
    matches!(
        byte,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}
