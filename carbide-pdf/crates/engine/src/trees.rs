//! Repairs of the trees besides the page tree that PDFium reads whenever it opens a page or a
//! document: the page labels (read for every page) and the form's field tree (read to draw forms).
//!
//! PDFium follows both without remembering the nodes it has already been to. A page-label node
//! that lists itself makes it run out of stack, and so does a chain of a hundred thousand nodes; a
//! label or field tree whose nodes each list the next one twice makes it visit two to the power of
//! its depth nodes, so it never finishes. A few kilobytes of file would crash or freeze whatever
//! shows the document. So when a document is loaded, a page-label tree that lists a node more than
//! once or is nested more deeply than [`MAX_DEPTH`] is replaced by a single node holding each of
//! its labels once, and a field listed more than once in the field tree is kept where it is first
//! listed, the one PDFium reads, and dropped from the other places. Trees that need no repair are
//! left exactly as they are.

use std::collections::{BTreeMap, HashSet};

use lopdf::{dictionary, Document, Object, ObjectId};

/// Deeper page-label trees are rebuilt; real ones are two or three levels deep.
pub const MAX_DEPTH: usize = 32;

/// Repairs the page labels and the field tree of `doc` as the module documentation describes.
pub(crate) fn repair(doc: &mut Document) {
    let Ok(catalog_id) = doc.trailer.get(b"Root").and_then(Object::as_reference) else { return };
    repair_page_labels(doc, catalog_id);
    repair_fields(doc, catalog_id);
}

fn repair_page_labels(doc: &mut Document, catalog_id: ObjectId) {
    let Ok(tree) = doc.get_dictionary(catalog_id).and_then(|c| c.get(b"PageLabels")).cloned() else { return };
    if listed_once(doc, &tree, &mut HashSet::new(), 0) {
        return;
    }
    // Each page index once, in order, the first label given for it kept.
    let mut labels = BTreeMap::new();
    for (start, label) in labels_in(doc, tree) {
        if let Ok(start) = start.as_i64() {
            labels.entry(start).or_insert(label);
        }
    }
    let nums: Vec<Object> = labels.into_iter().flat_map(|(start, label)| [Object::Integer(start), label]).collect();
    let labels = doc.add_object(dictionary! { "Nums" => nums });
    if let Ok(catalog) = doc.get_dictionary_mut(catalog_id) {
        catalog.set("PageLabels", labels);
    }
}

/// The labels of the number tree `root`, each node read once, in the order PDFium reads them: a
/// node's own `/Nums` when it has them, and otherwise its kids one after the other.
fn labels_in(doc: &Document, root: Object) -> Vec<(Object, Object)> {
    let resolved = |object: &Object| match object {
        Object::Reference(id) => doc.get_object(*id).cloned().unwrap_or(Object::Null),
        other => other.clone(),
    };
    let mut labels = Vec::new();
    let mut seen = HashSet::new();
    let mut nodes = vec![root];
    while let Some(node) = nodes.pop() {
        if let Object::Reference(id) = node {
            if !seen.insert(id) {
                continue;
            }
        }
        let Object::Dictionary(dict) = resolved(&node) else { continue };
        if let Ok(nums) = dict.get(b"Nums") {
            if let Object::Array(nums) = resolved(nums) {
                labels.extend(nums.as_chunks::<2>().0.iter().map(|[start, label]| (resolved(start), label.clone())));
            }
        } else if let Ok(Object::Array(kids)) = dict.get(b"Kids").map(resolved) {
            nodes.extend(kids.into_iter().rev());
        }
    }
    labels
}

/// True when no node of the number tree `node` is listed twice and none is deeper than
/// [`MAX_DEPTH`].
fn listed_once(doc: &Document, node: &Object, seen: &mut HashSet<ObjectId>, depth: usize) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    let dict = match node {
        Object::Reference(id) if !seen.insert(*id) => return false,
        Object::Reference(id) => match doc.get_dictionary(*id) {
            Ok(dict) => dict,
            Err(_) => return true,
        },
        Object::Dictionary(dict) => dict,
        _ => return true,
    };
    let kids = match dict.get(b"Kids") {
        Ok(Object::Reference(id)) => doc.get_object(*id).ok(),
        other => other.ok(),
    };
    match kids {
        Some(Object::Array(kids)) => kids.iter().all(|kid| listed_once(doc, kid, seen, depth + 1)),
        _ => true,
    }
}

fn repair_fields(doc: &mut Document, catalog_id: ObjectId) {
    let form = match doc.get_dictionary(catalog_id).and_then(|c| c.get(b"AcroForm")) {
        Ok(Object::Reference(id)) => Holder::Object(*id),
        Ok(Object::Dictionary(_)) => Holder::InCatalog(catalog_id),
        _ => return,
    };
    list_once(doc, form, b"Fields", &mut HashSet::new(), 0);
}

/// Drops from the array of fields `key` of `holder`, and from the arrays below it, every field
/// already listed before, in the order PDFium reads them: the fields of an array one after the
/// other, each with all that is below it. `depth` is how deep the fields of the array are; PDFium
/// reads no deeper than [`MAX_DEPTH`].
fn list_once(doc: &mut Document, holder: Holder, key: &[u8], seen: &mut HashSet<ObjectId>, depth: usize) {
    let listed = match holder.get(doc, key) {
        Some(Object::Reference(id)) => match doc.get_object(id) {
            Ok(Object::Array(items)) => items.clone(),
            _ => return,
        },
        Some(Object::Array(items)) => items,
        _ => return,
    };
    let mut kept = Vec::with_capacity(listed.len());
    for kid in &listed {
        if let Object::Reference(id) = kid {
            if !seen.insert(*id) {
                continue;
            }
            if depth < MAX_DEPTH && doc.get_dictionary(*id).is_ok_and(|d| d.has(b"Kids")) {
                list_once(doc, Holder::Object(*id), b"Kids", seen, depth + 1);
            }
        }
        kept.push(kid.clone());
    }
    if kept.len() < listed.len() {
        holder.set(doc, key, kept);
    }
}

/// Where an array of fields is kept: in an object of its own, or in the form dictionary stored
/// directly in the catalog.
#[derive(Clone, Copy)]
enum Holder {
    Object(ObjectId),
    InCatalog(ObjectId),
}

impl Holder {
    fn dictionary_mut<'a>(&self, doc: &'a mut Document) -> Option<&'a mut lopdf::Dictionary> {
        match *self {
            Holder::Object(id) => doc.get_dictionary_mut(id).ok(),
            Holder::InCatalog(catalog) => match doc.get_dictionary_mut(catalog).ok()?.get_mut(b"AcroForm") {
                Ok(Object::Dictionary(form)) => Some(form),
                _ => None,
            },
        }
    }

    fn get(&self, doc: &mut Document, key: &[u8]) -> Option<Object> {
        self.dictionary_mut(doc)?.get(key).ok().cloned()
    }

    /// Stores `items` as `key`, directly in the dictionary: the array it held may be shared with
    /// other fields, which keep it as it was.
    fn set(&self, doc: &mut Document, key: &[u8], items: Vec<Object>) {
        if let Some(dict) = self.dictionary_mut(doc) {
            dict.set(key.to_vec(), items);
        }
    }
}
