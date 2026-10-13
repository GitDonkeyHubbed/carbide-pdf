//! Standard annotations (highlight, underline, strikeout, sticky note, text box), each with an
//! explicit appearance stream so every viewer draws them the same way.

use lopdf::{dictionary, Dictionary, Object, ObjectId, Stream, StringFormat};

use crate::error::Result;
use crate::fonts::{literal_string, win_ansi, StandardFont};
use crate::geometry::{fmt, PageGeometry, Rect};
use crate::pdf::Pdf;

/// One annotation as listed in a comments panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnotationSummary {
    pub page: usize,
    /// The PDF annotation subtype: `Highlight`, `Text`, `FreeText`, ...
    pub kind: String,
    pub contents: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkupStyle {
    Highlight,
    Underline,
    StrikeOut,
}

impl MarkupStyle {
    pub fn default_color(self) -> [f32; 3] {
        match self {
            MarkupStyle::Highlight => [1.0, 0.92, 0.0],
            MarkupStyle::Underline => [0.0, 0.35, 1.0],
            MarkupStyle::StrikeOut => [0.9, 0.1, 0.1],
        }
    }

    fn subtype(self) -> &'static str {
        match self {
            MarkupStyle::Highlight => "Highlight",
            MarkupStyle::Underline => "Underline",
            MarkupStyle::StrikeOut => "StrikeOut",
        }
    }
}

fn real_array(values: &[f32]) -> Object {
    Object::Array(values.iter().map(|v| Object::Real(*v)).collect())
}

fn text_string(text: &str) -> Object {
    if text.is_ascii() {
        Object::String(text.as_bytes().to_vec(), StringFormat::Literal)
    } else {
        let mut bytes = vec![0xFE, 0xFF];
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_be_bytes());
        }
        Object::String(bytes, StringFormat::Hexadecimal)
    }
}

/// Linear part of the display-to-user matrix, used as an appearance stream `/Matrix` so content
/// authored upright in display space also appears upright on rotated pages.
fn linear_part(geometry: &PageGeometry) -> Object {
    let [a, b, c, d, _, _] = geometry.display_to_user();
    real_array(&[a, b, c, d, 0.0, 0.0])
}

impl Pdf {
    fn push_annotation(&mut self, page_id: ObjectId, annotation: ObjectId) -> Result<()> {
        enum Slot {
            Inline,
            Indirect(ObjectId),
            Missing,
        }
        let slot = match self.doc.get_dictionary(page_id)?.get(b"Annots") {
            Ok(Object::Array(_)) => Slot::Inline,
            Ok(Object::Reference(id)) => Slot::Indirect(*id),
            _ => Slot::Missing,
        };
        match slot {
            Slot::Inline => {
                let dict = self.doc.get_dictionary_mut(page_id)?;
                dict.get_mut(b"Annots")?.as_array_mut()?.push(Object::Reference(annotation));
            }
            Slot::Indirect(id) => {
                self.doc.get_object_mut(id)?.as_array_mut()?.push(Object::Reference(annotation));
            }
            Slot::Missing => {
                self.doc
                    .get_dictionary_mut(page_id)?
                    .set("Annots", Object::Array(vec![Object::Reference(annotation)]));
            }
        }
        Ok(())
    }

