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

/// The most pixels a rendered page has: 50 million, 200 MB as RGBA. A page that would be larger at
/// the resolution asked for is rendered at a lower one instead.
pub const MAX_RENDER_PIXELS: f32 = 50_000_000.0;

/// The longest side of a rendered page, in pixels.
pub const MAX_RENDER_SIDE: f32 = 20_000.0;

/// The scale, in pixels per point, a page displayed `width` by `height` points is rendered at for
/// `dpi`: `dpi / 72`, or less when that would go past [`MAX_RENDER_PIXELS`] or [`MAX_RENDER_SIDE`].
pub fn render_scale(width: f32, height: f32, dpi: f32) -> f32 {
    let scale = dpi / 72.0;
    let (w, h) = (width.max(1.0) * scale, height.max(1.0) * scale);
    // A little under the limits, so rounding to whole pixels cannot go past them.
    let fit = (MAX_RENDER_PIXELS / (w * h)).sqrt().min(MAX_RENDER_SIDE / w.max(h)) * 0.999;
    if fit < 1.0 {
        scale * fit
    } else {
        scale
    }
}

/// Folders searched for PDFium after the executable's own, by absolute path.
#[cfg(target_os = "macos")]
const SYSTEM_LIBRARY_DIRS: &[&str] = &["/opt/homebrew/lib", "/usr/local/lib", "/usr/lib"];
#[cfg(all(unix, not(target_os = "macos")))]
const SYSTEM_LIBRARY_DIRS: &[&str] =
    &["/usr/local/lib", "/usr/lib", "/usr/lib64", "/usr/lib/x86_64-linux-gnu", "/usr/lib/aarch64-linux-gnu"];
#[cfg(not(unix))]
const SYSTEM_LIBRARY_DIRS: &[&str] = &[r"C:\Windows\System32"];

/// The files PDFium is looked for at, in order: `configured` (`$CARBIDE_PDFIUM_LIB`, a library
/// file or the folder holding it), the folder of `executable` (links followed, then as started),
/// the `Frameworks` and `Resources` folders of the app bundle it runs from, and the system library
/// folders. Every place but the configured one is an absolute path, and none is the current
/// directory: loading a library runs its code, and a file named like PDFium in whatever folder the
/// program was started from (an unpacked download, say) could be anyone's.
pub fn library_candidates(configured: Option<&Path>, executable: Option<&Path>) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Some(path) = configured {
        files.push(if path.is_dir() { Pdfium::pdfium_platform_library_name_at_path(path) } else { path.to_path_buf() });
    }
    let mut folders: Vec<PathBuf> = Vec::new();
    if let Some(executable) = executable {
        let real = executable.canonicalize().ok();
        for dir in real.as_deref().into_iter().chain([executable]).filter_map(Path::parent) {
            folders.push(dir.to_path_buf());
            // Contents/MacOS of an app bundle: libraries ship in Contents/Frameworks or Resources.
            if dir.file_name().is_some_and(|name| name == "MacOS") {
                if let Some(contents) = dir.parent().filter(|c| c.file_name().is_some_and(|name| name == "Contents")) {
                    folders.push(contents.join("Frameworks"));
                    folders.push(contents.join("Resources"));
                }
            }
        }
    }
    folders.extend(SYSTEM_LIBRARY_DIRS.iter().map(PathBuf::from));
    for folder in folders.into_iter().filter(|f| f.is_absolute()) {
        let file = Pdfium::pdfium_platform_library_name_at_path(&folder);
        if !files.contains(&file) {
            files.push(file);
        }
    }
    files
}

