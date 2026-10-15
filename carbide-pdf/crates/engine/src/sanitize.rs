//! Remove Hidden Information: takes out what a document carries besides what its pages show.
//!
//! Metadata, attachments, scripts, form values, comments and the rest often repeat or reveal text
//! that was redacted or deleted from the pages. Each kind is chosen on its own, and what was taken
//! out is reported. The document has to be written in full afterwards (not as an appended update),
//! or the removed information stays in the file. Writing it in full also leaves out what only the
//! file holds: earlier versions that appended updates kept, and objects nothing refers to.

use std::collections::HashSet;

use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::error::Result;
use crate::incremental::file_structure;
use crate::navigation::collect_pairs;
use crate::pdf::Pdf;
use crate::save::SavedFile;

/// Deeper nesting than this is treated as damaged and left alone.
const MAX_DEPTH: usize = 64;

/// What [`Pdf::remove_hidden_information`] can take out, each on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HiddenItem {
    /// The document information dictionary: title, author, subject, keywords, dates.
    Metadata,
    /// XMP metadata, of the document and of its pages, images and other objects.
    Xmp,
    /// Embedded files: attachments of the document and file attachment comments.
    Attachments,
    /// Document JavaScript, the action run when the document opens, and actions run on events.
    Scripts,
    /// Values filled into form fields, with their drawn appearances and any XFA form data.
    FormData,
    /// Comments and markup: notes, text boxes (typewriter text), highlights, drawings, stamps.
    Comments,
    /// Page thumbnails.
    Thumbnails,
    /// Objects nothing in the document uses, and resources no page draws.
    UnusedObjects,
    /// Data applications keep for themselves (page-piece dictionaries).
    PrivateData,
    /// The outline (bookmarks) and named destinations, whose titles and names can repeat page text.
    Bookmarks,
}

impl HiddenItem {
    pub const ALL: [HiddenItem; 10] = [
        HiddenItem::Metadata,
        HiddenItem::Xmp,
        HiddenItem::Attachments,
        HiddenItem::Scripts,
        HiddenItem::FormData,
        HiddenItem::Comments,
        HiddenItem::Thumbnails,
        HiddenItem::UnusedObjects,
        HiddenItem::PrivateData,
        HiddenItem::Bookmarks,
    ];

    /// Chosen unless the user says otherwise: everything but the bookmarks, which readers use to
    /// find their way around.
    pub fn chosen_by_default(self) -> bool {
        self != HiddenItem::Bookmarks
    }

    pub fn label(self) -> &'static str {
        match self {
            HiddenItem::Metadata => "Document information (title, author, keywords)",
            HiddenItem::Xmp => "XMP metadata",
            HiddenItem::Attachments => "Attachments and embedded files",
            HiddenItem::Scripts => "JavaScript and actions",
            HiddenItem::FormData => "Form field values",
            HiddenItem::Comments => "Comments, text boxes and markup",
            HiddenItem::Thumbnails => "Page thumbnails",
            HiddenItem::UnusedObjects => "Unused objects and resources",
            HiddenItem::PrivateData => "Private application data",
            HiddenItem::Bookmarks => "Bookmarks and named destinations",
        }
    }
}

/// How [`HiddenReport::lines`] names what only the file holds.
pub const IN_FILE_LABEL: &str = "Earlier versions and unused objects in the file";

/// What [`Pdf::remove_hidden_information`] took out: how much of each item it found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HiddenReport {
    removed: Vec<(HiddenItem, usize)>,
    /// Earlier versions and unused objects only the file holds. Writing the document in full
    /// leaves them out, whatever was chosen.
    in_file: usize,
}

impl HiddenReport {
    /// How much of `item` was taken out: entries, streams, files, scripts, fields, comments or
    /// objects, whichever it holds.
    pub fn count(&self, item: HiddenItem) -> usize {
        self.removed
            .iter()
            .filter(|(i, _)| *i == item)
            .map(|(_, n)| n)
            .sum()
    }

    /// How many earlier versions (appended updates) and unused objects the file holds besides the
    /// document; see [`Pdf::remove_hidden_information_from`].
    pub fn in_file(&self) -> usize {
        self.in_file
    }

