//! Renders PDF pages for the in-app viewer.
//!
//! Everything happens on the CPU in Rust, so no native PDF library has to be
//! bundled with a release. The worker renders a page off the interface thread
//! and hands the pixels over as straight RGBA.

use std::path::Path;

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{RenderCache, RenderSettings, render};

/// Narrowest and widest raster the viewer asks for.
pub const MIN_WIDTH: u32 = 320;
pub const MAX_WIDTH: u32 = 2400;

/// One rendered page as straight RGBA.
#[derive(Clone, Debug, PartialEq)]
pub struct Page {
    pub page: usize,
    /// Pages the document holds in total.
    pub pages: usize,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// How wide a page should be rasterised for a view of the given size.
///
/// The requested width follows the zoom so a closer look stays sharp, and
/// the limits keep a huge page from eating memory.
pub fn render_width(view_width: f32, zoom: f32) -> u32 {
    let scaled = view_width * zoom.clamp(1.0, 2.0);
    (scaled.round() as u32).clamp(MIN_WIDTH, MAX_WIDTH)
}

/// Renders one page of a PDF, counting pages from zero.
pub fn render_page(path: &Path, page: usize, width: u32) -> Result<Page, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("Could not read the PDF: {error}"))?;
    let document = Pdf::new(bytes).map_err(|error| format!("Could not open the PDF: {error:?}"))?;
    let pages = document.pages().len();
    let page_ref = document
        .pages()
        .get(page)
        .ok_or_else(|| "That page is not in the document".to_owned())?;
    let (natural_width, _) = page_ref.render_dimensions();
    let scale = (width as f32 / natural_width).max(0.05);
    let settings = RenderSettings {
        x_scale: scale,
        y_scale: scale,
        width: None,
        height: None,
        bg_color: WHITE,
    };
    let pixmap = render(
        page_ref,
        &RenderCache::new(),
        &InterpreterSettings::default(),
        &settings,
    );
    Ok(Page {
        page,
        pages,
        width: u32::from(pixmap.width()),
        height: u32::from(pixmap.height()),
        // The page is drawn on an opaque white background, so premultiplied
        // and straight RGBA agree.
        rgba: pixmap.data_as_u8_slice().to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_asked_width_follows_the_zoom_within_limits() {
        assert_eq!(render_width(900.0, 1.0), 900);
        assert_eq!(render_width(900.0, 1.5), 1350);
        assert_eq!(render_width(900.0, 8.0), 1800);
        assert_eq!(render_width(10.0, 1.0), MIN_WIDTH);
        assert_eq!(render_width(9000.0, 2.0), MAX_WIDTH);
    }
    /// A one-page PDF written by hand, with a single line of text.
    const TINY: &[u8] = b"%PDF-1.4\n\
1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n\
3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 100]/Resources<</Font<</F1 5 0 R>>>>/Contents 4 0 R>>endobj\n\
4 0 obj<</Length 36>>stream\n\
BT /F1 24 Tf 20 40 Td (Hi) Tj ET\n\
endstream\nendobj\n\
5 0 obj<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>endobj\n\
trailer<</Root 1 0 R>>\n\
%%EOF\n";

    #[test]
    fn a_page_renders_to_pixels_and_a_missing_one_reports() {
        let dir = std::env::temp_dir().join(format!("zapfast-pdf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates");
        let path = dir.join("tiny.pdf");
        std::fs::write(&path, TINY).expect("writes");
        let page = render_page(&path, 0, 400).expect("renders");
        assert_eq!(page.pages, 1);
        assert_eq!(
            page.rgba.len(),
            page.width as usize * page.height as usize * 4,
            "a pixel per byte group"
        );
        assert_eq!(page.width, 400, "the asked width is honoured");
        assert!(
            page.rgba.iter().any(|byte| *byte < 200),
            "the page is not blank"
        );
        assert!(
            render_page(&path, 3, 200).is_err(),
            "a missing page reports"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