impl Renderer {
    /// Loads PDFium from the first of [`library_candidates`] that holds it, for this process's
    /// `$CARBIDE_PDFIUM_LIB` and executable.
    pub fn new() -> Result<Renderer> {
        let configured = std::env::var_os("CARBIDE_PDFIUM_LIB").map(PathBuf::from);
        let executable = std::env::current_exe().ok();
        let candidates = library_candidates(configured.as_deref(), executable.as_deref());
        let mut failure = None;
        for file in &candidates {
            if file.is_file() {
                match Self::with_library(file) {
                    Ok(renderer) => return Ok(renderer),
                    Err(e) => failure = Some(format!("{}: {e}", file.display())),
                }
            }
        }
        Err(Error::Render(match failure {
            Some(failure) => format!("PDFium could not be loaded ({failure}); set CARBIDE_PDFIUM_LIB to the path of libpdfium"),
            None => "PDFium library not found; set CARBIDE_PDFIUM_LIB to the path of libpdfium".into(),
        }))
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

    /// The size, in points, at which PDFium shows page `index`: its crop box, turned as the page is.
    pub fn page_size(&self, pdf: &[u8], index: usize) -> Result<(f32, f32)> {
        let document = self.pdfium.load_pdf_from_byte_slice(pdf, None).map_err(render_error)?;
        let page = document
            .pages()
            .get(index as PdfPageIndex)
            .map_err(|_| Error::PageOutOfRange { index, count: document.pages().len() as usize })?;
        Ok((page.width().value, page.height().value))
    }

    /// Renders one page as it is displayed (crop box and rotation applied) at `dpi`, or at a lower
    /// resolution when the image would be larger than [`MAX_RENDER_PIXELS`] or
    /// [`MAX_RENDER_SIDE`]. The size of the image says which; callers that map between the page
    /// and the image take the scale from it. PDFium does not limit what drawing a page takes, so
    /// a page of a document that is not trusted is checked first ([`crate::Pdf::check_drawable`]).
    pub fn render_page(&self, pdf: &[u8], index: usize, dpi: f32) -> Result<RgbaImage> {
        if !(dpi.is_finite() && dpi > 0.0) {
            return Err(Error::Render(format!("cannot render at {dpi} dpi")));
        }
        self.render_scaled(pdf, index, |width, height| render_scale(width, height, dpi))
    }

    /// Renders one page as it is displayed, as large as fits in `width` by `height` pixels with its
    /// shape kept (and within the limits of [`Renderer::render_page`]). The fit is worked out from
    /// the page size PDFium itself reads, so the image never comes out larger.
    pub fn render_page_fitting(&self, pdf: &[u8], index: usize, width: u32, height: u32) -> Result<RgbaImage> {
        if width == 0 || height == 0 {
            return Err(Error::Render(format!("cannot render a page into {width} x {height} pixels")));
        }
        self.render_scaled(pdf, index, |page_width, page_height| {
            // A little under, so rounding to whole pixels cannot go past the box.
            let fit = (width as f32 / page_width.max(1.0)).min(height as f32 / page_height.max(1.0)) * 0.999;
            fit.min(render_scale(page_width, page_height, 72.0 * fit))
        })
    }

    /// Renders page `index` at the scale (pixels per point) `scale` gives for its displayed size.
    fn render_scaled(&self, pdf: &[u8], index: usize, scale: impl FnOnce(f32, f32) -> f32) -> Result<RgbaImage> {
        let document = self.pdfium.load_pdf_from_byte_slice(pdf, None).map_err(render_error)?;
        let page = document
            .pages()
            .get(index as PdfPageIndex)
            .map_err(|_| Error::PageOutOfRange { index, count: document.pages().len() as usize })?;
        let scale = scale(page.width().value, page.height().value);
        // PDFium writes RGBA directly, so the image is its buffer copied once, not converted.
        let config = PdfRenderConfig::new()
            .scale_page_by_factor(scale)
            .set_reverse_byte_order(true)
            .render_form_data(true)
            .render_annotations(true);
        let bitmap = page.render_with_config(&config).map_err(render_error)?;
        let (width, height) = (bitmap.width() as u32, bitmap.height() as u32);
        RgbaImage::from_raw(width, height, bitmap.as_rgba_bytes())
            .ok_or_else(|| Error::Render(format!("PDFium returned an unexpected bitmap for a {width} x {height} image")))
    }

    /// Plain text of one page, in reading order as PDFium reports it.
    pub fn page_text(&self, pdf: &[u8], index: usize) -> Result<String> {
        self.text_of_page(pdf, None, index)
    }

    /// Plain text of one page of a file that needs a password to be read.
    pub fn page_text_with_password(&self, pdf: &[u8], password: &str, index: usize) -> Result<String> {
        self.text_of_page(pdf, Some(password), index)
    }

    fn text_of_page(&self, pdf: &[u8], password: Option<&str>, index: usize) -> Result<String> {
        let document = self.pdfium.load_pdf_from_byte_slice(pdf, password).map_err(render_error)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_current_directory_is_never_searched_for_pdfium() {
        let name = Pdfium::pdfium_platform_library_name();
        let bundle = Path::new("/Applications/Carbide PDF.app/Contents/MacOS/carbide-gui");
        let candidates = library_candidates(None, Some(bundle));
        assert!(candidates.iter().all(|c| c.is_absolute()), "{candidates:?}");
        assert!(!candidates.contains(&PathBuf::from(&name)) && !candidates.contains(&Path::new(".").join(&name)));
        let contents = Path::new("/Applications/Carbide PDF.app/Contents");
        assert_eq!(
            candidates[..3],
            [contents.join("MacOS").join(&name), contents.join("Frameworks").join(&name), contents.join("Resources").join(&name)]
        );
        // A plain executable has no bundle folders; the system folders come after its own.
        let plain = library_candidates(None, Some(Path::new("/opt/carbide/bin/carbide")));
        assert_eq!(plain[0], Path::new("/opt/carbide/bin").join(&name));
        assert_eq!(plain.len(), 1 + SYSTEM_LIBRARY_DIRS.len());
        assert!(library_candidates(None, None).iter().all(|c| c.is_absolute()));
    }

    #[test]
    fn rendering_scales_down_pages_that_would_be_too_large() {
        // A Letter page at 300 dpi is rendered as asked.
        assert_eq!(render_scale(612.0, 792.0, 300.0), 300.0 / 72.0);
        // The largest PDF page at 300 dpi would be 60000 pixels square.
        let scale = render_scale(14400.0, 14400.0, 300.0);
        let side = (14400.0 * scale).round();
        assert!(side * side <= MAX_RENDER_PIXELS && side > 7000.0, "{side}");
        // A long strip is limited by its longest side.
        let scale = render_scale(14400.0, 100.0, 300.0);
        assert!((14400.0 * scale).round() <= MAX_RENDER_SIDE);
    }

    #[test]
    fn the_configured_library_comes_first() {
        let file = Path::new("/somewhere/lib/libpdfium-custom.dylib");
        let candidates = library_candidates(Some(file), Some(Path::new("/opt/carbide/bin/carbide")));
        assert_eq!(candidates[0], file);
        // A configured folder names the library inside it.
        let folder = std::env::temp_dir();
        let candidates = library_candidates(Some(&folder), None);
        assert_eq!(candidates[0], Pdfium::pdfium_platform_library_name_at_path(&folder));
    }
}
