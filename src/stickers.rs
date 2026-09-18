//! Sticker identity, thumbnails, and the paths built from them.
//!
//! A sticker is identified by the SHA-256 of its bytes. Saved and cached
//! copies are filed under that hash, so the same picture can never be listed
//! twice, and a small static thumbnail beside it is what the picker draws.

use std::path::{Path, PathBuf};

/// Name of the file that keeps a pack title and its order.
pub const PACK_MANIFEST: &str = "pack.json";

/// Longest side of a thumbnail, in pixels.
pub const THUMB_SIDE: u32 = 128;

/// The SHA-256 of some bytes, as lowercase hex.
pub fn hash_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The content hash a file name carries, when it has one.
pub fn id_of(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    (stem.len() == 64 && stem.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| stem.to_ascii_lowercase())
}

/// Where the thumbnail of a sticker lives, once it has one.
pub fn thumb_path(thumbs: &Path, file: &Path) -> Option<PathBuf> {
    let id = id_of(file)?;
    Some(thumbs.join(format!("{id}.png")))
}

/// Files one sticker copy under its content hash, inside its own folder.
///
/// A copy that is already named after its hash stays where it is. One that
/// carries a message name is renamed once, so the thumbnail, the picker, and
/// the duplicate check all see the same identity for the same picture.
pub fn file_by_hash(dir: &Path, file: &Path) -> Result<PathBuf, String> {
    if id_of(file).is_some() {
        return Ok(file.to_path_buf());
    }
    let bytes = std::fs::read(file).map_err(|error| error.to_string())?;
    let target = dir.join(format!("{}.webp", hash_of(&bytes)));
    if target != file {
        if target.is_file() {
            // The same picture is already filed: this copy goes.
            std::fs::remove_file(file).map_err(|error| error.to_string())?;
        } else {
            std::fs::rename(file, &target).map_err(|error| error.to_string())?;
        }
    }
    Ok(target)
}

/// Builds the thumbnail of one sticker, unless it is already there.
///
/// Only the first frame of an animated sticker is decoded, so this stays
/// cheap for the whole picker.
pub fn build_thumb(thumbs: &Path, file: &Path) -> Result<PathBuf, String> {
    let target = thumb_path(thumbs, file)
        .ok_or_else(|| "the sticker is not filed under its content hash".to_owned())?;
    if target.is_file() {
        return Ok(target);
    }
    let bytes = std::fs::read(file).map_err(|error| error.to_string())?;
    let picture = first_frame(&bytes).ok_or_else(|| "could not read the picture".to_owned())?;
    let picture = if picture.width().max(picture.height()) > THUMB_SIDE {
        image::DynamicImage::ImageRgba8(picture)
            .resize(
                THUMB_SIDE,
                THUMB_SIDE,
                image::imageops::FilterType::Triangle,
            )
            .to_rgba8()
    } else {
        picture
    };
    std::fs::create_dir_all(thumbs).map_err(|error| error.to_string())?;
    // Written through a temporary file so a reader never sees a half one.
    let staging = target.with_extension("part");
    let file = std::fs::File::create(&staging).map_err(|error| error.to_string())?;
    image::DynamicImage::ImageRgba8(picture)
        .write_to(&mut std::io::BufWriter::new(file), image::ImageFormat::Png)
        .map_err(|error| error.to_string())?;
    std::fs::rename(&staging, &target).map_err(|error| error.to_string())?;
    Ok(target)
}

/// A picture as straight RGBA for the clipboard, first frame only.
pub fn clipboard_pixels(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let picture = first_frame(bytes)?;
    Some((picture.width(), picture.height(), picture.into_raw()))
}

/// The first frame of a WebP, or any other picture, as RGBA.
fn first_frame(bytes: &[u8]) -> Option<image::RgbaImage> {
    use image::ImageDecoder as _;

    if !(bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP") {
        return image::load_from_memory(bytes)
            .ok()
            .map(|image| image.to_rgba8());
    }
    let decoder = image::codecs::webp::WebPDecoder::new(std::io::Cursor::new(bytes)).ok()?;
    if decoder.has_animation() {
        // One frame only: the rest of an animation is never needed here.
        use image::AnimationDecoder;
        let frame = decoder.into_frames().next()?;
        return frame.ok().map(|frame| frame.into_buffer());
    }
    let (width, height) = decoder.dimensions();
    let mut rgba = vec![0u8; decoder.total_bytes() as usize];
    decoder.read_image(&mut rgba).ok()?;
    image::RgbaImage::from_raw(width, height, rgba)
}

/// Brings a pack folder up to date: files are filed under their content hash
/// and a manifest keeps the title and the order the author gave.
///
/// Old folders (numbered files, no manifest) are renamed in place once, so
/// every sticker in the app has one identity: the hash of its bytes.
pub fn adopt_pack(dir: &Path, fallback_title: &str) -> Result<(String, Vec<PathBuf>), String> {
    let manifest = dir.join(PACK_MANIFEST);
    if let Ok(raw) = std::fs::read_to_string(&manifest)
        && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&raw)
    {
        let title = parsed["title"]
            .as_str()
            .unwrap_or(fallback_title)
            .to_owned();
        let mut files: Vec<PathBuf> = parsed["order"]
            .as_array()
            .map(|order| {
                order
                    .iter()
                    .filter_map(|hash| hash.as_str())
                    .map(|hash| dir.join(format!("{hash}.webp")))
                    .filter(|path| path.is_file())
                    .collect()
            })
            .unwrap_or_default();
        // Anything else in the folder is still shown, after the listed ones.
        let mut extra = webp_files(dir)?;
        extra.retain(|path| !files.contains(path));
        files.append(&mut extra);
        return Ok((title, files));
    }
    let files = webp_files(dir)?;
    if files.is_empty() {
        return Ok((fallback_title.to_owned(), files));
    }
    let mut order = Vec::new();
    for path in files {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let hash = hash_of(&bytes);
        let target = dir.join(format!("{hash}.webp"));
        if path != target {
            let _ = std::fs::rename(&path, &target);
        }
        order.push(hash);
    }
    let record = serde_json::json!({ "title": fallback_title, "order": order });
    let _ = std::fs::write(&manifest, record.to_string());
    let files = order
        .iter()
        .map(|hash| dir.join(format!("{hash}.webp")))
        .filter(|path| path.is_file())
        .collect();
    Ok((fallback_title.to_owned(), files))
}

