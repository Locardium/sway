// Caratulas embebidas en los tags: se extraen con lofty, se reducen a thumb
// y se devuelven como data-URL (el frontend las cachea por track id).
use crate::id3_sanitize::sanitize_id3v2_date_frames;
use base64::Engine;
use lofty::file::{TaggedFile, TaggedFileExt};
use lofty::probe::Probe;
use std::io::Cursor;
use std::path::Path;

const THUMB_SIZE: u32 = 256;

fn read_tagged(path: &Path) -> Option<TaggedFile> {
    match lofty::read_from_path(path) {
        Ok(t) => Some(t),
        Err(e) => {
            // Mismo caso que import.rs::read_meta: un frame de fecha ID3
            // corrupto tira el tag entero. Sanitizar y reintentar en memoria
            // antes de rendirse (ver id3_sanitize.rs).
            log::warn!("cover: lofty read_from_path fallo para {}: {e}", path.display());
            let bytes = std::fs::read(path).ok()?;
            let patched = sanitize_id3v2_date_frames(&bytes)?;
            Probe::new(Cursor::new(patched.as_slice())).guess_file_type().ok()?.read().ok()
        }
    }
}

pub fn thumb_data_url(path: &Path) -> Option<String> {
    let tagged = read_tagged(path)?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    let pic = tag.pictures().first()?;
    let img = image::load_from_memory(pic.data()).ok()?;
    let thumb = img.thumbnail(THUMB_SIZE, THUMB_SIZE).to_rgb8();
    let mut buf = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 82)
        .encode_image(&thumb)
        .ok()?;
    Some(format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&buf)
    ))
}
