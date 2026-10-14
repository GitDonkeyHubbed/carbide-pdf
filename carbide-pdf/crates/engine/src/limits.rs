//! Limits on what a document can make the engine allocate.
//!
//! A stream compressed a thousand to one is a few hundred kilobytes in the file and gigabytes once
//! decoded, and several pages can share it. Every stream the engine decodes itself is decoded
//! against a budget, which fails with [`Error::TooLarge`] once it is spent: one for each page's
//! content when resources are trimmed, one for a whole operation when imposing or reducing.
//! Content is read token by token (see [`crate::content::tokens`]), never parsed into a list of
//! operations, which would take hundreds of times its size. Reducing decodes images within a
//! budget of their own, and leaves those past it as they are (see [`crate::reduce`]).
//!
//! While a file is read, lopdf decodes cross-reference streams, each within [`MAX_STREAM_ON_LOAD`];
//! object streams are unpacked by the engine (see [`crate::crypt`]), each within the same limit and
//! all of them within [`load_budget`], which counts what parsing their objects takes as well.
//!
//! PDFium has no limits of its own, so a page is checked before it is rendered or its text read
//! ([`Pdf::check_drawable`]): its content and the forms it draws may decode to no more than
//! [`MAX_DECODED_AT_ONCE`] and hold no more than [`MAX_DRAWN_OPERATORS`] operators, counting a
//! form each time it is drawn, and no form may draw itself. The size of rendered images is limited
//! in [`crate::render`].

use std::collections::HashMap;

use lopdf::{DecompressError, Dictionary, Document, Object, ObjectId, Stream};

use crate::content::{tokens, Token};
use crate::error::{Error, Result};
use crate::pdf::Pdf;

/// The most one cross-reference or object stream may decode to while a file is read.
pub const MAX_STREAM_ON_LOAD: usize = 16 << 20;

/// The most one page's content, or one stream, may decode to.
pub const MAX_DECODED_AT_ONCE: usize = 256 << 20;

/// The most content one operation (imposing, reducing) decodes in total.
pub const MAX_DECODED: usize = 1 << 30;

/// The most operators drawing one page may take, forms counted each time they are drawn. PDFium
/// keeps up to about 200 bytes for each (a saved graphics state, a path or a piece of text), so
/// this keeps what it needs for a page well under a gigabyte, and real pages far below it.
pub const MAX_DRAWN_OPERATORS: usize = 4_000_000;

pub mod messages {
    pub const TOO_LARGE: &str = "This document's content is too large to process safely: it decompresses to more than \
256 MB for one page or object, or more than 1 GB in all.";
    pub const TOO_LARGE_TO_READ: &str = "This document is too large to open safely: its compressed objects would take more \
memory than a file of its size can need.";
    pub const TOO_COMPLEX: &str = "This page is too complex to show safely: drawing it would take more than 256 MB of \
content or 4 million operations, or part of it draws itself.";
}

/// What the object streams of a file of `file_size` bytes may unpack to, objects parsed: 256
/// times the size of the file, at least 256 MB and at most 2 GB. Real files need a small part of
/// that; a crafted one can otherwise make a few kilobytes take gigabytes.
pub fn load_budget(file_size: usize) -> usize {
    file_size.saturating_mul(256).clamp(256 << 20, 2 << 30)
}

/// What is left of a decoding budget.
pub(crate) struct DecodeBudget {
    left: usize,
}

impl DecodeBudget {
    /// The budget of one operation.
    pub fn new() -> DecodeBudget {
        DecodeBudget { left: MAX_DECODED }
    }

    /// The budget of one page.
    pub fn for_page() -> DecodeBudget {
        DecodeBudget { left: MAX_DECODED_AT_ONCE }
    }

    /// The decoded data of `stream`, counted against the budget.
    pub fn stream(&mut self, stream: &Stream) -> Result<Vec<u8>> {
        let data = stream.get_plain_content_with_limit(self.at_once()).map_err(too_large)?;
        self.left = self.left.saturating_sub(data.len());
        Ok(data)
    }

