//! Renders PDF pages for the in-app viewer.
//!
//! Everything happens on the CPU in Rust, so no native PDF library has to be
//! bundled with a release. The worker renders a page off the interface thread
//! and hands the pixels over as straight RGBA.

use std::path::{Path, PathBuf};

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{RenderCache, RenderSettings, render};

/// Narrowest and widest raster the viewer asks for.
pub const MIN_WIDTH: u32 = 320;
pub const MAX_WIDTH: u32 = 2400;
/// How wide a page thumbnail is, and how many pages get one.
pub const THUMB_WIDTH: u32 = 96;
pub const THUMB_PAGES: usize = 300;
/// Names the previews of one file: file name plus size on disk.
/// Cache names already carry the chat and message, so this survives restarts
/// without pointing at anything personal.
pub fn thumb_key(path: &Path) -> String {
    let name: String = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let size = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    format!("{name}-{size}")
}
/// Largest page raster kept, however tall the page is.
const MAX_PIXELS: f64 = 12.0 * 1024.0 * 1024.0;
/// Pages kept beside the open document, and what they may take together.
const KEEP_PAGES: usize = 3;
const KEEP_BYTES: u64 = 64 * 1024 * 1024;
/// A single page larger than this is rendered but not kept.
const KEEP_PAGE_BYTES: u64 = 40 * 1024 * 1024;

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
impl Page {
    /// Returns the page turned clockwise a quarter at a time, in memory.
    ///
    /// Rotating the rendered pixels is instant beside re-rendering the
    /// document, so the viewer turns the page without asking the worker.
    pub fn rotated(&self, turns: u8) -> Page {
        let mut page = self.clone();
        for _ in 0..turns % 4 {
            let (width, height) = (page.width as usize, page.height as usize);
            let mut out = vec![0u8; page.rgba.len()];
            for y in 0..height {
                for x in 0..width {
                    let from = (y * width + x) * 4;
                    let to = (x * height + (height - 1 - y)) * 4;
                    out[to..to + 4].copy_from_slice(&page.rgba[from..from + 4]);
                }
            }
            page.rgba = out;
            (page.width, page.height) = (page.height, page.width);
        }
        page
    }
}

/// How wide a page should be rasterised for a view of the given size.
///
/// The requested width follows the zoom so a closer look stays sharp, and
/// the limits keep a huge page from eating memory.
pub fn render_width(view_width: f32, zoom: f32) -> u32 {
    let scaled = view_width * zoom.clamp(1.0, 2.0);
    (scaled.round() as u32).clamp(MIN_WIDTH, MAX_WIDTH)
}

/// A PDF kept open between renders.
///
/// Reading and parsing the file is the part that does not change when the
/// reader turns a page or zooms, and neither does a page that was just drawn.
/// The bytes stay in memory while it is the same file, and the pages that
/// were rendered stay beside them, so going back and forth is instant and a
/// sharper look only pays for the pages it really renders again.
#[derive(Default)]
pub struct Reader {
    file: Option<Open>,
    pages: Vec<Kept>,
}

/// The document on screen, with what it was read from.
struct Open {
    path: PathBuf,
    len: u64,
    modified: Option<std::time::SystemTime>,
    document: Pdf,
}

/// One rendered page, with the file and page it belongs to.
struct Kept {
    path: PathBuf,
    page: Page,
}

impl Reader {
    /// Renders one page, reading and parsing the file only when it changed.
    pub fn render(&mut self, path: &Path, page: usize, width: u32) -> Result<Page, String> {
        if let Some(kept) = self.kept(path, page, width) {
            return Ok(kept);
        }
        let rendered = render_page_of(self.document(path)?, page, width)?;
        self.keep(path, &rendered);
        Ok(rendered)
    }

    /// Renders one page ahead, so turning to it needs no wait.
    ///
    /// Nothing here is reported: a page that fails is asked for again when
    /// the reader actually shows it, and then the error is what they see.
    pub fn prefetch(&mut self, path: &Path, page: usize, width: u32) {
        if self.kept(path, page, width).is_none() {
            let _ = self.render(path, page, width);
        }
    }

