use std::collections::BTreeMap;
use std::ops::RangeInclusive;
use std::path::Path;

use lopdf::{dictionary, Dictionary, Document, Object, ObjectId};

use crate::error::{Error, Result};
use crate::geometry::{normalize_rotation, PageGeometry};
use crate::security::ProtectionOptions;

/// A PDF document. All page indexes in this API are zero-based.
#[derive(Clone)]
pub struct Pdf {
    pub(crate) doc: Document,
}

const INHERITABLE: [&[u8]; 4] = [b"Resources", b"MediaBox", b"CropBox", b"Rotate"];

impl Pdf {
    // ---- creation, loading, saving -------------------------------------------------------

    /// A document with no pages.
    pub fn new() -> Pdf {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => Vec::<Object>::new(),
                "Count" => 0,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        Pdf { doc }
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Pdf> {
        Self::from_bytes(&std::fs::read(path)?)
    }

    pub fn open_with_password(path: impl AsRef<Path>, password: &str) -> Result<Pdf> {
        Self::from_bytes_with_password(&std::fs::read(path)?, password)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Pdf> {
        let doc = Document::load_mem(bytes)?;
        if doc.is_encrypted() {
            return Err(Error::PasswordRequired);
        }
        Ok(Pdf { doc })
    }

    pub fn from_bytes_with_password(bytes: &[u8], password: &str) -> Result<Pdf> {
        // The password has to be given to the loader: without it only the encryption dictionary
        // is read and the document comes back empty.
        match Document::load_mem_with_options(bytes, lopdf::LoadOptions::with_password(password)) {
            Ok(doc) => Ok(Pdf { doc }),
            Err(lopdf::Error::InvalidPassword) | Err(lopdf::Error::Decryption(_)) => Err(Error::WrongPassword),
            Err(other) => Err(other.into()),
        }
    }

    /// Serializes the document, dropping objects that are no longer reachable.
    pub fn to_bytes(&mut self) -> Result<Vec<u8>> {
        self.doc.prune_objects();
        let mut out = Vec::new();
        self.doc.save_to(&mut out)?;
        Ok(out)
    }

    pub fn save(&mut self, path: impl AsRef<Path>) -> Result<()> {
        let bytes = self.to_bytes()?;
        std::fs::write(path, bytes)?;
        Ok(())
    }

    /// Bytes that should replace the file on disk.
    ///
    /// `base_bytes` is the file this document was loaded from. The result starts with those exact
    /// bytes, so signatures and saved-revision prefixes stay valid, and a document with no changes
    /// is returned unchanged. Pass `None` for a document that has never been saved.
    ///
    /// When `password` is set, the result is a new AES-256 file that still requires that password.
    /// A decrypted document's ordinary serialization is plaintext; writing that over the original
    /// would remove the protection. The in-memory document stays decrypted.
    pub fn bytes_for_save(&mut self, base_bytes: Option<&[u8]>, password: Option<&str>) -> Result<Vec<u8>> {
        if let Some(password) = password.map(str::trim).filter(|p| !p.is_empty()) {
            let mut options = ProtectionOptions::new(password, password);
            options.allow_printing = true;
            options.allow_copying = true;
            options.allow_modifying = true;
            options.allow_annotating = true;
            return self.to_encrypted_bytes(&options);
        }
        match base_bytes {
            Some(base) => self.incremental_update(base),
            None => self.to_bytes(),
        }
    }

    pub fn is_encrypted(&self) -> bool {
        self.doc.is_encrypted()
    }

    /// Low-level access for features implemented outside this module.
    pub fn document(&self) -> &Document {
        &self.doc
    }

    pub fn document_mut(&mut self) -> &mut Document {
        &mut self.doc
    }

    // ---- pages ---------------------------------------------------------------------------

    pub fn page_count(&self) -> usize {
        self.doc.get_pages().len()
    }

    /// Page object ids in document order.
    pub fn page_ids(&self) -> Vec<ObjectId> {
        self.doc.get_pages().values().copied().collect()
    }

    pub fn page_id(&self, index: usize) -> Result<ObjectId> {
        let ids = self.page_ids();
        ids.get(index).copied().ok_or(Error::PageOutOfRange { index, count: ids.len() })
    }

    /// Looks `key` up on the page, then up the `/Parent` chain, and returns a dereferenced clone.
    pub(crate) fn inherited(&self, page_id: ObjectId, key: &[u8]) -> Option<Object> {
        let mut current = page_id;
        for _ in 0..64 {
            let dict = self.doc.get_dictionary(current).ok()?;
            if let Ok(value) = dict.get(key) {
                return Some(self.resolve(value));
            }
            current = dict.get(b"Parent").ok()?.as_reference().ok()?;
        }
        None
    }

    /// Follows references until a direct object is reached (bounded to avoid cycles).
    pub(crate) fn resolve(&self, object: &Object) -> Object {
        let mut current = object.clone();
        for _ in 0..16 {
            match current {
                Object::Reference(id) => match self.doc.get_object(id) {
                    Ok(next) => current = next.clone(),
                    Err(_) => return Object::Null,
                },
                other => return other,
            }
        }
        Object::Null
    }

    fn rect_from(&self, object: Option<Object>) -> Option<[f32; 4]> {
        let array = object?.as_array().ok()?.clone();
        if array.len() != 4 {
            return None;
        }
        let mut values = [0f32; 4];
        for (slot, item) in values.iter_mut().zip(array.iter()) {
            *slot = self.resolve(item).as_float().ok()?;
        }
        Some([
            values[0].min(values[2]),
            values[1].min(values[3]),
            values[0].max(values[2]),
            values[1].max(values[3]),
        ])
    }

    pub fn page_geometry(&self, index: usize) -> Result<PageGeometry> {
        let id = self.page_id(index)?;
        let media = self.rect_from(self.inherited(id, b"MediaBox")).unwrap_or([0.0, 0.0, 612.0, 792.0]);
        let crop = self.rect_from(self.inherited(id, b"CropBox"));
        let visible = match crop {
            Some(c) => [
                c[0].max(media[0]),
                c[1].max(media[1]),
                c[2].min(media[2]),
                c[3].min(media[3]),
            ],
            None => media,
        };
        let (w, h) = (visible[2] - visible[0], visible[3] - visible[1]);
        let (visible, w, h) = if w > 0.0 && h > 0.0 { (visible, w, h) } else { (media, media[2] - media[0], media[3] - media[1]) };
        let rotation = self
            .inherited(id, b"Rotate")
            .and_then(|o| o.as_i64().ok())
            .map(normalize_rotation)
            .unwrap_or(0);
        Ok(PageGeometry::new(visible[0], visible[1], w, h, rotation))
    }

    pub fn page_rotation(&self, index: usize) -> Result<i32> {
        Ok(self.page_geometry(index)?.rotation)
    }

    /// Makes the page tree a single flat `/Pages` node, copying inherited attributes onto each page.
    pub fn flatten_page_tree(&mut self) -> Result<()> {
        let ids = self.page_ids();
        let root_id = self.pages_root()?;

        let mut inherited_values: Vec<(ObjectId, Vec<(Vec<u8>, Object)>)> = Vec::new();
        for id in &ids {
            let mut values = Vec::new();
            for key in INHERITABLE {
                let own = self.doc.get_dictionary(*id)?.has(key);
                if !own {
                    if let Some(value) = self.inherited(*id, key) {
                        values.push((key.to_vec(), value));
                    }
                }
            }
            inherited_values.push((*id, values));
        }
        for (id, values) in inherited_values {
            let dict = self.doc.get_dictionary_mut(id)?;
            for (key, value) in values {
                dict.set(key, value);
            }
            dict.set("Parent", root_id);
        }
        let root = self.doc.get_dictionary_mut(root_id)?;
        root.set("Kids", ids.iter().map(|id| Object::Reference(*id)).collect::<Vec<_>>());
        root.set("Count", ids.len() as i64);
        Ok(())
    }

    pub(crate) fn pages_root(&self) -> Result<ObjectId> {
        let catalog = self.doc.catalog()?;
        Ok(catalog.get(b"Pages")?.as_reference()?)
    }

    pub fn rotate_pages(&mut self, pages: &[usize], degrees: i32) -> Result<()> {
        for &index in pages {
            let id = self.page_id(index)?;
            let current = self.page_rotation(index)?;
            let next = normalize_rotation((current + degrees) as i64);
            self.doc.get_dictionary_mut(id)?.set("Rotate", next as i64);
        }
        Ok(())
    }

    /// Deletes pages. A document must keep at least one page.
    pub fn delete_pages(&mut self, pages: &[usize]) -> Result<()> {
        let count = self.page_count();
        let mut unique: Vec<usize> = pages.to_vec();
        unique.sort_unstable();
        unique.dedup();
        if let Some(&bad) = unique.iter().find(|&&p| p >= count) {
            return Err(Error::PageOutOfRange { index: bad, count });
        }
        if unique.len() >= count {
            return Err(Error::Other("a document needs at least one page".into()));
        }
        let numbers: Vec<u32> = unique.iter().map(|&p| p as u32 + 1).collect();
        self.doc.delete_pages(&numbers);
        Ok(())
    }

    /// Inserts a blank page so that it ends up at `index`. Size defaults to the neighbouring page.
    pub fn insert_blank_page(&mut self, index: usize, size: Option<(f32, f32)>) -> Result<()> {
        let count = self.page_count();
        if index > count {
            return Err(Error::PageOutOfRange { index, count });
        }
        let (width, height) = match size {
            Some(s) => s,
            None if count > 0 => self.page_geometry(index.saturating_sub(1).min(count - 1))?.display_size(),
            None => (612.0, 792.0),
        };
        self.flatten_page_tree()?;
        let root_id = self.pages_root()?;
        let page_id = self.doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => root_id,
            "MediaBox" => vec![0.into(), 0.into(), Object::Real(width), Object::Real(height)],
            "Resources" => Dictionary::new(),
        });
        let root = self.doc.get_dictionary_mut(root_id)?;
        let kids = root.get_mut(b"Kids")?.as_array_mut()?;
        kids.insert(index, Object::Reference(page_id));
        let new_count = kids.len() as i64;
        root.set("Count", new_count);
        Ok(())
    }

