//! Neutralizes ID3v2 date frames with invalid content (not digits or date
//! separators) that make lofty discard the ENTIRE tag over a single
//! corrupted field.
//!
//! Real case that motivated this: a track had the `TORY` frame (Original
//! Release Year) with the text "Cmp3.eu" instead of a year — overwritten by
//! the tagging tool of a download site — and `lofty::read_from_path` throws
//! `BadTimestamp` and returns NOTHING (no title, no artist, no cover art, no
//! duration), even though the rest of the tag is perfectly fine.
//! `ParsingMode::Relaxed` doesn't help: this is one of the few errors lofty
//! treats as hard regardless of mode.

const DATE_FRAMES: &[[u8; 4]] = &[
    *b"TYER", *b"TDAT", *b"TIME", *b"TORY", *b"TRDA", *b"TDRC", *b"TDOR", *b"TDRL", *b"TDEN", *b"TDTG",
];

fn syncsafe_u32(b: &[u8]) -> Option<u32> {
    if b.len() < 4 {
        return None;
    }
    Some(
        ((b[0] as u32 & 0x7f) << 21)
            | ((b[1] as u32 & 0x7f) << 14)
            | ((b[2] as u32 & 0x7f) << 7)
            | (b[3] as u32 & 0x7f),
    )
}

/// True if the content of a date frame (1 encoding byte + text) is
/// plausible text for an ID3 date (only digits + common separators),
/// decoding according to the encoding byte (0=Latin1, 1=UTF-16+BOM,
/// 2=UTF-16BE, 3=UTF-8).
fn is_plausible_date_text(content: &[u8]) -> bool {
    let Some((&enc, text)) = content.split_first() else {
        return true;
    };
    let chars: Vec<char> = match enc {
        0 | 3 => text.iter().map(|&b| b as char).collect(),
        1 => {
            if text.len() < 2 {
                return true;
            }
            let be = text[0] == 0xFE && text[1] == 0xFF;
            let units: Vec<u16> = text[2..]
                .chunks_exact(2)
                .map(|c| {
                    if be {
                        u16::from_be_bytes([c[0], c[1]])
                    } else {
                        u16::from_le_bytes([c[0], c[1]])
                    }
                })
                .collect();
            char::decode_utf16(units).filter_map(|r| r.ok()).collect()
        }
        2 => {
            let units: Vec<u16> = text.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
            char::decode_utf16(units).filter_map(|r| r.ok()).collect()
        }
        _ => return true, // unknown encoding: don't touch, better not risk it
    };
    chars.iter().all(|&c| c == '\0' || c.is_ascii_digit() || matches!(c, '-' | ':' | 'T' | 'Z' | ' '))
}

