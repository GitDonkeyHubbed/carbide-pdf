//! Low-level helpers for adding content and resources to an existing page without disturbing it.

use lopdf::{dictionary, Dictionary, Object, ObjectId, Stream};

use crate::error::Result;
use crate::fonts::StandardFont;
use crate::pdf::Pdf;

impl Pdf {
    /// Appends `stamp` after the page's existing content. The original content is wrapped in
    /// `q`/`Q` so that any graphics state it leaves behind cannot affect the stamp.
    pub(crate) fn append_content(&mut self, page_id: ObjectId, stamp: Vec<u8>) -> Result<()> {
        let existing = self.doc.get_page_contents(page_id);
        let open = self.doc.add_object(Stream::new(Dictionary::new(), b"q\n".to_vec()));
        let close = self.doc.add_object(Stream::new(Dictionary::new(), b"\nQ\n".to_vec()));
        let stamp_id = self.doc.add_object(Stream::new(Dictionary::new(), stamp));

        let mut list: Vec<Object> = vec![Object::Reference(open)];
        list.extend(existing.into_iter().map(Object::Reference));
        list.push(Object::Reference(close));
        list.push(Object::Reference(stamp_id));
        self.doc.get_dictionary_mut(page_id)?.set("Contents", Object::Array(list));
        Ok(())
    }

    /// Adds `value` under `/Resources/<category>` on the page and returns the unique resource name.
    ///
    /// Inherited or indirect resource dictionaries are copied onto the page first, so other pages
    /// that share them are not modified.
    pub(crate) fn add_page_resource(
        &mut self,
        page_id: ObjectId,
        category: &str,
        base_name: &str,
        value: Object,
    ) -> Result<String> {
        let mut resources = self
            .inherited(page_id, b"Resources")
            .and_then(|o| o.as_dict().ok().cloned())
            .unwrap_or_default();
        let mut group = match resources.get(category.as_bytes()) {
            Ok(entry) => self.resolve(entry).as_dict().ok().cloned().unwrap_or_default(),
            Err(_) => Dictionary::new(),
        };
        let mut n = 1;
        let name = loop {
            let candidate = format!("{base_name}{n}");
            if !group.has(candidate.as_bytes()) {
                break candidate;
            }
            n += 1;
        };
        group.set(name.as_bytes().to_vec(), value);
        resources.set(category.as_bytes().to_vec(), Object::Dictionary(group));
        self.doc.get_dictionary_mut(page_id)?.set("Resources", Object::Dictionary(resources));
        Ok(name)
    }

    /// Registers one of the standard fonts on the page and returns its resource name.
    pub(crate) fn add_standard_font(&mut self, page_id: ObjectId, font: StandardFont) -> Result<String> {
        let font_id = self.doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => font.base_font(),
            "Encoding" => "WinAnsiEncoding",
        });
        self.add_page_resource(page_id, "Font", "CarbideF", Object::Reference(font_id))
    }

    /// Registers a constant-alpha graphics state (for both fills and strokes).
    pub(crate) fn add_alpha_state(&mut self, page_id: ObjectId, alpha: f32) -> Result<String> {
        let state = dictionary! {
            "Type" => "ExtGState",
            "ca" => Object::Real(alpha),
            "CA" => Object::Real(alpha),
        };
        self.add_page_resource(page_id, "ExtGState", "CarbideGS", Object::Dictionary(state))
    }
}