/// Every WebP file in a folder, in name order.
fn webp_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = std::fs::read_dir(dir).map_err(|error| error.to_string())?;
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "webp")
        })
        .collect();
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn webp_picture() -> Vec<u8> {
        let mut out = Vec::new();
        let picture = image::RgbaImage::from_pixel(600, 300, image::Rgba([10, 200, 90, 255]));
        image::codecs::webp::WebPEncoder::new_lossless(&mut out)
            .encode(&picture, 600, 300, image::ExtendedColorType::Rgba8)
            .expect("encodes");
        out
    }

    #[test]
    fn a_sticker_is_known_by_the_hash_in_its_name() {
        let hash = hash_of(b"sticker");
        assert_eq!(hash.len(), 64);
        let path = PathBuf::from(format!("{hash}.webp"));
        assert_eq!(id_of(&path).as_deref(), Some(hash.as_str()));
        assert_eq!(id_of(Path::new("000.webp")), None);
        assert_eq!(
            thumb_path(Path::new("/thumbs"), &path),
            Some(PathBuf::from(format!("/thumbs/{hash}.png")))
        );
    }

    #[test]
    fn a_thumbnail_is_small_and_written_once() {
        let dir = std::env::temp_dir().join(format!("zapfast-thumb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates");
        let bytes = webp_picture();
        let file = dir.join(format!("{}.webp", hash_of(&bytes)));
        std::fs::write(&file, &bytes).expect("writes");
        let thumbs = dir.join("thumbs");
        let built = build_thumb(&thumbs, &file).expect("builds");
        let picture = image::open(&built).expect("reads").to_rgba8();
        assert!(picture.width() <= THUMB_SIDE && picture.height() <= THUMB_SIDE);
        assert!(picture.height() * 2 == picture.width(), "the shape is kept");
        let stamp = std::fs::metadata(&built)
            .expect("stat")
            .modified()
            .expect("time");
        // Asking again keeps the file that is already there.
        let again = build_thumb(&thumbs, &file).expect("builds");
        assert_eq!(again, built);
        assert_eq!(
            std::fs::metadata(&built)
                .expect("stat")
                .modified()
                .expect("time"),
            stamp
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    fn small_picture(rgb: [u8; 3]) -> Vec<u8> {
        let mut out = Vec::new();
        let picture =
            image::RgbaImage::from_pixel(200, 200, image::Rgba([rgb[0], rgb[1], rgb[2], 255]));
        image::codecs::webp::WebPEncoder::new_lossless(&mut out)
            .encode(&picture, 200, 200, image::ExtendedColorType::Rgba8)
            .expect("encodes");
        out
    }

    #[test]
    fn a_sticker_copy_is_filed_under_its_hash() {
        let dir = std::env::temp_dir().join(format!("zapfast-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates");
        let bytes = small_picture([10, 200, 90]);
        let named = dir.join("5541999999999-3EB0C19.webp");
        std::fs::write(&named, &bytes).expect("writes");
        let filed = file_by_hash(&dir, &named).expect("files");
        assert_eq!(filed, dir.join(format!("{}.webp", hash_of(&bytes))));
        assert!(filed.is_file());
        assert!(!named.exists(), "the message name is gone");
        // Asking again keeps the file that is already there.
        assert_eq!(file_by_hash(&dir, &filed).expect("files"), filed);
        // A second copy of the same picture goes instead of piling up.
        let other = dir.join("another.webp");
        std::fs::write(&other, &bytes).expect("writes");
        assert_eq!(file_by_hash(&dir, &other).expect("files"), filed);
        assert!(!other.exists());
        assert!(filed.is_file());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_old_pack_is_filed_under_its_content_and_keeps_its_order() {
        let dir = std::env::temp_dir().join(format!("zapfast-pack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates");
        let first = small_picture([10, 200, 90]);
        let second = small_picture([200, 20, 20]);
        std::fs::write(dir.join("000.webp"), &first).expect("writes");
        std::fs::write(dir.join("001.webp"), &second).expect("writes");
        let (title, files) = adopt_pack(&dir, "Happy Frogs").expect("adopts");
        assert_eq!(title, "Happy Frogs");
        let names: Vec<String> = files
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                format!("{}.webp", hash_of(&first)),
                format!("{}.webp", hash_of(&second))
            ]
        );
        assert!(dir.join(PACK_MANIFEST).is_file(), "a manifest is written");
        // Reading again keeps the order and renames nothing.
        let (title, again) = adopt_pack(&dir, "Happy Frogs").expect("adopts");
        assert_eq!(title, "Happy Frogs");
        assert_eq!(again, files);
        let _ = std::fs::remove_dir_all(dir);
    }
}