    /// True when nothing was found.
    pub fn is_empty(&self) -> bool {
        self.in_file == 0 && self.removed.iter().all(|(_, n)| *n == 0)
    }

    /// One line per item that was found, such as "Attachments and embedded files: 2", and one for
    /// what only the file holds.
    pub fn lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .removed
            .iter()
            .filter(|(_, n)| *n > 0)
            .map(|(item, n)| format!("{}: {n}", item.label()))
            .collect();
        if self.in_file > 0 {
            lines.push(format!("{IN_FILE_LABEL}: {}", self.in_file));
        }
        lines
    }
}

impl Pdf {
    /// Takes `items` out of the document and reports how much of each it found. Comments go
    /// without their pop-ups; links and form fields are not comments and stay. Write the document
    /// in full afterwards: an appended update would leave the removed information in the file.
    pub fn remove_hidden_information(&mut self, items: &[HiddenItem]) -> Result<HiddenReport> {
        self.remove_hidden_information_from(items, None)
    }

    /// [`Pdf::remove_hidden_information`] for a document opened from `file`, the file it is saved
    /// over. What only that file holds is reported too: the earlier versions appended updates kept
    /// in it and the objects nothing refers to, which the full rewrite that has to follow leaves
    /// out. They are reported even when nothing else is found, since saving still removes them.
    pub fn remove_hidden_information_from(
        &mut self,
        items: &[HiddenItem],
        file: Option<&SavedFile>,
    ) -> Result<HiddenReport> {
        let mut report = HiddenReport::default();
        let chosen = |item| items.contains(&item);
        let empty = HashSet::new();
        let in_file = file.map_or(&empty, SavedFile::unused_objects);
        report.in_file = in_file.len() + file.map_or(0, SavedFile::appended_updates);
        // Objects already unused are counted before the other items leave more of them behind.
        // Those the file holds unused are counted with the file; how the file was written (its
        // cross-reference and object streams, linearization data, stream lengths) does not count.
        if chosen(HiddenItem::UnusedObjects) {
            let structure = file_structure(&self.doc);
            self.trim_resources()?;
            let removed = crate::save::prune(&mut self.doc)
                .into_iter()
                .filter(|id| !structure.contains(id) && !in_file.contains(id))
                .count();
            report.removed.push((HiddenItem::UnusedObjects, removed));
        }
        for item in HiddenItem::ALL
            .into_iter()
            .filter(|&i| chosen(i) && i != HiddenItem::UnusedObjects)
        {
            let count = match item {
                HiddenItem::Metadata => self.remove_info(),
                HiddenItem::Xmp => remove_key_everywhere(&mut self.doc, b"Metadata"),
                HiddenItem::Attachments => self.remove_attachments(),
                HiddenItem::Scripts => self.remove_scripts(),
                HiddenItem::FormData => self.remove_form_data(),
                HiddenItem::Comments => {
                    self.remove_annotations(&|subtype| !matches!(subtype, b"Link" | b"Widget"))
                }
                HiddenItem::Thumbnails => self
                    .page_ids()
                    .into_iter()
                    .filter(|&page| self.remove_from(page, b"Thumb"))
                    .count(),
                HiddenItem::PrivateData => {
                    let modified = remove_key_everywhere(&mut self.doc, b"LastModified");
                    let spider = self
                        .doc
                        .catalog_mut()
                        .map(|c| c.remove(b"SpiderInfo").is_some())
                        .unwrap_or(false);
                    modified
                        + remove_key_everywhere(&mut self.doc, b"PieceInfo")
                        + usize::from(spider)
                }
                HiddenItem::Bookmarks => self.remove_bookmarks(),
                HiddenItem::UnusedObjects => 0,
            };
            report.removed.push((item, count));
        }
        // What the removals left unused goes too, so it is not written out with the document.
        crate::save::prune(&mut self.doc);
        Ok(report)
    }