    /// Adds a highlight, underline or strikeout covering `rects` (display space, one per text line).
    pub fn add_markup(&mut self, page: usize, rects: &[Rect], style: MarkupStyle, color: Option<[f32; 3]>) -> Result<()> {
        let rects: Vec<Rect> = rects.iter().copied().filter(|r| !r.is_empty()).collect();
        if rects.is_empty() {
            return Ok(());
        }
        let page_id = self.page_id(page)?;
        let geometry = self.page_geometry(page)?;
        let [r, g, b] = color.unwrap_or_else(|| style.default_color());
        let user: Vec<Rect> = rects.iter().map(|rect| geometry.map_rect(*rect)).collect();

        let min_x = user.iter().map(|u| u.x).fold(f32::INFINITY, f32::min);
        let min_y = user.iter().map(|u| u.y).fold(f32::INFINITY, f32::min);
        let max_x = user.iter().map(|u| u.max_x()).fold(f32::NEG_INFINITY, f32::max);
        let max_y = user.iter().map(|u| u.max_y()).fold(f32::NEG_INFINITY, f32::max);
        let bounds = [min_x, min_y, max_x, max_y];

        let mut quads: Vec<f32> = Vec::new();
        let mut content = String::new();
        for u in &user {
            quads.extend_from_slice(&[u.x, u.max_y(), u.max_x(), u.max_y(), u.x, u.y, u.max_x(), u.y]);
            match style {
                MarkupStyle::Highlight => {
                    content.push_str(&format!("{} {} {} {} re f\n", fmt(u.x), fmt(u.y), fmt(u.width), fmt(u.height)));
                }
                MarkupStyle::Underline => {
                    let y = u.y + (u.height * 0.12).max(0.5);
                    content.push_str(&format!("{} {} m {} {} l S\n", fmt(u.x), fmt(y), fmt(u.max_x()), fmt(y)));
                }
                MarkupStyle::StrikeOut => {
                    let y = u.y + u.height * 0.5;
                    content.push_str(&format!("{} {} m {} {} l S\n", fmt(u.x), fmt(y), fmt(u.max_x()), fmt(y)));
                }
            }
        }

        let color_ops = match style {
            MarkupStyle::Highlight => format!("/GS gs {} {} {} rg\n", fmt(r), fmt(g), fmt(b)),
            _ => format!("{} {} {} RG 1 w\n", fmt(r), fmt(g), fmt(b)),
        };
        let appearance = self.doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Form",
                "BBox" => real_array(&bounds),
                "Resources" => dictionary! {
                    "ExtGState" => dictionary! {
                        "GS" => dictionary! { "Type" => "ExtGState", "BM" => "Multiply" },
                    },
                },
            },
            format!("{color_ops}{content}").into_bytes(),
        ));
        let annotation = self.doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => style.subtype(),
            "Rect" => real_array(&bounds),
            "QuadPoints" => real_array(&quads),
            "C" => real_array(&[r, g, b]),
            "F" => 4,
            "AP" => dictionary! { "N" => appearance },
        });
        self.push_annotation(page_id, annotation)
    }

    /// A 24x24 sticky note whose top-left corner is at (`x`, `y`) in display space.
    pub fn add_note(&mut self, page: usize, x: f32, y: f32, text: &str) -> Result<()> {
        let page_id = self.page_id(page)?;
        let geometry = self.page_geometry(page)?;
        const SIZE: f32 = 24.0;
        let rect = geometry.map_rect(Rect::new(x, y - SIZE, SIZE, SIZE));
        let icon = "1 0.9 0.2 rg 0 0 24 24 re f 0.4 0.3 0 RG 1 w 0.5 0.5 23 23 re S \
                    0.4 0.3 0 RG 5 17 m 19 17 l S 5 12 m 19 12 l S 5 7 m 14 7 l S\n";
        let appearance = self.doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Form",
                "BBox" => real_array(&[0.0, 0.0, SIZE, SIZE]),
                "Matrix" => linear_part(&geometry),
            },
            icon.as_bytes().to_vec(),
        ));
        let annotation = self.doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Text",
            "Rect" => real_array(&[rect.x, rect.y, rect.max_x(), rect.max_y()]),
            "Contents" => text_string(text),
            "Name" => "Note",
            "C" => real_array(&[1.0, 0.9, 0.2]),
            "Open" => false,
            // Print + NoZoom + NoRotate
            "F" => 28,
            "AP" => dictionary! { "N" => appearance },
        });
        self.push_annotation(page_id, annotation)
    }

    /// Free text ("typewriter") with its top-left corner at (`x`, `y`) in display space.
    pub fn add_text_box(&mut self, page: usize, x: f32, y: f32, text: &str, font_size: f32) -> Result<()> {
        let page_id = self.page_id(page)?;
        let geometry = self.page_geometry(page)?;
        let font = StandardFont::Helvetica;
        let lines: Vec<&str> = text.lines().collect();
        let widest = lines.iter().map(|l| font.text_width(l, font_size)).fold(0.0f32, f32::max);
        let leading = font_size * 1.2;
        let width = (widest + 8.0).max(24.0);
        let height = leading * lines.len().max(1) as f32 + 8.0;
        let rect = geometry.map_rect(Rect::new(x, y - height, width, height));

        let mut content: Vec<u8> = format!("BT\n/Helv {} Tf\n0 g\n{} TL\n", fmt(font_size), fmt(leading)).into_bytes();
        content.extend_from_slice(format!("4 {} Td\n", fmt(height - 4.0 - font_size)).as_bytes());
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                content.extend_from_slice(b"T*\n");
            }
            content.extend_from_slice(&literal_string(&win_ansi(line)));
            content.extend_from_slice(b" Tj\n");
        }
        content.extend_from_slice(b"ET\n");

        let helvetica = self.doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
            "Encoding" => "WinAnsiEncoding",
        });
        let appearance = self.doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Form",
                "BBox" => real_array(&[0.0, 0.0, width, height]),
                "Matrix" => linear_part(&geometry),
                "Resources" => dictionary! { "Font" => dictionary! { "Helv" => helvetica } },
            },
            content,
        ));
        let annotation = self.doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "FreeText",
            "Rect" => real_array(&[rect.x, rect.y, rect.max_x(), rect.max_y()]),
            "Contents" => text_string(text),
            "DA" => Object::String(format!("/Helv {} Tf 0 g", fmt(font_size)).into_bytes(), StringFormat::Literal),
            "Q" => 0,
            "F" => 4,
            "AP" => dictionary! { "N" => appearance },
        });
        self.push_annotation(page_id, annotation)
    }

    /// Annotation dictionaries of a page, for inspection and tests.
    pub fn annotations(&self, page: usize) -> Result<Vec<Dictionary>> {
        let page_id = self.page_id(page)?;
        Ok(self.doc.get_page_annotations(page_id)?.into_iter().cloned().collect())
    }

    /// Every comment-like annotation in the document (links, widgets and popups are left out),
    /// in page order, for a comments panel.
    pub fn annotation_summaries(&self) -> Vec<AnnotationSummary> {
        let mut out = Vec::new();
        for (page, page_id) in self.page_ids().into_iter().enumerate() {
            let Ok(annotations) = self.doc.get_page_annotations(page_id) else { continue };
            for dict in annotations {
                let kind = dict
                    .get(b"Subtype")
                    .ok()
                    .and_then(|o| o.as_name().ok())
                    .map(|n| String::from_utf8_lossy(n).into_owned())
                    .unwrap_or_default();
                if matches!(kind.as_str(), "Link" | "Widget" | "Popup" | "") {
                    continue;
                }
                let contents = match dict.get(b"Contents") {
                    Ok(Object::String(bytes, _)) => crate::revisions::decode_text(bytes),
                    _ => String::new(),
                };
                out.push(AnnotationSummary { page, kind, contents });
            }
        }
        out
    }
}