    /// Forgets the open document and everything rendered from it.
    pub fn clear(&mut self) {
        self.file = None;
        self.pages.clear();
    }
    /// How many pages the file holds, parsing it when it changed.
    pub fn pages(&mut self, path: &Path) -> Result<usize, String> {
        Ok(self.document(path)?.pages().len())
    }
    /// Renders a small preview of one page without touching the kept pages.
    ///
    /// Thumbnails share the parsed document but never evict the pages the
    /// reader is looking at.
    pub fn thumb(&mut self, path: &Path, page: usize, width: u32) -> Result<Page, String> {
        let rendered = render_page_of(self.document(path)?, page, width)?;
        Ok(rendered)
    }

    /// A page already in hand, as long as it is not coarser than asked for.
    fn kept(&self, path: &Path, page: usize, width: u32) -> Option<Page> {
        self.pages
            .iter()
            .find(|kept| kept.path == path && kept.page.page == page && kept.page.width >= width)
            .map(|kept| kept.page.clone())
    }

    /// The open document, parsed again only when another file is asked for.
    fn document(&mut self, path: &Path) -> Result<&Pdf, String> {
        let meta =
            std::fs::metadata(path).map_err(|error| format!("Could not read the PDF: {error}"))?;
        let len = meta.len();
        let modified = meta.modified().ok();
        let changed = self
            .file
            .as_ref()
            .is_none_or(|open| open.path != path || open.len != len || open.modified != modified);
        if changed {
            let bytes =
                std::fs::read(path).map_err(|error| format!("Could not read the PDF: {error}"))?;
            let document = Pdf::new(bytes).map_err(open_error)?;
            self.file = Some(Open {
                path: path.to_path_buf(),
                len,
                modified,
                document,
            });
            // Pages of the file that was open say nothing about this one.
            self.pages.clear();
        }
        Ok(&self.file.as_ref().expect("just opened").document)
    }

    /// Keeps a rendered page, dropping the oldest ones past the bounds.
    fn keep(&mut self, path: &Path, page: &Page) {
        let size = page.rgba.len() as u64;
        if size > KEEP_PAGE_BYTES {
            return;
        }
        // A sharper render of a page replaces the one already there.
        self.pages
            .retain(|kept| !(kept.path == path && kept.page.page == page.page));
        self.pages.push(Kept {
            path: path.to_path_buf(),
            page: page.clone(),
        });
        while self.pages.len() > KEEP_PAGES || self.held() > KEEP_BYTES {
            self.pages.remove(0);
        }
    }

    /// What the kept pages take together.
    fn held(&self) -> u64 {
        self.pages
            .iter()
            .map(|kept| kept.page.rgba.len() as u64)
            .sum()
    }
}

/// Renders one page of a PDF, counting pages from zero.
pub fn render_page(path: &Path, page: usize, width: u32) -> Result<Page, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("Could not read the PDF: {error}"))?;
    let document = Pdf::new(bytes).map_err(open_error)?;
    render_page_of(&document, page, width)
}