    /// Removes `key` from the dictionary `id`; true when it was there.
    fn remove_from(&mut self, id: ObjectId, key: &[u8]) -> bool {
        self.doc
            .get_dictionary_mut(id)
            .map(|d| d.remove(key).is_some())
            .unwrap_or(false)
    }

    /// The document information dictionary; the count is how many entries it had.
    fn remove_info(&mut self) -> usize {
        let Some(info) = self.doc.trailer.remove(b"Info") else {
            return 0;
        };
        match self.resolve(&info) {
            Object::Dictionary(dict) => dict.len().max(1),
            _ => 1,
        }
    }

    /// Removes the name tree `key` from the catalog's /Names and returns how many entries it had.
    fn remove_name_tree(&mut self, key: &[u8]) -> usize {
        let Some(tree) = self.name_tree(key) else {
            return 0;
        };
        let mut pairs = Vec::new();
        collect_pairs(&self.doc, &tree, b"Names", &mut pairs);
        let names = self
            .doc
            .catalog()
            .ok()
            .and_then(|c| c.get(b"Names").ok().cloned());
        match names {
            Some(Object::Reference(id)) => {
                self.remove_from(id, key);
            }
            Some(Object::Dictionary(mut dict)) => {
                dict.remove(key);
                if let Ok(catalog) = self.doc.catalog_mut() {
                    catalog.set("Names", dict);
                }
            }
            _ => {}
        }
        pairs.len()
    }

    /// Embedded files of the document, file attachment comments, associated files and the
    /// portfolio view.
    fn remove_attachments(&mut self) -> usize {
        let mut count = self.remove_name_tree(b"EmbeddedFiles");
        count += self.remove_annotations(&|subtype| subtype == b"FileAttachment");
        count += remove_key_everywhere(&mut self.doc, b"AF");
        if let Ok(catalog) = self.doc.catalog_mut() {
            count += usize::from(catalog.remove(b"Collection").is_some());
        }
        count
    }

    /// Document JavaScript, the open action, additional actions (/AA) everywhere, the JavaScript and
    /// Launch actions that links, form fields and bookmarks run, wherever they sit in a chain of
    /// actions (/Next), and the scripts of rendition actions. A chain closes up around an action
    /// that goes: the actions after it take its place.
    fn remove_scripts(&mut self) -> usize {
        let mut count = self.remove_name_tree(b"JavaScript");
        if let Ok(catalog) = self.doc.catalog_mut() {
            count += usize::from(catalog.remove(b"OpenAction").is_some());
        }
        count += remove_key_everywhere(&mut self.doc, b"AA");
        let code: HashSet<ObjectId> = self
            .doc
            .objects
            .iter()
            .filter(|(_, object)| object.as_dict().is_ok_and(runs_code))
            .map(|(id, _)| *id)
            .collect();
        let mut removed = HashSet::new();
        let ids: Vec<ObjectId> = self.doc.objects.keys().copied().collect();
        for id in ids {
            // Taken out while it is changed, so the rest of the document can still be read.
            let Some(mut object) = self.doc.objects.remove(&id) else {
                continue;
            };
            count += strip_code(&self.doc, &mut object, &code, &mut removed, 0);
            self.doc.objects.insert(id, object);
        }
        count + removed.len()
    }

    /// The values of form fields (signatures excepted), the options picked in lists and the
    /// default values, the appearances that draw them, and any XFA form data. Viewers draw the
    /// emptied fields afresh.
    fn remove_form_data(&mut self) -> usize {
        let Ok(catalog_id) = self.doc.trailer.get(b"Root").and_then(Object::as_reference) else {
            return 0;
        };
        let Ok(form) = self
            .doc
            .get_dictionary(catalog_id)
            .and_then(|c| c.get(b"AcroForm"))
            .cloned()
        else {
            return 0;
        };
        let form_id = form.as_reference().ok();
        let Some(mut dict) = self.resolve(&form).as_dict().ok().cloned() else {
            return 0;
        };
        let mut count = usize::from(dict.remove(b"XFA").is_some());
        let fields: Vec<Object> = match dict.get(b"Fields").map(|f| self.resolve(f)) {
            Ok(Object::Array(fields)) => fields,
            _ => Vec::new(),
        };
        let mut visited = HashSet::new();
        for field in &fields {
            count += self.clear_field(field, None, &mut visited, 0);
        }
        if count > 0 {
            dict.set("NeedAppearances", true);
        }
        match form_id {
            Some(id) => {
                self.doc.objects.insert(id, Object::Dictionary(dict));
            }
            None => {
                if let Ok(catalog) = self.doc.get_dictionary_mut(catalog_id) {
                    catalog.set("AcroForm", dict);
                }
            }
        }
        count
    }