    /// Moves a page so that it ends up at `to`.
    pub fn move_page(&mut self, from: usize, to: usize) -> Result<()> {
        let count = self.page_count();
        if from >= count {
            return Err(Error::PageOutOfRange { index: from, count });
        }
        if to >= count {
            return Err(Error::PageOutOfRange { index: to, count });
        }
        self.flatten_page_tree()?;
        let root_id = self.pages_root()?;
        let kids = self.doc.get_dictionary_mut(root_id)?.get_mut(b"Kids")?.as_array_mut()?;
        let item = kids.remove(from);
        kids.insert(to, item);
        Ok(())
    }

    /// A new document holding only the given pages, in the given order.
    pub fn extract_pages(&self, pages: &[usize]) -> Result<Pdf> {
        let count = self.page_count();
        if pages.is_empty() {
            return Err(Error::InvalidRange("no pages selected".into()));
        }
        if let Some(&bad) = pages.iter().find(|&&p| p >= count) {
            return Err(Error::PageOutOfRange { index: bad, count });
        }
        let mut copy = self.clone();
        copy.flatten_page_tree()?;
        let ids = copy.page_ids();
        let root_id = copy.pages_root()?;
        let kids: Vec<Object> = pages.iter().map(|&p| Object::Reference(ids[p])).collect();
        let root = copy.doc.get_dictionary_mut(root_id)?;
        root.set("Count", kids.len() as i64);
        root.set("Kids", kids);
        // Outlines would point at pages that no longer exist.
        if let Ok(catalog_id) = copy.doc.trailer.get(b"Root").and_then(|o| o.as_reference()) {
            if let Ok(catalog) = copy.doc.get_dictionary_mut(catalog_id) {
                catalog.remove(b"Outlines");
                catalog.remove(b"Names");
                catalog.remove(b"Dests");
                catalog.remove(b"OpenAction");
            }
        }
        copy.doc.prune_objects();
        Ok(copy)
    }

