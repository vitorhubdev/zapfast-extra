//! Sticker-pack import from .wastickers and zip archives.
//!
//! Imported packs become named directories of WebP files.

use std::io::Read;
use std::path::{Path, PathBuf};

/// Imports a .wastickers or zip archive. Uses `title.txt` or the filename as title.
pub fn import_archive(path: &Path, packs: &Path) -> Result<String, String> {
    let file =
        std::fs::File::open(path).map_err(|error| format!("Could not open the file: {error}"))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| format!("This file is not a sticker archive: {error}"))?;
    let mut title: Option<String> = None;
    let mut pictures: Vec<(String, Vec<u8>)> = Vec::new();
    for index in 0..archive.len() {
        let Ok(mut entry) = archive.by_index(index) else {
            continue;
        };
        let name = entry.name().to_lowercase();
        let mut bytes = Vec::new();
        if entry.read_to_end(&mut bytes).is_err() {
            continue;
        }
        if name.ends_with("title.txt") {
            let text = String::from_utf8_lossy(&bytes).trim().to_owned();
            if !text.is_empty() {
                title = Some(text);
            }
            continue;
        }
        let picture = [".webp", ".png", ".jpg", ".jpeg"]
            .iter()
            .any(|extension| name.ends_with(extension));
        // Exclude the pack thumbnail.
        if picture && !name.contains("tray") {
            pictures.push((name, bytes));
        }
    }
    pictures.sort_by(|a, b| a.0.cmp(&b.0));
    let title = title
        .or_else(|| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    let files: Vec<Vec<u8>> = pictures
        .into_iter()
        .filter_map(|(_, bytes)| webp_bytes(bytes))
        .collect();
    write_pack(packs, &title, files)
}

/// Writes pack images to a new directory named after the title.
fn write_pack(packs: &Path, title: &str, files: Vec<Vec<u8>>) -> Result<String, String> {
    if files.is_empty() {
        return Err("No stickers could be read from this pack".to_owned());
    }
    let dir = unique_pack_dir(packs, title)?;
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("Could not write the sticker pack: {error}"))?;
    // Files are filed under the hash of their bytes, so the same picture keeps
    // one identity wherever it shows up, and a manifest holds the pack title
    // and the order its author gave.
    let mut order = Vec::new();
    for bytes in &files {
        let hash = crate::stickers::hash_of(bytes);
        let file = dir.join(format!("{hash}.webp"));
        if !file.is_file() {
            std::fs::write(&file, bytes)
                .map_err(|error| format!("Could not write the sticker pack: {error}"))?;
        }
        // A pack that carries the same picture twice lists it once.
        if !order.contains(&hash) {
            order.push(hash);
        }
    }
    let manifest = serde_json::json!({ "title": title, "order": order });
    std::fs::write(
        dir.join(crate::stickers::PACK_MANIFEST),
        manifest.to_string(),
    )
    .map_err(|error| format!("Could not write the pack manifest: {error}"))?;
    Ok(dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| title.to_owned()))
}

/// Converts an image to WebP. Preserves WebP, converts animated APNG/GIF to
/// animated WebP, and scales other images to 512 pixels per side.
fn webp_bytes(bytes: Vec<u8>) -> Option<Vec<u8>> {
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some(bytes);
    }
    if let Some(frames) = animation_frames(&bytes) {
        return encode_animated(frames);
    }
    let picture = image::load_from_memory(&bytes).ok()?;
    let picture = if picture.width().max(picture.height()) > 512 {
        picture.resize(512, 512, image::imageops::FilterType::Lanczos3)
    } else {
        picture
    };
    let mut out = Vec::new();
    let picture = picture.to_rgba8();
    image::codecs::webp::WebPEncoder::new_lossless(&mut out)
        .encode(
            &picture,
            picture.width(),
            picture.height(),
            image::ExtendedColorType::Rgba8,
        )
        .ok()?;
    Some(out)
}