    /// Clears the value of the field `field` and of the fields below it, and the appearances of
    /// their widgets. `field_type` is the type (/FT) it inherits. Returns how many fields it cleared.
    fn clear_field(
        &mut self,
        field: &Object,
        field_type: Option<&[u8]>,
        visited: &mut HashSet<ObjectId>,
        depth: usize,
    ) -> usize {
        let Ok(id) = field.as_reference() else {
            return 0;
        };
        if depth > MAX_DEPTH || !visited.insert(id) {
            return 0;
        }
        let Ok(dict) = self.doc.get_dictionary(id).cloned() else {
            return 0;
        };
        let field_type = dict
            .get(b"FT")
            .and_then(Object::as_name)
            .ok()
            .or(field_type)
            .map(<[u8]>::to_vec);
        if field_type.as_deref() == Some(b"Sig") {
            return 0;
        }
        let mut count = 0;
        if let Ok(dict) = self.doc.get_dictionary_mut(id) {
            // The value, its rich text, the options picked in a list (/I) and the value a reset
            // restores (/DV).
            let cleared = [&b"V"[..], b"RV", b"I", b"DV"]
                .into_iter()
                .filter(|key| dict.remove(key).is_some())
                .count();
            let is_widget = dict
                .get(b"Subtype")
                .and_then(Object::as_name)
                .is_ok_and(|s| s == b"Widget");
            // A widget can show a value its field no longer holds; clearing that counts too.
            let shown = is_widget
                && match field_type.as_deref() {
                    // A check box or radio button shows its state through /AS.
                    Some(b"Btn") => {
                        let on = dict
                            .get(b"AS")
                            .and_then(Object::as_name)
                            .is_ok_and(|state| state != b"Off");
                        dict.set("AS", Object::Name(b"Off".to_vec()));
                        on
                    }
                    _ => dict.remove(b"AP").is_some(),
                };
            count += usize::from(cleared > 0 || shown);
        }
        if let Ok(Object::Array(kids)) = dict.get(b"Kids").map(|k| self.resolve(k)) {
            for kid in &kids {
                count += self.clear_field(kid, field_type.as_deref(), visited, depth + 1);
            }
        }
        count
    }

