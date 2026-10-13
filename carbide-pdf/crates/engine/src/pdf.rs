use std::collections::{BTreeMap, HashSet};
use std::ops::RangeInclusive;
use std::path::Path;

use lopdf::{dictionary, Dictionary, Document, Object, ObjectId};

use crate::error::{Error, Result};
use crate::geometry::{normalize_rotation, rotation_value, PageGeometry};
use crate::pagetree::INHERITABLE;
use crate::scrub::widget_area;

/// A PDF document. All page indexes in this API are zero-based.
#[derive(Clone)]
pub struct Pdf {
    pub(crate) doc: Document,
}

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

    /// Opens a file. An encrypted one opens only when it needs no password to be read (its
    /// restrictions are kept and saving encrypts it again). A damaged page tree is repaired as it
    /// is read (see [`crate::pagetree`]), or refused when that cannot be done safely.
    pub fn from_bytes(bytes: &[u8]) -> Result<Pdf> {
        let mut doc = Self::load_unrepaired(bytes, None)?;
        crate::pagetree::repair(&mut doc)?;
        Ok(Pdf { doc })
    }

    /// Opens a file with its open password or its owner password, used exactly as given.
    pub fn from_bytes_with_password(bytes: &[u8], password: &str) -> Result<Pdf> {
        let mut doc = Self::load_unrepaired(bytes, Some(password))?;
        crate::pagetree::repair(&mut doc)?;
        Ok(Pdf { doc })
    }

    /// The file's objects exactly as stored (decrypted), without repairing the page tree: what an
    /// update appended to the file has to be compared with, so that repairs are written too.
    pub(crate) fn load_unrepaired(bytes: &[u8], password: Option<&str>) -> Result<Document> {
        match crate::crypt::load(bytes, password) {
            Err(Error::Pdf(lopdf::Error::InvalidPassword | lopdf::Error::Decryption(_))) if password.is_some() => {
                Err(Error::WrongPassword)
            }
            other => other,
        }
    }

    /// Serializes the document, dropping objects that are no longer reachable.
    pub fn to_bytes(&mut self) -> Result<Vec<u8>> {
        crate::save::prune(&mut self.doc);
        let mut out = Vec::new();
        self.doc.save_to(&mut out)?;
        Ok(out)
    }

    /// Writes the document to `path` as [`Pdf::to_file_bytes`] does, replacing a file there in one
    /// step: a document that carries a protection is written encrypted with it. One protected with
    /// an open password needs that password, given to `to_file_bytes`.
    pub fn save(&mut self, path: impl AsRef<Path>) -> Result<()> {
        let bytes = self.to_file_bytes(None)?;
        crate::fs::write_replacing(path.as_ref(), &bytes)?;
        Ok(())
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
        self.doc.page_iter().count()
    }

    /// Page object ids in document order. Code that visits every page takes this list once
    /// instead of asking for each page by index, which walks the page tree each time.
    pub fn page_ids(&self) -> Vec<ObjectId> {
        self.doc.page_iter().collect()
    }

    pub fn page_id(&self, index: usize) -> Result<ObjectId> {
        match self.doc.page_iter().nth(index) {
            Some(id) => Ok(id),
            None => Err(Error::PageOutOfRange { index, count: self.page_count() }),
        }
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
        Ok(self.geometry_of(self.page_id(index)?))
    }

    /// The geometry of every page, in page order.
    pub fn page_geometries(&self) -> Vec<PageGeometry> {
        self.page_ids().into_iter().map(|id| self.geometry_of(id)).collect()
    }

    /// The geometry of the page object `id`.
    pub(crate) fn geometry_of(&self, id: ObjectId) -> PageGeometry {
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
        let rotation = self.inherited(id, b"Rotate").map(|o| rotation_value(&o)).unwrap_or(0);
        PageGeometry::new(visible[0], visible[1], w, h, rotation)
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
        // Every page holds its own values now. Left on the root they would also reach pages added
        // later, which have their own size and orientation.
        for key in INHERITABLE {
            root.remove(key);
        }
        Ok(())
    }

    pub(crate) fn pages_root(&self) -> Result<ObjectId> {
        let catalog = self.doc.catalog()?;
        Ok(catalog.get(b"Pages")?.as_reference()?)
    }

    /// Turns pages clockwise by `degrees`, a multiple of 90 (negative turns them the other way).
    /// A page listed more than once is turned once.
    pub fn rotate_pages(&mut self, pages: &[usize], degrees: i32) -> Result<()> {
        if degrees % 90 != 0 {
            return Err(Error::Other(format!("pages can only be turned by a multiple of 90 degrees, not {degrees}")));
        }
        let ids = self.page_ids();
        if let Some(&index) = pages.iter().find(|&&p| p >= ids.len()) {
            return Err(Error::PageOutOfRange { index, count: ids.len() });
        }
        for index in once_each(pages) {
            let id = ids[index];
            let current = self.geometry_of(id).rotation;
            let next = normalize_rotation(current as i64 + degrees as i64);
            self.doc.get_dictionary_mut(id)?.set("Rotate", next as i64);
        }
        Ok(())
    }

    /// Deletes pages. A document must keep at least one page. Nothing of a deleted page stays in
    /// the document: its form fields, its tags and the resources only it used go with it, and so do
    /// the outline items, named destinations and other entries that lead only to it. The pages that
    /// stay keep their page labels.
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
        let ids = self.page_ids();
        let doomed: HashSet<ObjectId> = unique.iter().map(|&p| ids[p]).collect();
        // What refers to the pages is found through them, so this comes first. An invisible
        // signature only sits on a page because every field has to; it is not the page's content.
        self.drop_form_fields_on(&doomed, &|_, widget, signature| !signature || widget_area(widget).is_some());
        self.drop_structure_of(&doomed);
        self.drop_navigation_to(&doomed);
        let labels = self.page_labels_without(&unique);
        // The pages leave every /Kids and everything else that names them in one pass, and the
        // counts are taken again from the tree itself; nothing follows /Parent, which can loop.
        self.drop_references_to(&doomed);
        for id in &doomed {
            self.doc.objects.remove(id);
        }
        crate::pagetree::repair(&mut self.doc)?;
        if let Some(labels) = labels {
            self.doc.catalog_mut()?.set("PageLabels", labels);
        }
        self.trim_resources()
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
        let page_box = vec![0.into(), 0.into(), Object::Real(width), Object::Real(height)];
        let page_id = self.doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => root_id,
            "MediaBox" => page_box.clone(),
            "CropBox" => page_box,
            "Rotate" => 0,
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

    /// A new document holding only the given pages, in the given order. Nothing of the other
    /// pages is carried along, not through form fields, tags, links or shared resources either.
    /// It carries this document's protection (see [`Pdf::protection`]).
    pub fn extract_pages(&self, pages: &[usize]) -> Result<Pdf> {
        let count = self.page_count();
        if pages.is_empty() {
            return Err(Error::InvalidRange("no pages selected".into()));
        }
        if let Some(&bad) = pages.iter().find(|&&p| p >= count) {
            return Err(Error::PageOutOfRange { index: bad, count });
        }
        // A page object can only be in the new document once; a page asked for twice is taken once.
        let pages = once_each(pages);
        let mut copy = self.clone();
        copy.flatten_page_tree()?;
        let ids = copy.page_ids();
        let kept: HashSet<ObjectId> = pages.iter().map(|&p| ids[p]).collect();
        let dropped: HashSet<ObjectId> = ids.iter().copied().filter(|id| !kept.contains(id)).collect();
        copy.drop_form_fields_on(&dropped, &|_, _, _| true);
        let root_id = copy.pages_root()?;
        let kids: Vec<Object> = pages.iter().map(|&p| Object::Reference(ids[p])).collect();
        let root = copy.doc.get_dictionary_mut(root_id)?;
        root.set("Count", kids.len() as i64);
        root.set("Kids", kids);
        // Outlines and page labels would describe pages that no longer exist, and the structure
        // tree holds the tagged content of every page.
        if let Ok(catalog_id) = copy.doc.trailer.get(b"Root").and_then(|o| o.as_reference()) {
            if let Ok(catalog) = copy.doc.get_dictionary_mut(catalog_id) {
                for key in [&b"Outlines"[..], b"Names", b"Dests", b"OpenAction", b"PageLabels", b"StructTreeRoot", b"MarkInfo"] {
                    catalog.remove(key);
                }
            }
        }
        // Links, beads and anything else that still points at a dropped page loses that target.
        copy.drop_references_to(&dropped);
        for id in &dropped {
            copy.doc.objects.remove(id);
        }
        copy.trim_resources()?;
        crate::save::prune(&mut copy.doc);
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
    /// The result carries the protection its protected inputs share; inputs protected in different
    /// ways are refused (see [`Pdf::shared_protection`]).
    pub fn merge(documents: &[&Pdf]) -> Result<Pdf> {
        if documents.is_empty() {
            return Err(Error::InvalidRange("there are no documents to merge".into()));
        }
        let protection = Pdf::shared_protection(documents)?;
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
        result.set_protection(protection);
        Ok(result)
    }

    /// Parses a page list such as `1-3, 5, 3` (one-based) into zero-based pages, each once, in the
    /// order they are first named: for edits that act on each page (rotate, watermark, recognize
    /// text, extract), where doing a page twice would turn it twice or stamp it twice.
    pub fn parse_pages(text: &str, page_count: usize) -> Result<Vec<usize>> {
        let all: Vec<usize> = Self::parse_ranges(text, page_count)?.into_iter().flatten().collect();
        Ok(once_each(&all))
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

/// `pages` with each page kept the first time it appears.
pub(crate) fn once_each(pages: &[usize]) -> Vec<usize> {
    let mut seen = HashSet::new();
    pages.iter().copied().filter(|page| seen.insert(*page)).collect()
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
