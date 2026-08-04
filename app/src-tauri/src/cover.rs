// Caratulas embebidas en los tags: se extraen con lofty, se reducen a thumb
// y se devuelven como data-URL (el frontend las cachea por track id).
use base64::Engine;
use lofty::file::TaggedFileExt;
use std::path::Path;

const THUMB_SIZE: u32 = 256;

pub fn thumb_data_url(path: &Path) -> Option<String> {
    let tagged = match lofty::read_from_path(path) {
        Ok(t) => t,
        Err(e) => {
            log::warn!("cover: lofty read_from_path fallo para {}: {e}", path.display());
            return None;
        }
    };
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