    /// Removes the annotations whose subtype `goes` gives up from every page, with their pop-ups,
    /// and every reference to them. Returns how many there were, pop-ups not counted.
    fn remove_annotations(&mut self, goes: &dyn Fn(&[u8]) -> bool) -> usize {
        let mut removed: HashSet<ObjectId> = HashSet::new();
        let mut counted = 0;
        for page in self.page_ids() {
            let annotations = match self
                .doc
                .get_dictionary(page)
                .and_then(|d| d.get(b"Annots"))
                .map(|a| self.resolve(a))
            {
                Ok(Object::Array(items)) => items,
                _ => continue,
            };
            let mut kept = Vec::with_capacity(annotations.len());
            let mut popups = HashSet::new();
            let subtype = |doc: &Document, annotation: &Object| -> Option<Vec<u8>> {
                let dict = match annotation {
                    Object::Reference(id) => doc.get_dictionary(*id).ok()?,
                    Object::Dictionary(dict) => dict,
                    _ => return None,
                };
                dict.get(b"Subtype")
                    .and_then(Object::as_name)
                    .ok()
                    .map(<[u8]>::to_vec)
            };
            for annotation in &annotations {
                let kind = subtype(&self.doc, annotation).unwrap_or_default();
                if kind != b"Popup" && goes(&kind) {
                    counted += 1;
                    if let Ok(id) = annotation.as_reference() {
                        removed.insert(id);
                        if let Ok(popup) = self
                            .doc
                            .get_dictionary(id)
                            .and_then(|d| d.get(b"Popup"))
                            .and_then(Object::as_reference)
                        {
                            popups.insert(popup);
                        }
                    }
                } else {
                    kept.push(annotation.clone());
                }
            }
            // A pop-up goes with its comment, or when it belongs to no annotation that stays.
            kept.retain(|annotation| {
                let Ok(id) = annotation.as_reference() else {
                    return true;
                };
                let parent = self
                    .doc
                    .get_dictionary(id)
                    .and_then(|d| d.get(b"Parent"))
                    .and_then(Object::as_reference)
                    .ok();
                let goes = popups.contains(&id) || parent.is_some_and(|p| removed.contains(&p));
                if goes {
                    removed.insert(id);
                }
                !goes
            });
            if kept.len() == annotations.len() {
                continue;
            }
            let stored = self
                .doc
                .get_dictionary(page)
                .and_then(|d| d.get(b"Annots"))
                .cloned();
            match stored {
                Ok(Object::Reference(id)) => {
                    self.doc.objects.insert(id, Object::Array(kept));
                }
                _ => {
                    if let Ok(dict) = self.doc.get_dictionary_mut(page) {
                        if kept.is_empty() {
                            dict.remove(b"Annots");
                        } else {
                            dict.set("Annots", kept);
                        }
                    }
                }
            }
        }
        if !removed.is_empty() {
            // The structure tree and anything else that still points at them lets go.
            self.drop_references_to(&removed);
        }
        counted
    }

    /// The outline and the named destinations; the count is how many items and names there were.
    fn remove_bookmarks(&mut self) -> usize {
        let mut count = 0;
        if let Ok(Object::Reference(root)) =
            self.doc.catalog().and_then(|c| c.get(b"Outlines")).cloned()
        {
            count += count_outline(&self.doc, root);
        }
        if let Ok(Object::Dictionary(dests)) = self
            .doc
            .catalog()
            .and_then(|c| c.get(b"Dests"))
            .map(|d| self.resolve(d))
        {
            count += dests.len();
        }
        if let Ok(catalog) = self.doc.catalog_mut() {
            catalog.remove(b"Outlines");
            catalog.remove(b"Dests");
        }
        count += self.remove_name_tree(b"Dests");
        count
    }
}

/// How many outline items there are below `node`.
fn count_outline(doc: &Document, node: ObjectId) -> usize {
    let mut count = 0;
    let mut pending = vec![node];
    let mut seen = HashSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        let Ok(dict) = doc.get_dictionary(id) else {
            continue;
        };
        if id != node {
            count += 1;
        }
        for key in [&b"First"[..], b"Next"] {
            if let Ok(next) = dict.get(key).and_then(Object::as_reference) {
                pending.push(next);
            }
        }
    }
    count
}

/// True for an action that runs code: JavaScript, or launching an application or file.
fn runs_code(action: &Dictionary) -> bool {
    action
        .get(b"S")
        .and_then(Object::as_name)
        .is_ok_and(|s| matches!(s, b"JavaScript" | b"Launch"))
}

