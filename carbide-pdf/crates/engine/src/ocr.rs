//! OCR: makes scanned pages searchable by adding an invisible text layer.
//!
//! The scan itself is left exactly as it was. Recognition uses the `ocrs` engine (pure Rust) and
//! its detection/recognition models, which are located through `CARBIDE_OCR_MODELS`, a `models`
//! folder next to the executable, or the directory passed in [`OcrOptions`].
//!
//! Limitation: the text layer is written with a standard Latin font, so characters outside
//! WinAnsi (for example Cyrillic or CJK) cannot be represented and are written as `?`.

use std::path::{Path, PathBuf};

use ocrs::{ImageSource, OcrEngine, OcrEngineParams, TextItem};

use crate::error::{Error, Result};
use crate::fonts::{literal_string, win_ansi, StandardFont};
use crate::geometry::{fmt, matrix_string};
use crate::pdf::Pdf;
use crate::render::Renderer;

const DETECTION_MODEL: &str = "text-detection.rten";
const RECOGNITION_MODEL: &str = "text-recognition.rten";

#[derive(Debug, Clone)]
pub struct OcrOptions {
    /// Resolution pages are rendered at before recognition.
    pub dpi: f32,
    /// Folder holding the two `.rten` model files; `None` searches the usual places.
    pub models_dir: Option<PathBuf>,
    /// Pages with more characters of text than this are left alone.
    pub skip_pages_with_text_over: usize,
    /// Zero-based pages to consider; `None` means all.
    pub pages: Option<Vec<usize>>,
}

impl Default for OcrOptions {
    fn default() -> Self {
        Self { dpi: 300.0, models_dir: None, skip_pages_with_text_over: 10, pages: None }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OcrReport {
    pub recognized_pages: Vec<usize>,
    pub skipped_pages: Vec<usize>,
    pub lines: usize,
}

fn locate_models(hint: Option<&Path>) -> Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    candidates.extend(hint.map(Path::to_path_buf));
    if let Some(dir) = std::env::var_os("CARBIDE_OCR_MODELS") {
        candidates.push(PathBuf::from(dir));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("models"));
            candidates.push(dir.to_path_buf());
        }
    }
    candidates.push(PathBuf::from("models"));
    candidates
        .into_iter()
        .find(|d| d.join(DETECTION_MODEL).exists() && d.join(RECOGNITION_MODEL).exists())
        .ok_or_else(|| {
            Error::Other(format!(
                "OCR models not found: put {DETECTION_MODEL} and {RECOGNITION_MODEL} in a folder and set CARBIDE_OCR_MODELS"
            ))
        })
}

fn engine(models: &Path) -> Result<OcrEngine> {
    let load = |name: &str| {
        rten::Model::load_file(models.join(name))
            .map_err(|e| Error::Other(format!("could not load OCR model {name}: {e}")))
    };
    OcrEngine::new(OcrEngineParams {
        detection_model: Some(load(DETECTION_MODEL)?),
        recognition_model: Some(load(RECOGNITION_MODEL)?),
        ..Default::default()
    })
    .map_err(|e| Error::Other(format!("could not start the OCR engine: {e}")))
}

/// One recognized line in display space (points, origin bottom-left).
struct Line {
    text: String,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

fn recognize(engine: &OcrEngine, image: &image::RgbaImage, dpi: f32, page_height_pt: f32) -> Result<Vec<Line>> {
    let rgb = image::DynamicImage::ImageRgba8(image.clone()).into_rgb8();
    let source = ImageSource::from_bytes(rgb.as_raw(), rgb.dimensions())
        .map_err(|e| Error::Other(format!("OCR input error: {e}")))?;
    let input = engine.prepare_input(source).map_err(|e| Error::Other(format!("OCR error: {e}")))?;
    let words = engine.detect_words(&input).map_err(|e| Error::Other(format!("OCR error: {e}")))?;
    let line_rects = engine.find_text_lines(&input, &words);
    let lines = engine.recognize_text(&input, &line_rects).map_err(|e| Error::Other(format!("OCR error: {e}")))?;

    let scale = 72.0 / dpi;
    let mut out = Vec::new();
    for line in lines.into_iter().flatten() {
        let text = line.to_string().trim().to_string();
        if text.is_empty() {
            continue;
        }
        let rect = line.bounding_rect();
        let (left, top) = (rect.left() as f32, rect.top() as f32);
        let (width, height) = (rect.width() as f32, rect.height() as f32);
        out.push(Line {
            text,
            x: left * scale,
            y: page_height_pt - (top + height) * scale,
            width: width * scale,
            height: height * scale,
        });
    }
    Ok(out)
}

impl Pdf {
    /// Adds an invisible, selectable text layer to pages that have no text of their own.
    pub fn make_searchable(&mut self, renderer: &Renderer, options: &OcrOptions) -> Result<OcrReport> {
        let models = locate_models(options.models_dir.as_deref())?;
        let engine = engine(&models)?;
        let snapshot = self.to_bytes()?;
        let ids = self.page_ids();
        let count = ids.len();
        let wanted: Vec<usize> = options.pages.clone().unwrap_or_else(|| (0..count).collect());
        let mut report = OcrReport::default();

        for page in wanted {
            if page >= count {
                return Err(Error::PageOutOfRange { index: page, count });
            }
            let existing = renderer.page_text(&snapshot, page)?;
            if existing.trim().chars().count() > options.skip_pages_with_text_over {
                report.skipped_pages.push(page);
                continue;
            }
            let page_id = ids[page];
            let geometry = self.geometry_of(page_id);
            let (_, height) = geometry.display_size();
            let image = renderer.render_page(&snapshot, page, options.dpi)?;
            let lines = recognize(&engine, &image, options.dpi, height)?;
            report.recognized_pages.push(page);
            if lines.is_empty() {
                continue;
            }

            let font_name = self.add_standard_font(page_id, StandardFont::Helvetica)?;
            let mut content: Vec<u8> = format!("q\n{}\nBT\n3 Tr\n", matrix_string(geometry.display_to_user())).into_bytes();
            for line in &lines {
                let encoded = win_ansi(&line.text);
                // Font size from the line height; horizontal scale stretches the text so that
                // selection boxes match the width of the printed line.
                let size = (line.height * 0.85).max(2.0);
                let natural = StandardFont::Helvetica.text_width(&line.text, size).max(0.01);
                let scale = (line.width / natural * 100.0).clamp(10.0, 400.0);
                content.extend_from_slice(
                    format!(
                        "/{font_name} {} Tf {} Tz 1 0 0 1 {} {} Tm\n",
                        fmt(size),
                        fmt(scale),
                        fmt(line.x),
                        fmt(line.y + line.height * 0.2)
                    )
                    .as_bytes(),
                );
                content.extend_from_slice(&literal_string(&encoded));
                content.extend_from_slice(b" Tj\n");
            }
            content.extend_from_slice(b"ET\nQ\n");
            self.append_content(page_id, content)?;
            report.lines += lines.len();
        }
        Ok(report)
    }
}
