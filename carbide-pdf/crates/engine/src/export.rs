use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::pdf::Pdf;
use crate::render::Renderer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    Jpeg,
}

impl ImageFormat {
    pub fn extension(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpg",
        }
    }
}

/// Writes one image per page as `<base>-001.png`, `<base>-002.png`, ... and returns the paths.
pub fn export_images(
    renderer: &Renderer,
    pdf: &mut Pdf,
    directory: impl AsRef<Path>,
    base_name: &str,
    format: ImageFormat,
    dpi: f32,
) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(directory.as_ref())?;
    let bytes = pdf.to_bytes()?;
    let mut paths = Vec::new();
    for index in 0..pdf.page_count() {
        let image = renderer.render_page(&bytes, index, dpi)?;
        let path = directory
            .as_ref()
            .join(format!("{base_name}-{:03}.{}", index + 1, format.extension()));
        match format {
            ImageFormat::Png => image.save(&path),
            ImageFormat::Jpeg => image::DynamicImage::ImageRgba8(image).to_rgb8().save(&path),
        }
        .map_err(|e| Error::Other(format!("could not write {}: {e}", path.display())))?;
        paths.push(path);
    }
    Ok(paths)
}

/// Text of every page, separated by blank lines.
pub fn export_text(renderer: &Renderer, pdf: &mut Pdf) -> Result<String> {
    let bytes = pdf.to_bytes()?;
    let mut pages = Vec::new();
    for index in 0..pdf.page_count() {
        pages.push(renderer.page_text(&bytes, index)?);
    }
    Ok(pages.join("\n\n"))
}