/// Takes the actions that run code out of the action chains in `object` and in everything it holds
/// directly. `code` are the actions stored as objects of their own that run code; those taken out
/// are added to `removed`. Returns how many actions held directly were taken out, and how many
/// rendition scripts.
fn strip_code(
    doc: &Document,
    object: &mut Object,
    code: &HashSet<ObjectId>,
    removed: &mut HashSet<ObjectId>,
    depth: usize,
) -> usize {
    if depth > MAX_DEPTH {
        return 0;
    }
    let dict = match object {
        Object::Dictionary(dict) => dict,
        Object::Stream(stream) => &mut stream.dict,
        Object::Array(items) => {
            return items
                .iter_mut()
                .map(|item| strip_code(doc, item, code, removed, depth + 1))
                .sum()
        }
        _ => return 0,
    };
    let mut count = 0;
    if dict
        .get(b"S")
        .and_then(Object::as_name)
        .is_ok_and(|s| s == b"Rendition")
        && dict.remove(b"JS").is_some()
    {
        count += 1;
    }
    for key in [&b"A"[..], b"Next"] {
        let Ok(value) = dict.get(key) else { continue };
        let mut kept = Vec::new();
        if !splice(
            doc,
            value,
            code,
            removed,
            &mut kept,
            &mut count,
            &mut HashSet::new(),
            0,
        ) {
            continue;
        }
        match kept.len() {
            0 => {
                dict.remove(key);
            }
            1 => dict.set(key.to_vec(), kept.remove(0)),
            _ if key == b"Next" => dict.set(key.to_vec(), kept),
            // /A holds one action: the first, as a copy that the others follow.
            _ => {
                let first = kept.remove(0);
                let Ok(mut first) = (match &first {
                    Object::Reference(id) => doc.get_dictionary(*id).cloned(),
                    other => other.as_dict().cloned(),
                }) else {
                    dict.remove(key);
                    continue;
                };
                let mut next = match first.get(b"Next") {
                    Ok(Object::Array(items)) => items.clone(),
                    Ok(other) => vec![other.clone()],
                    Err(_) => Vec::new(),
                };
                next.extend(kept);
                first.set("Next", next);
                dict.set(key.to_vec(), first);
            }
        }
    }
    for (_, value) in dict.iter_mut() {
        count += strip_code(doc, value, code, removed, depth + 1);
    }
    count
}

/// Puts the actions `value` (an action, or an array of them) runs into `kept`, each that runs code
/// replaced by the actions that follow it (its /Next). Returns true when one was taken out. Those
/// held directly are counted in `count`, the others added to `removed`; `chain` guards against
/// chains that loop.
#[allow(clippy::too_many_arguments)]
fn splice(
    doc: &Document,
    value: &Object,
    code: &HashSet<ObjectId>,
    removed: &mut HashSet<ObjectId>,
    kept: &mut Vec<Object>,
    count: &mut usize,
    chain: &mut HashSet<ObjectId>,
    depth: usize,
) -> bool {
    // A chain this deep is damaged; what is left of it goes.
    if depth > MAX_DEPTH {
        return true;
    }
    let next = match value {
        Object::Array(items) => {
            let mut changed = false;
            for item in items {
                changed |= splice(doc, item, code, removed, kept, count, chain, depth + 1);
            }
            return changed;
        }
        Object::Reference(id) if code.contains(id) => {
            removed.insert(*id);
            if !chain.insert(*id) {
                return true;
            }
            doc.get_dictionary(*id)
                .ok()
                .and_then(|action| action.get(b"Next").ok())
        }
        Object::Dictionary(action) if runs_code(action) => {
            *count += 1;
            action.get(b"Next").ok()
        }
        other => {
            kept.push(other.clone());
            return false;
        }
    };
    if let Some(next) = next {
        splice(doc, next, code, removed, kept, count, chain, depth + 1);
    }
    true
}

/// Removes `key` from every dictionary and stream dictionary in the document, and returns how many
/// it was removed from.
fn remove_key_everywhere(doc: &mut Document, key: &[u8]) -> usize {
    fn strip(dict: &mut Dictionary, key: &[u8], depth: usize) -> usize {
        if depth > MAX_DEPTH {
            return 0;
        }
        let mut count = usize::from(dict.remove(key).is_some());
        for (_, value) in dict.iter_mut() {
            count += strip_object(value, key, depth + 1);
        }
        count
    }
    fn strip_object(object: &mut Object, key: &[u8], depth: usize) -> usize {
        match object {
            Object::Dictionary(dict) => strip(dict, key, depth),
            Object::Stream(stream) => strip(&mut stream.dict, key, depth),
            Object::Array(items) => items
                .iter_mut()
                .map(|item| strip_object(item, key, depth + 1))
                .sum(),
            _ => 0,
        }
    }
    doc.objects
        .values_mut()
        .map(|object| strip_object(object, key, 0))
        .sum()
}
