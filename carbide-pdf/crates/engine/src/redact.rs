//! Redaction that removes content instead of covering it.
//!
//! Each affected page is rasterized, the marked areas are painted black in the bitmap, and the page
//! is rebuilt around that single image. Nothing of the original page content, annotations, form
//! field values or thumbnails remains in the saved file.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;

use image::{codecs::jpeg::JpegEncoder, ExtendedColorType, Rgba};
use lopdf::{dictionary, Dictionary, Object, ObjectId, Stream};

use crate::error::{Error, Result};
use crate::geometry::{fmt, Rect};
use crate::pdf::Pdf;
use crate::render::Renderer;
use crate::scrub::widget_area;

#[derive(Debug, Clone, Copy)]
pub struct Redaction {
    /// Zero-based page.
    pub page: usize,
    /// In display space (points, origin bottom-left of the displayed page).
    pub rect: Rect,
}

impl Pdf {
    pub fn apply_redactions(&mut self, renderer: &Renderer, redactions: &[Redaction], dpi: f32) -> Result<()> {
        if redactions.is_empty() {
            return Ok(());
        }
        let count = self.page_count();
        if let Some(bad) = redactions.iter().find(|r| r.page >= count) {
            return Err(Error::PageOutOfRange { index: bad.page, count });
        }
        let snapshot = self.to_bytes()?;

        let mut pages: Vec<usize> = redactions.iter().map(|r| r.page).collect();
        pages.sort_unstable();
        pages.dedup();
        // The rebuilt pages lose their annotations, and with them their form fields: the values
        // and appearances those hold would otherwise stay in the form. A signature field goes
        // only when its signature is drawn inside a redacted area; otherwise it stays (it no
        // longer validates, which saving says).
        let mut redacted: HashMap<ObjectId, Vec<Rect>> = HashMap::new();
        for item in redactions {
            let geometry = self.page_geometry(item.page)?;
            redacted.entry(self.page_id(item.page)?).or_default().push(geometry.map_rect(item.rect));
        }
        let rebuilt: HashSet<ObjectId> = redacted.keys().copied().collect();
        self.drop_form_fields_on(&rebuilt, &|page, widget, signature| {
            !signature
                || widget_area(widget).is_some_and(|[x0, y0, x1, y1]| {
                    redacted.get(&page).is_some_and(|areas| {
                        areas.iter().any(|a| a.x < x1 && x0 < a.max_x() && a.y < y1 && y0 < a.max_y())
                    })
                })
        });

        for page in pages {
            let geometry = self.page_geometry(page)?;
            let (display_w, display_h) = geometry.display_size();
            let mut image = renderer.render_page(&snapshot, page, dpi)?;
            let sx = image.width() as f32 / display_w;
            let sy = image.height() as f32 / display_h;

            for item in redactions.iter().filter(|r| r.page == page) {
                let x0 = (item.rect.x * sx).floor().max(0.0) as u32;
                let x1 = ((item.rect.max_x() * sx).ceil().max(0.0) as u32).min(image.width());
                // Image rows run top to bottom; display space runs bottom to top.
                let y0 = ((display_h - item.rect.max_y()) * sy).floor().max(0.0) as u32;
                let y1 = (((display_h - item.rect.y) * sy).ceil().max(0.0) as u32).min(image.height());
                for y in y0..y1 {
                    for x in x0..x1 {
                        image.put_pixel(x, y, Rgba([0, 0, 0, 255]));
                    }
                }
            }

            let rgb = image::DynamicImage::ImageRgba8(image).to_rgb8();
            let mut jpeg = Cursor::new(Vec::new());
            JpegEncoder::new_with_quality(&mut jpeg, 90)
                .encode(rgb.as_raw(), rgb.width(), rgb.height(), ExtendedColorType::Rgb8)
                .map_err(|e| Error::Other(format!("JPEG encoding failed: {e}")))?;

            let image_id = self.doc.add_object(Stream::new(
                dictionary! {
                    "Type" => "XObject",
                    "Subtype" => "Image",
                    "Width" => rgb.width() as i64,
                    "Height" => rgb.height() as i64,
                    "ColorSpace" => "DeviceRGB",
                    "BitsPerComponent" => 8,
                    "Filter" => "DCTDecode",
                },
                jpeg.into_inner(),
            ).with_compression(false));
            let content = format!("q {} 0 0 {} 0 0 cm /CarbideRedacted Do Q\n", fmt(display_w), fmt(display_h));
            let content_id = self.doc.add_object(Stream::new(Dictionary::new(), content.into_bytes()));

            let page_id = self.page_id(page)?;
            let dict = self.doc.get_dictionary_mut(page_id)?;
            for key in [
                &b"Annots"[..], b"TrimBox", b"BleedBox", b"ArtBox", b"Thumb", b"B", b"AA", b"StructParents", b"Group",
                b"Metadata", b"PieceInfo",
            ] {
                dict.remove(key);
            }
            // The image already shows the page as displayed, so the page gets an explicit box and no
            // rotation: without them a /Rotate or /CropBox of a page-tree node above it would apply
            // a second time.
            let page_box = vec![0.into(), 0.into(), Object::Real(display_w), Object::Real(display_h)];
            dict.set("MediaBox", page_box.clone());
            dict.set("CropBox", page_box);
            dict.set("Rotate", 0);
            dict.set("Contents", content_id);
            dict.set(
                "Resources",
                dictionary! {
                    "XObject" => dictionary! { "CarbideRedacted" => image_id },
                },
            );
        }

        // Tagged-PDF structure can carry copies of the redacted text.
        if let Ok(root) = self.doc.trailer.get(b"Root").and_then(|o| o.as_reference()) {
            if let Ok(catalog) = self.doc.get_dictionary_mut(root) {
                catalog.remove(b"StructTreeRoot");
                catalog.remove(b"MarkInfo");
            }
        }
        // Images and forms only the old page content drew can sit in resources other pages share.
        self.trim_resources()
    }
}
