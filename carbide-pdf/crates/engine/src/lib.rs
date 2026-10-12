//! Carbide PDF engine: PDF operations with no UI dependencies.
//!
//! Structure edits go through `lopdf` and are applied to the existing objects, so outlines, links
//! and form fields survive. PDFium (`render`) is only used to rasterize pages and read text.

pub mod annotate;
pub mod autosave;
pub mod compare;
mod content;
mod der;
pub mod error;
pub mod export;
pub mod fonts;
pub mod fs;
pub mod geometry;
pub mod imposition;
pub mod imposition_plan;
pub mod incremental;
pub mod ocr;
pub mod pdf;
pub mod redact;
pub mod reduce;
pub mod render;
pub mod revisions;
pub mod security;
pub mod signature;
pub mod stamp;

pub use annotate::{AnnotationSummary, MarkupStyle};
pub use compare::{compare_documents, PageDiff};
pub use error::{Error, Result};
pub use export::{export_images, export_text, ImageFormat};
pub use fonts::StandardFont;
pub use geometry::{PageGeometry, Rect};
pub use pdf::Pdf;
pub use redact::Redaction;
pub use render::Renderer;
pub use revisions::{open_revision_bytes, pdf_date_now, revision_display_name, validate_label, Revision};
pub use security::ProtectionOptions;
pub use stamp::{BatesOptions, PageAnchor, WatermarkOptions};