    /// The content of the page `page`, its streams decoded and joined, counted against the budget.
    /// A stream that cannot be decoded is used as it is stored, as lopdf does.
    pub fn page_content(&mut self, doc: &Document, page: ObjectId) -> Result<Vec<u8>> {
        let data = doc.get_page_content_with_limit(page, self.at_once()).map_err(too_large)?;
        self.left = self.left.saturating_sub(data.len());
        Ok(data)
    }

    fn at_once(&self) -> usize {
        self.left.min(MAX_DECODED_AT_ONCE)
    }
}

pub(crate) fn too_large(error: lopdf::Error) -> Error {
    match error {
        lopdf::Error::Decompress(DecompressError::MemoryLimitExceeded { .. }) => Error::TooLarge(messages::TOO_LARGE.into()),
        other => Error::Pdf(other),
    }
}

/// What drawing some content takes: decoded bytes and operators, forms counted each time.
#[derive(Clone, Copy, Default)]
struct Cost {
    bytes: usize,
    operators: usize,
}

impl Cost {
    fn add(&mut self, other: Cost) -> Result<()> {
        self.bytes = self.bytes.saturating_add(other.bytes);
        self.operators = self.operators.saturating_add(other.operators);
        if self.bytes > MAX_DECODED_AT_ONCE || self.operators > MAX_DRAWN_OPERATORS {
            return Err(Error::TooLarge(messages::TOO_COMPLEX.into()));
        }
        Ok(())
    }
}

/// Works out what drawing a page takes, each form once for each set of resources it is drawn with.
struct Drawing<'a> {
    pdf: &'a Pdf,
    /// What drawing a form takes, by the form and the page or form whose resources it uses: a
    /// form without resources of its own uses those of what draws it.
    forms: HashMap<(ObjectId, ObjectId), Cost>,
    /// The forms being drawn, outermost first.
    path: Vec<ObjectId>,
}

impl Drawing<'_> {
    /// What drawing `data` takes, with `resources`, those of the page or form `holder`.
    fn content(&mut self, data: &[u8], resources: &Dictionary, holder: ObjectId) -> Result<Cost> {
        let xobjects = match resources.get(b"XObject").map(|o| self.pdf.resolve(o)) {
            Ok(Object::Dictionary(dict)) => dict,
            _ => Dictionary::new(),
        };
        let mut cost = Cost { bytes: data.len(), operators: 0 };
        let mut last_name: Option<Vec<u8>> = None;
        let mut failure = None;
        tokens(data, |token| {
            if failure.is_some() {
                return;
            }
            match token {
                Token::Name(name) => last_name = Some(crate::content::name(name).into_owned()),
                Token::Operator(operator) => {
                    let form = match (operator, last_name.take()) {
                        (b"Do", Some(name)) => xobjects.get(&name).ok().and_then(|o| o.as_reference().ok()),
                        _ => None,
                    };
                    let step = match form {
                        Some(id) => self.form(id, resources, holder).map(|form| Cost { operators: form.operators + 1, ..form }),
                        None => Ok(Cost { bytes: 0, operators: 1 }),
                    };
                    if let Err(error) = step.and_then(|step| cost.add(step)) {
                        failure = Some(error);
                    }
                }
                _ => {}
            }
        });
        match failure {
            Some(error) => Err(error),
            None => Ok(cost),
        }
    }

    /// What drawing the form XObject `id` takes; nothing for an image or anything else. A form
    /// without resources of its own uses `resources`, those of `holder`, which draws it.
    fn form(&mut self, id: ObjectId, resources: &Dictionary, holder: ObjectId) -> Result<Cost> {
        let Ok(Object::Stream(stream)) = self.pdf.doc.get_object(id) else { return Ok(Cost::default()) };
        if !stream.dict.get(b"Subtype").and_then(Object::as_name).is_ok_and(|s| s == b"Form") {
            return Ok(Cost::default());
        }
        let own = match stream.dict.get(b"Resources").map(|o| self.pdf.resolve(o)) {
            Ok(Object::Dictionary(dict)) => Some(dict),
            _ => None,
        };
        let holder = if own.is_some() { id } else { holder };
        if let Some(cost) = self.forms.get(&(id, holder)) {
            return Ok(*cost);
        }
        if self.path.contains(&id) {
            return Err(Error::TooLarge(messages::TOO_COMPLEX.into()));
        }
        let data = match stream.get_plain_content_with_limit(MAX_DECODED_AT_ONCE) {
            Ok(data) => data,
            Err(lopdf::Error::Decompress(DecompressError::MemoryLimitExceeded { .. })) => {
                return Err(Error::TooLarge(messages::TOO_COMPLEX.into()));
            }
            // One that cannot be decoded draws nothing.
            Err(_) => return Ok(Cost::default()),
        };
        self.path.push(id);
        let cost = self.content(&data, own.as_ref().unwrap_or(resources), holder);
        self.path.pop();
        let cost = cost?;
        self.forms.insert((id, holder), cost);
        Ok(cost)
    }
}