/// Walks the frames of an ID3v2 tag and neutralizes (encoding -> Latin1,
/// content -> zeros) date frames with invalid text. Returns
/// `Some(patched bytes)` if it found and fixed something, or `None` if there
/// was no recognizable ID3v2 tag or the date frames were already fine (no
/// need to reparse anything different from what was already tried).
pub fn sanitize_id3v2_date_frames(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.len() < 10 || &bytes[0..3] != b"ID3" {
        return None;
    }
    let major = bytes[3];
    let flags = bytes[5];
    let tag_size = syncsafe_u32(bytes.get(6..10)?)? as usize;
    let mut pos = 10usize;
    if flags & 0x40 != 0 {
        // Extended header present: skip it (uncommon in practice).
        let ext_size = if major >= 4 {
            syncsafe_u32(bytes.get(pos..pos + 4)?)? as usize
        } else {
            u32::from_be_bytes(bytes.get(pos..pos + 4)?.try_into().ok()?) as usize
        };
        pos += ext_size.max(4);
    }
    let tag_end = (10 + tag_size).min(bytes.len());
    let mut out: Option<Vec<u8>> = None;
    while pos + 10 <= tag_end {
        let id = &bytes[pos..pos + 4];
        if id == [0, 0, 0, 0] {
            break; // padding: no more real frames
        }
        let frame_size = if major >= 4 {
            syncsafe_u32(bytes.get(pos + 4..pos + 8)?)? as usize
        } else {
            u32::from_be_bytes(bytes.get(pos + 4..pos + 8)?.try_into().ok()?) as usize
        };
        let frame_start = pos + 10;
        let frame_end = (frame_start + frame_size).min(tag_end);
        let id_arr: [u8; 4] = id.try_into().ok()?;
        if DATE_FRAMES.contains(&id_arr) && frame_end > frame_start {
            let content = &bytes[frame_start..frame_end];
            if !is_plausible_date_text(content) {
                let buf = out.get_or_insert_with(|| bytes.to_vec());
                buf[frame_start] = 0; // encoding -> Latin1 (no BOM to validate)
                for b in &mut buf[frame_start + 1..frame_end] {
                    *b = 0;
                }
                log::warn!(
                    "id3_sanitize: frame {} had an invalid date, neutralized",
                    String::from_utf8_lossy(id)
                );
            }
        }
        pos = frame_end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn syncsafe(n: u32) -> [u8; 4] {
        [
            ((n >> 21) & 0x7f) as u8,
            ((n >> 14) & 0x7f) as u8,
            ((n >> 7) & 0x7f) as u8,
            (n & 0x7f) as u8,
        ]
    }

    fn frame(id: &[u8; 4], content: &[u8]) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(id);
        f.extend_from_slice(&(content.len() as u32).to_be_bytes()); // ID3v2.3: size normal, no syncsafe
        f.extend_from_slice(&[0, 0]); // flags
        f.extend_from_slice(content);
        f
    }

    fn build_tag(frames: &[u8]) -> Vec<u8> {
        let mut t = Vec::new();
        t.extend_from_slice(b"ID3");
        t.extend_from_slice(&[3, 0]); // v2.3
        t.push(0); // flags
        t.extend_from_slice(&syncsafe(frames.len() as u32));
        t.extend_from_slice(frames);
        t
    }

    #[test]
    fn neutralizes_garbage_tory_leaves_valid_tyer_untouched() {
        // TORY with "AB" in UTF-16LE with BOM (invalid: letters, not digits).
        let tory_content: Vec<u8> = {
            let mut c = vec![1u8]; // encoding = UTF-16 + BOM
            c.extend_from_slice(&[0xff, 0xfe]); // BOM LE
            c.extend_from_slice(&[b'A', 0, b'B', 0]);
            c
        };
        let tyer_content: Vec<u8> = {
            let mut c = vec![0u8]; // encoding = Latin1
            c.extend_from_slice(b"2020");
            c
        };
        let mut frames = Vec::new();
        frames.extend(frame(b"TORY", &tory_content));
        frames.extend(frame(b"TYER", &tyer_content));
        let tag = build_tag(&frames);

        let patched = sanitize_id3v2_date_frames(&tag).expect("should sanitize TORY");

        // TORY was neutralized (Latin1 encoding, rest zeroed).
        let tory_start = 10 + 10; // header + frame header de TORY
        assert_eq!(patched[tory_start], 0);
        assert!(patched[tory_start + 1..tory_start + 1 + tory_content.len() - 1]
            .iter()
            .all(|&b| b == 0));

        // TYER (valid) was not touched: same bytes as the original.
        let tyer_start = tory_start + tory_content.len() + 10;
        assert_eq!(
            &patched[tyer_start..tyer_start + tyer_content.len()],
            tyer_content.as_slice()
        );
    }

    #[test]
    fn no_op_when_all_date_frames_are_valid() {
        let tyer_content: Vec<u8> = {
            let mut c = vec![0u8];
            c.extend_from_slice(b"2020");
            c
        };
        let tag = build_tag(&frame(b"TYER", &tyer_content));
        assert!(sanitize_id3v2_date_frames(&tag).is_none());
    }

    #[test]
    fn no_op_when_not_id3v2() {
        assert!(sanitize_id3v2_date_frames(b"not an id3 tag at all").is_none());
    }
}
