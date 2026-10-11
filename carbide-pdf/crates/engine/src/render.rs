//! PDFium-backed rasterizing and text extraction. PDFium is loaded dynamically, so the engine
//! builds without it and reports a clear error if the library cannot be found.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use image::RgbaImage;
use pdfium_render::prelude::*;

use crate::error::{Error, Result};
use crate::geometry::{PageGeometry, Rect};

pub struct Renderer {
    pdfium: Pdfium,
}

fn render_error(error: PdfiumError) -> Error {
    Error::Render(format!("{error:?}"))
}

impl Renderer {
    /// Looks for PDFium in, in order: `$CARBIDE_PDFIUM_LIB` (file or directory), the current
    /// directory, the executable's directory, and finally the system library path.
    pub fn new() -> Result<Renderer> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(value) = std::env::var_os("CARBIDE_PDFIUM_LIB") {
            candidates.push(PathBuf::from(value));
        }
        candidates.push(PathBuf::from("."));
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                candidates.push(dir.to_path_buf());
            }
        }
        for candidate in candidates {
            let file = if candidate.is_dir() {
                Pdfium::pdfium_platform_library_name_at_path(&candidate)
            } else {
                candidate.clone()
            };
            if file.exists() {
                if let Ok(renderer) = Self::with_library(&file) {
                    return Ok(renderer);
                }
            }
        }
        let bindings = Pdfium::bind_to_system_library().map_err(|_| {
            Error::Render(
                "PDFium library not found; set CARBIDE_PDFIUM_LIB to the path of libpdfium".into(),
            )
        })?;
        Ok(Renderer { pdfium: Pdfium::new(bindings) })
    }

    /// The process-wide renderer, created on first use. PDFium keeps global state, so an
    /// application should use this (or create exactly one `Renderer`) rather than several.
    pub fn global() -> Result<&'static Renderer> {
        static GLOBAL: OnceLock<std::result::Result<Renderer, String>> = OnceLock::new();
        GLOBAL
            .get_or_init(|| Renderer::new().map_err(|e| e.to_string()))
            .as_ref()
            .map_err(|message| Error::Render(message.clone()))
    }

    pub fn with_library(path: impl AsRef<Path>) -> Result<Renderer> {
        let bindings = Pdfium::bind_to_library(path.as_ref()).map_err(render_error)?;
        Ok(Renderer { pdfium: Pdfium::new(bindings) })
    }

    pub fn page_count(&self, pdf: &[u8]) -> Result<usize> {
        let document = self.pdfium.load_pdf_from_byte_slice(pdf, None).map_err(render_error)?;
        Ok(document.pages().len() as usize)
    }

    /// Renders one page as it is displayed (crop box and rotation applied) at `dpi`.
    pub fn render_page(&self, pdf: &[u8], index: usize, dpi: f32) -> Result<RgbaImage> {
        let document = self.pdfium.load_pdf_from_byte_slice(pdf, None).map_err(render_error)?;
        let page = document
            .pages()
            .get(index as PdfPageIndex)
            .map_err(|_| Error::PageOutOfRange { index, count: document.pages().len() as usize })?;
        let config = PdfRenderConfig::new()
            .scale_page_by_factor(dpi / 72.0)
            .render_form_data(true)
            .render_annotations(true);
        let bitmap = page.render_with_config(&config).map_err(render_error)?;
        Ok(bitmap.as_image().map_err(render_error)?.to_rgba8())
    }

    /// Plain text of one page, in reading order as PDFium reports it.
    pub fn page_text(&self, pdf: &[u8], index: usize) -> Result<String> {
        let document = self.pdfium.load_pdf_from_byte_slice(pdf, None).map_err(render_error)?;
        let page = document
            .pages()
            .get(index as PdfPageIndex)
            .map_err(|_| Error::PageOutOfRange { index, count: document.pages().len() as usize })?;
        let text = page.text().map_err(render_error)?;
        // `PdfPageText::all()` clips to the displayed page size in *unrotated* coordinates, which
        // silently drops everything on 90/270 degree pages. Read the characters directly.
        let mut out = String::new();
        for character in text.chars().iter() {
            if let Some(c) = character.unicode_char() {
                out.push(c);
            }
        }
        Ok(out)
    }

    /// Rectangles (display space, one per text line) covering the characters that lie inside
    /// `region`. Used to turn a dragged area into a text-snapped markup.
    pub fn text_line_rects(&self, pdf: &[u8], index: usize, geometry: &PageGeometry, region: Rect) -> Result<Vec<Rect>> {
        let document = self.pdfium.load_pdf_from_byte_slice(pdf, None).map_err(render_error)?;
        let page = document
            .pages()
            .get(index as PdfPageIndex)
            .map_err(|_| Error::PageOutOfRange { index, count: document.pages().len() as usize })?;
        let text = page.text().map_err(render_error)?;
        // `chars_inside_rect` and `all()` clip in unrotated coordinates and drop text on rotated
        // pages, so walk every character and apply the region in display space instead.
        // Lines are grouped in the page's own space, where text runs horizontally, and only the
        // finished line boxes are mapped to display space.
        let mut boxes: Vec<Rect> = Vec::new();
        for character in text.chars().iter() {
            match character.unicode_char() {
                Some(c) if !c.is_whitespace() && !c.is_control() => {}
                _ => continue,
            }
            let Ok(bounds) = character.loose_bounds() else { continue };
            let user = Rect::from_corners(
                (bounds.left().value, bounds.bottom().value),
                (bounds.right().value, bounds.top().value),
            );
            let (cx, cy) = geometry.user_to_display(user.x + user.width / 2.0, user.y + user.height / 2.0);
            if cx >= region.x && cx <= region.max_x() && cy >= region.y && cy <= region.max_y() {
                boxes.push(user);
            }
        }

        // Group into lines: boxes whose vertical centres are within half a line height.
        boxes.sort_by(|a, b| {
            let (ay, by) = (a.y + a.height / 2.0, b.y + b.height / 2.0);
            by.partial_cmp(&ay).unwrap_or(std::cmp::Ordering::Equal).then(a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
        });
        let mut grouped: Vec<Rect> = Vec::new();
        for b in boxes {
            let centre = b.y + b.height / 2.0;
            match grouped.last_mut() {
                Some(line) if (centre - (line.y + line.height / 2.0)).abs() <= line.height.max(b.height) * 0.5 => {
                    let (x0, y0) = (line.x.min(b.x), line.y.min(b.y));
                    let (x1, y1) = (line.max_x().max(b.max_x()), line.max_y().max(b.max_y()));
                    *line = Rect::new(x0, y0, x1 - x0, y1 - y0);
                }
                _ => grouped.push(b),
            }
        }

        let lines = grouped
            .into_iter()
            .map(|line| {
                let corners = [
                    geometry.user_to_display(line.x, line.y),
                    geometry.user_to_display(line.max_x(), line.y),
                    geometry.user_to_display(line.x, line.max_y()),
                    geometry.user_to_display(line.max_x(), line.max_y()),
                ];
                let min_x = corners.iter().map(|c| c.0).fold(f32::INFINITY, f32::min);
                let max_x = corners.iter().map(|c| c.0).fold(f32::NEG_INFINITY, f32::max);
                let min_y = corners.iter().map(|c| c.1).fold(f32::INFINITY, f32::min);
                let max_y = corners.iter().map(|c| c.1).fold(f32::NEG_INFINITY, f32::max);
                Rect::new(min_x, min_y, max_x - min_x, max_y - min_y)
            })
            .collect::<Vec<_>>();
        Ok(lines)
    }
}
