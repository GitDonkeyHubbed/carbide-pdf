//! Removing what removed pages leave behind.
//!
//! Deleting a page, redacting it or leaving it out of an extract takes it out of the page tree,
//! but other parts of the document can still refer to it or to things only it used: form fields
//! hold the values of its widgets, structure elements hold its tagged text, links point at it,
//! and a resource dictionary shared between pages keeps the images and forms only it drew.
//! Anything still referred to is kept when the document is written, so these are cleaned up too.

use std::collections::{HashMap, HashSet};

use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::content::{name, tokens, Token};
use crate::error::{Error, Result};
use crate::limits::{messages, DecodeBudget, MAX_FORM_DEPTH};
use crate::pdf::Pdf;

const FONT: &[u8] = b"Font";
const XOBJECT: &[u8] = b"XObject";
const EXT_G_STATE: &[u8] = b"ExtGState";
const COLOR_SPACE: &[u8] = b"ColorSpace";
const PATTERN: &[u8] = b"Pattern";
const SHADING: &[u8] = b"Shading";
const PROPERTIES: &[u8] = b"Properties";

/// Resource categories whose entries content refers to by name.
const NAMED_RESOURCES: [&[u8]; 7] = [
    FONT,
    XOBJECT,
    EXT_G_STATE,
    COLOR_SPACE,
    PATTERN,
    SHADING,
    PROPERTIES,
];

/// Colour spaces that stand in for the device ones (`rg`, `g`, `k`, an image in `/DeviceRGB`)
/// without content naming them; kept wherever they are.
const DEFAULT_SPACES: [&[u8]; 3] = [b"DefaultGray", b"DefaultRGB", b"DefaultCMYK"];

/// The resources content uses: each name with the category it is looked up in. A font `/R1`
/// does not keep an image that is also called `/R1`.
type Uses = HashSet<(&'static [u8], Vec<u8>)>;

/// Deeper nesting than this is treated as damaged and left alone.
const MAX_DEPTH: usize = 64;

/// The most different names one page's content, with the forms, patterns, glyphs and
/// appearances it draws, may give the operators that look a resource up. Real pages give a few
/// hundred; each name is kept while the page is read, so without a limit millions of names in
/// content within the limit for one page in [`crate::limits`] could take gigabytes.
const MAX_NAMES: usize = 100_000;

impl Pdf {
    /// The annotations placed on `pages`, with the page each is on.
    fn annotations_on(&self, pages: &HashSet<ObjectId>) -> HashMap<ObjectId, ObjectId> {
        let mut found = HashMap::new();
        for page in pages {
            let Ok(dict) = self.doc.get_dictionary(*page) else {
                continue;
            };
            let Some(Object::Array(annots)) = dict.get(b"Annots").ok().map(|a| self.resolve(a))
            else {
                continue;
            };
            found.extend(
                annots
                    .iter()
                    .filter_map(|a| a.as_reference().ok())
                    .map(|a| (a, *page)),
            );
        }
        found
    }

    /// The annotations on `pages` that are on no page in `staying` as well: those that leave the
    /// document with the pages.
    pub(crate) fn annotations_leaving(
        &self,
        pages: &HashSet<ObjectId>,
        staying: &HashSet<ObjectId>,
    ) -> HashSet<ObjectId> {
        let kept = self.annotations_on(staying);
        self.annotations_on(pages)
            .into_keys()
            .filter(|id| !kept.contains_key(id))
            .collect()
    }

    /// Takes out the links other annotations have to `gone`, annotations that are leaving the
    /// document: the note a reply answers (`/IRT`), the annotation a pop-up belongs to (`/Parent`)
    /// and an annotation's pop-up (`/Popup`). A reply on a page that stays would otherwise keep the
    /// note it answers in the file, text and all.
    pub(crate) fn drop_annotation_links_to(&mut self, gone: &HashSet<ObjectId>) {
        // Annotations can also be written straight into a page's /Annots, so every dictionary is
        // looked at, wherever it is.
        fn strip(object: &mut Object, gone: &HashSet<ObjectId>) {
            match object {
                Object::Array(items) => items.iter_mut().for_each(|item| strip(item, gone)),
                Object::Dictionary(dict) => {
                    if dict.has(b"Subtype") {
                        for key in [&b"IRT"[..], b"Parent", b"Popup"] {
                            if dict
                                .get(key)
                                .and_then(Object::as_reference)
                                .is_ok_and(|id| gone.contains(&id))
                            {
                                dict.remove(key);
                            }
                        }
                    }
                    dict.iter_mut().for_each(|(_, value)| strip(value, gone));
                }
                _ => {}
            }
        }
        if gone.is_empty() {
            return;
        }
        for object in self.doc.objects.values_mut() {
            strip(object, gone);
        }
    }

    /// Takes widgets on `pages` out of the form, together with every field left without a widget,
    /// so their values and appearances are no longer part of the document. `goes` decides for each
    /// widget, given its page, its dictionary and whether it belongs to a signature field. A field
    /// that still has a widget elsewhere keeps its value: it is still shown there.
    pub(crate) fn drop_form_fields_on(
        &mut self,
        pages: &HashSet<ObjectId>,
        goes: &dyn Fn(ObjectId, &Dictionary, bool) -> bool,
    ) {
        let annotations = self.annotations_on(pages);
        let widgets = WidgetScope {
            pages,
            annotations: &annotations,
            goes,
        };
        let Ok(catalog_id) = self.doc.trailer.get(b"Root").and_then(Object::as_reference) else {
            return;
        };
        let Ok(form) = self
            .doc
            .get_dictionary(catalog_id)
            .and_then(|c| c.get(b"AcroForm"))
            .cloned()
        else {
            return;
        };
        let (form_id, mut form) = match form {
            Object::Reference(id) => match self.doc.get_dictionary(id) {
                Ok(dict) => (Some(id), dict.clone()),
                Err(_) => return,
            },
            Object::Dictionary(dict) => (None, dict),
            _ => return,
        };
        let Ok(mut fields) = form.get(b"Fields").cloned() else {
            return;
        };
        let mut removed = HashSet::new();
        let mut seen = HashSet::new();
        let left = edit_array(&mut self.doc, &mut fields, |doc, items| {
            items.retain_mut(|item| {
                !prune_field(doc, item, &widgets, None, &mut removed, &mut seen, 0)
            });
            items.len()
        });
        if removed.is_empty() {
            return;
        }
        form.set("Fields", fields);
        if let Ok(mut order) = form.get(b"CO").cloned() {
            edit_array(&mut self.doc, &mut order, |_, items| {
                items.retain(|item| !item.as_reference().is_ok_and(|id| removed.contains(&id)));
            });
            form.set("CO", order);
        }
        // The XFA packets can carry a copy of the field values.
        form.remove(b"XFA");
        match (left, form_id) {
            (Some(0), _) => {
                if let Ok(catalog) = self.doc.get_dictionary_mut(catalog_id) {
                    catalog.remove(b"AcroForm");
                }
            }
            (_, Some(id)) => {
                self.doc.objects.insert(id, Object::Dictionary(form));
            }
            (_, None) => {
                if let Ok(catalog) = self.doc.get_dictionary_mut(catalog_id) {
                    catalog.set("AcroForm", Object::Dictionary(form));
                }
            }
        }
    }

    /// Takes the structure elements (tags) of `pages` out of the structure tree, with everything
    /// below them, and the tree's references to their marked content and annotations.