    pub fn split(&self, ranges: &[RangeInclusive<usize>]) -> Result<Vec<Pdf>> {
        ranges
            .iter()
            .map(|r| self.extract_pages(&r.clone().collect::<Vec<_>>()))
            .collect()
    }

    pub fn split_every_page(&self) -> Result<Vec<Pdf>> {
        (0..self.page_count()).map(|p| self.extract_pages(&[p])).collect()
    }

    /// Concatenates documents. Outlines and named destinations of the inputs are not carried over.
    pub fn merge(documents: &[&Pdf]) -> Result<Pdf> {
        if documents.is_empty() {
            return Ok(Pdf::new());
        }
        // The new document already owns a few low object ids (catalog, page tree); the inputs
        // are renumbered to start after them so nothing collides.
        let mut result = Pdf::new();
        let root_id = result.pages_root()?;
        let mut max_id: u32 = result.doc.max_id + 1;
        let mut page_ids: Vec<ObjectId> = Vec::new();
        let mut objects: BTreeMap<ObjectId, Object> = BTreeMap::new();

        for source in documents {
            let mut part = (*source).clone();
            part.flatten_page_tree()?;
            part.doc.renumber_objects_with(max_id);
            max_id = part.doc.max_id + 1;
            page_ids.extend(part.page_ids());
            let old_root_id = part.pages_root()?;
            for (id, object) in part.doc.objects {
                // The old catalog and page-tree root are replaced below.
                if id == old_root_id {
                    continue;
                }
                if let Object::Dictionary(d) = &object {
                    if d.has_type(b"Catalog") {
                        continue;
                    }
                }
                objects.insert(id, object);
            }
        }

        for (id, object) in objects {
            result.doc.objects.insert(id, object);
        }
        result.doc.max_id = result.doc.max_id.max(max_id);
        for id in &page_ids {
            result.doc.get_dictionary_mut(*id)?.set("Parent", root_id);
        }
        let root = result.doc.get_dictionary_mut(root_id)?;
        root.set("Kids", page_ids.iter().map(|id| Object::Reference(*id)).collect::<Vec<_>>());
        root.set("Count", page_ids.len() as i64);
        Ok(result)
    }

    /// Parses `1-3, 5, 8-10` (one-based, inclusive) into zero-based ranges validated against `page_count`.
    pub fn parse_ranges(text: &str, page_count: usize) -> Result<Vec<RangeInclusive<usize>>> {
        let mut ranges = Vec::new();
        for part in text.split(',') {
            let token = part.trim();
            if token.is_empty() {
                continue;
            }
            let bad = || Error::InvalidRange(token.to_string());
            let mut bounds = token.split('-').map(|s| s.trim());
            let first: usize = bounds.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
            let last: usize = match bounds.next() {
                Some(s) => s.parse().map_err(|_| bad())?,
                None => first,
            };
            if bounds.next().is_some() || first < 1 || last < first || last > page_count {
                return Err(bad());
            }
            ranges.push((first - 1)..=(last - 1));
        }
        if ranges.is_empty() {
            return Err(Error::InvalidRange(text.to_string()));
        }
        Ok(ranges)
    }
}

impl std::fmt::Debug for Pdf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pdf")
            .field("pages", &self.page_count())
            .field("objects", &self.doc.objects.len())
            .finish()
    }
}

impl Default for Pdf {
    fn default() -> Self {
        Pdf::new()
    }
}
