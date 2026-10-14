use std::io::Cursor;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::fs::Existing;
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

/// Where [`export_images`] writes the image of each page: `<base>-001.png`, `<base>-002.png`, ...
pub fn image_paths(pdf: &Pdf, directory: impl AsRef<Path>, base_name: &str, format: ImageFormat) -> Vec<PathBuf> {
    (0..pdf.page_count())
        .map(|index| directory.as_ref().join(format!("{base_name}-{:03}.{}", index + 1, format.extension())))
        .collect()
}

/// Writes one image per page at the paths [`image_paths`] gives, and returns the paths written.
///
/// `existing` says what happens to a file that is already there. With [`Existing::Refuse`] nothing
/// is written when any of the paths is taken; with [`Existing::KeepBoth`] the image whose name is
/// taken gets a numbered name instead. Each image is written whole or not at all.
pub fn export_images(
    renderer: &Renderer,
    pdf: &mut Pdf,
    directory: impl AsRef<Path>,
    base_name: &str,
    format: ImageFormat,
    dpi: f32,
    existing: Existing,
) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(directory.as_ref())?;
    let targets = image_paths(pdf, directory.as_ref(), base_name, format);
    if existing == Existing::Refuse {
        if let Some(taken) = targets.iter().find(|path| std::fs::symlink_metadata(path).is_ok()) {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("{} already exists", taken.display()),
            )));
        }
    }
    for index in 0..targets.len() {
        pdf.check_drawable(index)?;
    }
    let bytes = pdf.to_bytes()?;
    let mut paths = Vec::new();
    for (index, target) in targets.iter().enumerate() {
        let image = image::DynamicImage::ImageRgba8(renderer.render_page(&bytes, index, dpi)?);
        let mut encoded = Cursor::new(Vec::new());
        match format {
            ImageFormat::Png => image.write_to(&mut encoded, image::ImageFormat::Png),
            ImageFormat::Jpeg => image::DynamicImage::ImageRgb8(image.into_rgb8()).write_to(&mut encoded, image::ImageFormat::Jpeg),
        }
        .map_err(|e| Error::Other(format!("could not encode {}: {e}", target.display())))?;
        let written = crate::fs::write_file(target, encoded.get_ref(), existing)
            .map_err(|e| Error::Other(format!("could not write {}: {e}", target.display())))?;
        paths.push(written);
    }
    Ok(paths)
}

/// Text of every page, separated by blank lines.
pub fn export_text(renderer: &Renderer, pdf: &mut Pdf) -> Result<String> {
    for index in 0..pdf.page_count() {
        pdf.check_drawable(index)?;
    }
    let bytes = pdf.to_bytes()?;
    let mut pages = Vec::new();
    for index in 0..pdf.page_count() {
        pages.push(renderer.page_text(&bytes, index)?);
    }
    Ok(pages.join("\n\n"))
}