/// Rasterises one page of an already parsed document.
fn render_page_of(document: &Pdf, page: usize, width: u32) -> Result<Page, String> {
    let pages = document.pages().len();
    let page_ref = document
        .pages()
        .get(page)
        .ok_or_else(|| "That page is not in the document".to_owned())?;
    let (natural_width, natural_height) = page_ref.render_dimensions();
    let mut scale = (width as f32 / natural_width).max(0.05);
    // A very tall page is scaled back, so one raster cannot eat the memory
    // of the whole viewer.
    let (wide, tall) = (f64::from(natural_width), f64::from(natural_height));
    let pixels = wide * tall * f64::from(scale) * f64::from(scale);
    if pixels > MAX_PIXELS {
        scale *= (MAX_PIXELS / pixels).sqrt() as f32;
    }
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

/// Why a document could not be opened, in words a reader can act on.
fn open_error(error: impl std::fmt::Debug) -> String {
    let detail = format!("{error:?}");
    let lower = detail.to_ascii_lowercase();
    if lower.contains("decrypt") || lower.contains("encrypt") || lower.contains("password") {
        return "This PDF is protected, so it cannot be shown here. Use Open in the default app instead.".to_owned();
    }
    if lower.contains("invalid") || lower.contains("eof") || lower.contains("header") {
        return "This file is not a PDF, or it is damaged.".to_owned();
    }
    format!("Could not open the PDF: {detail}")
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

    /// A one-page PDF whose page is very tall.
    const TALL: &[u8] = b"%PDF-1.4\n\
1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n\
3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 40 16000]/Resources<</Font<</F1 5 0 R>>>>/Contents 4 0 R>>endobj\n\
4 0 obj<</Length 36>>stream\n\
BT /F1 24 Tf 20 40 Td (Hi) Tj ET\n\
endstream\nendobj\n\
5 0 obj<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>endobj\n\
trailer<</Root 1 0 R>>\n\
%%EOF\n";

    #[test]
    fn an_open_document_serves_its_pages_again() {
        let dir = std::env::temp_dir().join(format!("zapfast-pdf-reader-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates");
        let path = dir.join("tiny.pdf");
        std::fs::write(&path, TINY).expect("writes");
        let mut reader = Reader::default();
        let first = reader.render(&path, 0, 400).expect("renders");
        assert_eq!(first.width, 400);
        // The same page at the same width comes back from memory.
        assert_eq!(reader.render(&path, 0, 400).expect("again"), first);
        // Asking for a sharper one renders it, and that is what is kept.
        let sharp = reader.render(&path, 0, 800).expect("renders sharper");
        assert_eq!(sharp.width, 800);
        assert_eq!(reader.render(&path, 0, 400).expect("served"), sharp);
        // A page that is not in the document still reports.
        assert!(reader.render(&path, 4, 400).is_err());
        // Another file replaces the one that was open.
        let other = dir.join("other.pdf");
        std::fs::write(&other, TINY).expect("writes");
        assert_eq!(reader.render(&other, 0, 400).expect("renders").pages, 1);
        assert_eq!(reader.render(&other, 0, 800).expect("sharper").width, 800);
        reader.clear();
        assert_eq!(
            reader.render(&path, 0, 400).expect("renders again").width,
            400
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_page_taller_than_the_budget_is_scaled_back() {
        let dir = std::env::temp_dir().join(format!("zapfast-pdf-tall-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates");
        let path = dir.join("tall.pdf");
        std::fs::write(&path, TALL).expect("writes");
        let page = render_page(&path, 0, 400).expect("renders");
        let pixels = u64::from(page.width) * u64::from(page.height);
        assert!(
            pixels <= 13_000_000,
            "a page this tall is scaled back: {pixels} pixels"
        );
        assert!(page.height > page.width, "and keeps its shape");
        let _ = std::fs::remove_dir_all(dir);
    }
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
    #[test]
    fn a_page_turns_without_rerendering_and_names_its_previews() {
        let dir = std::env::temp_dir().join(format!("zapfast-pdf-turn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates");
        let path = dir.join("tall portrait.pdf");
        std::fs::write(&path, TINY).expect("writes");
        let page = render_page(&path, 0, 400).expect("renders");
        assert!(page.width > page.height, "the test page is landscape");
        let turned = page.rotated(1);
        assert_eq!((turned.width, turned.height), (page.height, page.width));
        assert_eq!(
            turned.rgba.len(),
            turned.width as usize * turned.height as usize * 4
        );
        assert_eq!(page.rotated(4), page, "four turns come home");
        assert_eq!(page.rotated(0).rgba, page.rgba);
        let key = thumb_key(&path);
        assert!(
            key.starts_with("tall_portrait_pdf"),
            "names stay readable: {key}"
        );
        assert_eq!(key, thumb_key(&path), "the same file keeps its name");
        let mut reader = Reader::default();
        assert_eq!(reader.pages(&path).expect("counts"), 1);
        let thumb = reader.thumb(&path, 0, THUMB_WIDTH).expect("previews");
        assert!(thumb.width <= THUMB_WIDTH, "small enough for the strip");
        let _ = std::fs::remove_dir_all(dir);
    }
}