/// Returns frames and millisecond delays for animated APNG or GIF data.
fn animation_frames(bytes: &[u8]) -> Option<Vec<(image::RgbaImage, u32)>> {
    use image::AnimationDecoder;
    let cursor = std::io::Cursor::new(bytes);
    let frames = if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        let decoder = image::codecs::png::PngDecoder::new(cursor).ok()?;
        if !decoder.is_apng().ok()? {
            return None;
        }
        decoder.apng().ok()?.into_frames()
    } else if bytes.starts_with(b"GIF8") {
        image::codecs::gif::GifDecoder::new(cursor)
            .ok()?
            .into_frames()
    } else {
        return None;
    };
    let frames: Vec<(image::RgbaImage, u32)> = frames
        .take(200)
        .filter_map(|frame| frame.ok())
        .map(|frame| {
            let (numerator, denominator) = frame.delay().numer_denom_ms();
            (frame.into_buffer(), numerator / denominator.max(1))
        })
        .collect();
    (frames.len() > 1).then_some(frames)
}

/// Encodes frames as a lossy animated WebP at sticker size.
fn encode_animated(frames: Vec<(image::RgbaImage, u32)>) -> Option<Vec<u8>> {
    use webp_animation::prelude::*;
    let (source_width, source_height) = frames.first().map(|(frame, _)| frame.dimensions())?;
    let scale = (512.0 / f64::from(source_width.max(source_height))).min(1.0);
    let width = (f64::from(source_width) * scale).round().max(1.0) as u32;
    let height = (f64::from(source_height) * scale).round().max(1.0) as u32;
    let mut encoder = Encoder::new_with_options(
        (width, height),
        EncoderOptions {
            encoding_config: Some(EncodingConfig {
                quality: 80.0,
                encoding_type: EncodingType::Lossy(LossyEncodingConfig::default()),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .ok()?;
    let mut clock = 0i32;
    for (frame, delay) in frames {
        let frame = if frame.dimensions() == (width, height) {
            frame
        } else {
            image::imageops::resize(&frame, width, height, image::imageops::FilterType::Lanczos3)
        };
        encoder.add_frame(frame.as_raw(), clock).ok()?;
        clock += delay.clamp(10, 10_000) as i32;
    }
    Some(encoder.finalize(clock).ok()?.to_vec())
}

/// Creates a unique pack directory from a sanitized title.
fn unique_pack_dir(root: &Path, title: &str) -> Result<PathBuf, String> {
    let clean: String = title
        .trim()
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '/' | '\\' | ':'))
        .take(60)
        .collect();
    let base = if clean.trim().is_empty() {
        "Stickers".to_owned()
    } else {
        clean.trim().to_owned()
    };
    for attempt in 0..100 {
        let name = if attempt == 0 {
            base.clone()
        } else {
            format!("{base} {}", attempt + 1)
        };
        let dir = root.join(&name);
        if !dir.exists() {
            std::fs::create_dir_all(&dir)
                .map_err(|error| format!("Could not create the pack folder: {error}"))?;
            return Ok(dir);
        }
    }
    Err("Too many sticker packs have this name".to_owned())
}
/// Unpacks a WhatsApp sticker-pack zip into a folder, in the message order,
/// stamping the listed emoji tags into each file. Adapted from upstream
/// ZapFast (crmne/zapfast, MIT). Entry names are ignored, so a hostile
/// archive cannot escape the folder; numbered files are hash-filed later.
pub fn extract_whatsapp_pack(
    zip: &[u8],
    stickers: &[(String, Vec<String>)],
    tray: Option<&str>,
    name: &str,
    dir: &Path,
) -> Result<Vec<PathBuf>, String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip))
        .map_err(|error| format!("This is not a sticker pack: {error}"))?;
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    for index in 0..archive.len() {
        let Ok(mut entry) = archive.by_index(index) else {
            continue;
        };
        let file = entry.name().to_owned();
        if Some(file.as_str()) == tray || !file.to_lowercase().ends_with(".webp") {
            continue;
        }
        let mut bytes = Vec::new();
        if entry.read_to_end(&mut bytes).is_ok() {
            files.push((file, bytes));
        }
    }
    let rank = |file: &str| {
        stickers
            .iter()
            .position(|(listed, _)| listed == file)
            .unwrap_or(usize::MAX)
    };
    files.sort_by_key(|(file, _)| rank(file));
    if files.is_empty() {
        return Err("No stickers could be read from this pack".to_owned());
    }
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let mut written = Vec::new();
    for (index, (file, bytes)) in files.into_iter().enumerate() {
        let emojis = stickers
            .iter()
            .find(|(listed, _)| *listed == file)
            .map(|(_, emojis)| emojis.clone())
            .unwrap_or_default();
        let bytes = if emojis.is_empty() {
            bytes
        } else {
            let info = crate::sticker_meta::StickerInfo {
                pack_name: name.to_owned(),
                emojis,
                ..crate::sticker_meta::read(&bytes).unwrap_or_default()
            };
            crate::sticker_meta::write(&bytes, &info).unwrap_or(bytes)
        };
        let path = dir.join(format!("{index:03}.webp"));
        std::fs::write(&path, bytes).map_err(|error| error.to_string())?;
        written.push(path);
    }
    Ok(written)
}
/// Copies a pack folder into a new pack, hash-filing every sticker so the
/// copy shares one identity per picture with the rest of the library.
pub fn copy_pack(from: &Path, packs: &Path, name: &str) -> Result<String, String> {
    let mut stickers: Vec<PathBuf> = std::fs::read_dir(from)
        .map_err(|error| error.to_string())?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "webp")
        })
        .collect();
    stickers.sort();
    let files = stickers
        .iter()
        .map(std::fs::read)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    write_pack(packs, name, files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wastickers_archive_becomes_a_named_pack() {
        let root = std::env::temp_dir().join(format!("zapfast-packs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let archive_path = root.join("in").join("Frogs.wastickers");
        std::fs::create_dir_all(archive_path.parent().expect("dir")).expect("dirs");
        let webp = webp_bytes(tiny_png()).expect("encodes");
        let file = std::fs::File::create(&archive_path).expect("creates");
        let mut writer = zip::ZipWriter::new(file);
        let plain: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
        writer.start_file("title.txt", plain).expect("entry");
        std::io::Write::write_all(&mut writer, b"Happy Frogs\n").expect("writes");
        writer.start_file("tray.png", plain).expect("entry");
        std::io::Write::write_all(&mut writer, &tiny_png()).expect("writes");
        writer.start_file("02.webp", plain).expect("entry");
        std::io::Write::write_all(&mut writer, &webp).expect("writes");
        writer.start_file("01.png", plain).expect("entry");
        std::io::Write::write_all(&mut writer, &tiny_png()).expect("writes");
        writer.finish().expect("finishes");
        let packs = root.join("packs");
        let title = import_archive(&archive_path, &packs).expect("imports");
        assert_eq!(title, "Happy Frogs");
        let mut files: Vec<String> = std::fs::read_dir(packs.join("Happy Frogs"))
            .expect("lists")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        files.sort();
        // Both entries are the same picture, so they collapse into one sticker
        // filed under the hash of its content, beside the pack manifest.
        assert_eq!(files.len(), 2, "tray and title stay out");
        assert_eq!(files[1], crate::stickers::PACK_MANIFEST);
        let hash = crate::stickers::hash_of(&webp);
        assert_eq!(files[0], format!("{hash}.webp"));
        let manifest: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                packs
                    .join("Happy Frogs")
                    .join(crate::stickers::PACK_MANIFEST),
            )
            .expect("reads"),
        )
        .expect("parses");
        assert_eq!(manifest["title"], "Happy Frogs");
        assert_eq!(manifest["order"], serde_json::json!([hash]));
        // Reimporting creates a separate numbered directory.
        let again = import_archive(&archive_path, &packs).expect("imports");
        assert_eq!(again, "Happy Frogs 2");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_apng_keeps_its_motion_as_animated_webp() {
        let mut apng = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut apng, 4, 4);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_animated(2, 0).expect("animated");
            encoder.set_frame_delay(1, 10).expect("delay");
            let mut writer = encoder.write_header().expect("header");
            writer.write_image_data(&[10u8; 4 * 4 * 4]).expect("frame");
            writer.write_image_data(&[200u8; 4 * 4 * 4]).expect("frame");
            writer.finish().expect("finishes");
        }
        let webp = webp_bytes(apng).expect("converts");
        let decoder =
            image::codecs::webp::WebPDecoder::new(std::io::Cursor::new(&webp)).expect("is a webp");
        assert!(decoder.has_animation(), "the motion survives");
    }

    #[test]
    fn an_animated_gif_keeps_its_motion_too() {
        let mut gif = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut gif);
            let frames = [10u8, 200].map(|shade| {
                image::Frame::from_parts(
                    image::RgbaImage::from_pixel(4, 4, image::Rgba([shade, 0, 0, 255])),
                    0,
                    0,
                    image::Delay::from_numer_denom_ms(100, 1),
                )
            });
            encoder.encode_frames(frames).expect("encodes");
        }
        let webp = webp_bytes(gif).expect("converts");
        let decoder =
            image::codecs::webp::WebPDecoder::new(std::io::Cursor::new(&webp)).expect("is a webp");
        assert!(decoder.has_animation(), "the motion survives");
    }

    /// Verifies converted frames apply animation disposal correctly.
    #[test]
    fn a_moving_square_converts_without_a_trace() {
        let side = 64u32;
        let mut apng = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut apng, side, side);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_animated(2, 0).expect("animated");
            encoder.set_frame_delay(1, 10).expect("delay");
            let mut writer = encoder.write_header().expect("header");
            let mut frame1 = vec![0u8; (side * side * 4) as usize];
            let mut frame2 = frame1.clone();
            for y in 0..16u32 {
                for x in 0..16u32 {
                    let a = ((y * side + x) * 4) as usize;
                    frame1[a..a + 4].copy_from_slice(&[255, 0, 0, 255]);
                    let b = (((y + 40) * side + x + 40) * 4) as usize;
                    frame2[b..b + 4].copy_from_slice(&[0, 255, 0, 255]);
                }
            }
            writer.write_image_data(&frame1).expect("frame");
            writer.write_image_data(&frame2).expect("frame");
            writer.finish().expect("finishes");
        }
        let webp = webp_bytes(apng).expect("converts");
        let frames: Vec<_> = webp_animation::Decoder::new(&webp)
            .expect("decodes")
            .into_iter()
            .collect();
        assert_eq!(frames.len(), 2);
        let at = |frame: &webp_animation::Frame, x: u32, y: u32| {
            let p = ((y * side + x) * 4) as usize;
            frame.data()[p + 3]
        };
        assert_eq!(at(&frames[1], 8, 8), 0, "the old square is gone");
        assert!(at(&frames[1], 48, 48) > 200, "the new square shows");
        assert_eq!(at(&frames[0], 48, 48), 0, "frame one starts clean");
    }

    #[test]
    fn a_whatsapp_pack_unpacks_in_order_with_its_emojis() {
        let root = std::env::temp_dir().join(format!("zapfast-shared-pack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("creates");
        let webp = webp_bytes(tiny_png()).expect("encodes");
        let zip_path = root.join("pack.zip");
        let file = std::fs::File::create(&zip_path).expect("creates");
        let mut writer = zip::ZipWriter::new(file);
        let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
        for name in [
            "stickers/b.webp",
            "../evil.webp",
            "stickers/a.webp",
            "tray.webp",
        ] {
            writer.start_file(name, options).expect("entry");
            std::io::Write::write_all(&mut writer, &webp).expect("writes");
        }
        writer.finish().expect("finishes");
        let zip = std::fs::read(&zip_path).expect("reads");
        let out = root.join("out");
        let listed = vec![
            ("stickers/a.webp".to_owned(), vec!["\u{1F602}".to_owned()]),
            ("stickers/b.webp".to_owned(), Vec::new()),
        ];
        let written = extract_whatsapp_pack(&zip, &listed, Some("tray.webp"), "Ducks", &out)
            .expect("unpacks");
        assert_eq!(written.len(), 3, "tray excluded, hostile name kept inside");
        assert!(!root.join("evil.webp").exists(), "no archive entry escapes");
        assert_eq!(
            crate::sticker_meta::emojis(&std::fs::read(&written[0]).expect("reads")),
            vec!["\u{1F602}"]
        );
        assert!(
            crate::sticker_meta::emojis(&std::fs::read(&written[1]).expect("reads")).is_empty()
        );
        let _ = std::fs::remove_dir_all(&root);
    }
    fn tiny_png() -> Vec<u8> {
        use image::ImageEncoder;
        let mut bytes = Vec::new();
        let picture = image::RgbaImage::from_pixel(4, 4, image::Rgba([0, 255, 0, 255]));
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(&picture, 4, 4, image::ExtendedColorType::Rgba8)
            .expect("encodes");
        bytes
    }
}