impl Pdf {
    /// Fails with [`Error::TooLarge`] when rendering page `index`, or reading its text, could
    /// make PDFium decode or draw more than the limits in [`crate::limits`] allow: its content and
    /// the forms it and its annotations draw (each time they are drawn) decode to more than
    /// [`MAX_DECODED_AT_ONCE`] or hold more than [`MAX_DRAWN_OPERATORS`] operators, or a form draws
    /// itself. PDFium has no such limits, and would take gigabytes or never finish.
    pub fn check_drawable(&self, index: usize) -> Result<()> {
        let id = self.page_id(index)?;
        let holder = self.inherited_from(id, b"Resources").unwrap_or(id);
        let resources = match self.inherited(id, b"Resources") {
            Some(Object::Dictionary(dict)) => dict,
            _ => Dictionary::new(),
        };
        let mut drawing = Drawing { pdf: self, forms: HashMap::new(), path: Vec::new() };
        let content = self.doc.get_page_content_with_limit(id, MAX_DECODED_AT_ONCE).map_err(|error| match too_large(error) {
            Error::TooLarge(_) => Error::TooLarge(messages::TOO_COMPLEX.into()),
            other => other,
        })?;
        let mut cost = drawing.content(&content, &resources, holder)?;
        drop(content);
        // Annotations are drawn from their appearance streams, forms that use the page's resources
        // when they have none of their own.
        let annotations = match self.doc.get_dictionary(id)?.get(b"Annots").map(|a| self.resolve(a)) {
            Ok(Object::Array(items)) => items,
            _ => Vec::new(),
        };
        for annotation in annotations {
            let Object::Dictionary(annotation) = self.resolve(&annotation) else { continue };
            let Ok(appearances) = annotation.get(b"AP").map(|ap| self.resolve(ap)) else { continue };
            let Ok(normal) = appearances.as_dict().and_then(|ap| ap.get(b"N")) else { continue };
            // One appearance, or one for each state of the annotation.
            let states = |states: &Dictionary| states.iter().filter_map(|(_, state)| state.as_reference().ok()).collect();
            let forms: Vec<ObjectId> = match normal {
                Object::Reference(id) => match self.doc.get_object(*id) {
                    Ok(Object::Dictionary(dict)) => states(dict),
                    _ => vec![*id],
                },
                Object::Dictionary(dict) => states(dict),
                _ => Vec::new(),
            };
            for form in forms {
                cost.add(drawing.form(form, &resources, holder)?)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Dictionary};

    #[test]
    fn the_budget_covers_every_stream_of_an_operation() {
        let stream = Stream::new(Dictionary::new(), vec![b'x'; 600]);
        let mut budget = DecodeBudget { left: 1000 };
        assert_eq!(budget.stream(&stream).unwrap().len(), 600);
        assert!(matches!(budget.stream(&stream), Err(Error::TooLarge(message)) if message == messages::TOO_LARGE));

        // A stream that cannot be decoded is an error of its own, not a limit.
        let unknown = Stream::new(dictionary! { "Filter" => "JBIG2Decode" }, b"data".to_vec());
        assert!(matches!(DecodeBudget::new().stream(&unknown), Err(Error::Pdf(_))));
    }
}
