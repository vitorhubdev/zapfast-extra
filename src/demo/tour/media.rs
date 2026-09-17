//! Stickers for the offline tour, drawn from the bundled emoji font.

use anyhow::{Context, Result};
use image::RgbaImage;
use skrifa::{FontRef, MetadataProvider, bitmap::BitmapData, instance::Size};

use crate::{app::App, model::StickerPack};

pub fn populate(app: &mut App) -> Result<()> {
    let dir = app.dirs.media_cache_dir().join("tour");
    std::fs::create_dir_all(&dir)?;
    let font = FontRef::new(include_bytes!("../../../assets/fonts/NotoColorEmoji.ttf"))?;
    let mut stickers = Vec::new();
    for (index, character) in ['🥳', '🎉', '🚀', '👋', '😎', '🐸'].into_iter().enumerate()
    {
        let sticker = dir.join(format!("still-sticker-{index}.webp"));
        if !sticker.exists() {
            let glyph = font
                .charmap()
                .map(character as u32)
                .context("sample emoji glyph")?;
            let bitmap = font
                .bitmap_strikes()
                .glyph_for_size(Size::new(128.0), glyph)
                .context("sample emoji bitmap")?;
            let BitmapData::Png(png) = bitmap.data else {
                anyhow::bail!("expected PNG emoji");
            };
            let emoji = image::load_from_memory(png)?.to_rgba8();
            let resized =
                image::imageops::resize(&emoji, 136, 136, image::imageops::FilterType::Lanczos3);
            let mut tile = RgbaImage::new(192, 192);
            image::imageops::overlay(&mut tile, &resized, 28, 28);
            tile.save(&sticker)?;
        }
        stickers.push(sticker);
    }
    app.stickers_saved = stickers[..3].to_vec();
    app.stickers = stickers.clone();
    app.sticker_packs = vec![StickerPack {
        name: "Launch party".into(),
        dir,
        stickers,
    }];
    app.stickers_pending = false;
    Ok(())
}
