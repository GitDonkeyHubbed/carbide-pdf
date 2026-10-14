//! The page tree, made to read the same way to every part of the engine.
//!
//! Structure edits find pages with lopdf, which counts a node as a page only when its `/Type` says
//! so, skips kids that are not references, and follows `/Kids` loops until it runs out of objects.
//! Rendering and text go through PDFium, which takes every node without `/Kids` for a page, trusts
//! the root's `/Count`, and skips loops. On a damaged tree the two would disagree about which page
//! is page N, so a redaction could black out a page other than the one shown and a structural edit
//! could drop pages. Inherited attributes and deleting pages follow `/Parent`, which can loop too.
//!
//! So every document is checked when it is loaded and repaired where that keeps what it shows: kids
//! that are not page or tree dictionaries are dropped, kids given directly get objects of their
//! own, kids that name their object through another reference, or by another generation number
//! (which PDFium ignores), name it directly, a node that refers back to one of its ancestors loses
//! that reference, a page listed twice becomes two pages, every node is typed by whether it has
//! `/Kids`, every `/Count` says how many pages are really below it, and every `/Parent` names the
//! node the page is listed under (with what the page inherited through the old `/Parent` copied
//! onto it first). A `/Rotate` that is not a whole number of quarter turns written as a plain
//! integer is replaced by the turn PDFium reads from it, and a page box with coordinates past
//! [`MAX_COORDINATE`] is brought within it, so that the file written afterwards says the same to
//! every reader. A tree nested deeper than [`MAX_DEPTH`], one that lists the same branch twice,
//! one that lists pages so many times that copying them would take too much memory, and one with
//! a page or node the file holds but that could not be read (lopdf leaves out objects it cannot
//! parse, where PDFium may still show them) are refused.
//!
//! Copies, of a page listed twice and of what a page inherited, share what is large in them with
//! the original instead of duplicating it (see [`share`]), so their cost does not grow with the
//! size of the page.

use std::collections::HashMap;
use std::collections::HashSet;

use lopdf::xref::XrefEntry;
use lopdf::{dictionary, Dictionary, Document, Object, ObjectId};

use crate::error::{Error, Result};
use crate::geometry::rotation_value;

/// Deeper page trees are refused. Real files are a handful of levels deep.
pub const MAX_DEPTH: usize = 128;

/// The largest page box coordinate, in either direction; larger ones are brought within it. PDF
/// itself limits pages to 14400 units (times a page's `/UserUnit`), so this leaves room for any
/// real page while keeping sizes finite and writable.
pub const MAX_COORDINATE: f32 = 1_000_000.0;

/// The most values the copies a repair makes (of pages listed more than once and of what pages
/// inherit) may hold in all, about 150 MB.
pub const MAX_COPIED_VALUES: usize = 1_000_000;

/// Attributes a page takes from the nodes above it when it does not set them itself.
pub(crate) const INHERITABLE: [&[u8]; 4] = [b"Resources", b"MediaBox", b"CropBox", b"Rotate"];

/// The boxes of a page, all read and written the same way.
const BOXES: [&[u8]; 5] = [b"MediaBox", b"CropBox", b"BleedBox", b"TrimBox", b"ArtBox"];

pub mod messages {
    pub const TOO_DEEP: &str = "This document's page tree is nested too deeply to be read safely.";
    pub const SHARED_BRANCH: &str =
        "This document's page tree lists the same group of pages in two places, so its page order cannot be read safely.";
    pub const TOO_MANY_COPIES: &str = "This document's page tree lists the same pages too many times to be read safely.";
    pub const UNREADABLE: &str =
        "Part of this document's page tree is damaged and could not be read, so pages would be lost if it were edited.";
}

/// Repairs the page tree of `doc` as the module documentation describes. A tree that needs no
/// repair is left exactly as it is.
pub(crate) fn repair(doc: &mut Document) -> Result<()> {
    let catalog_id = match doc.trailer.get(b"Root").and_then(Object::as_reference) {
        Ok(id) => target(doc, id)?,
        Err(_) => None,
    };
    let Some(catalog_id) = catalog_id.filter(|id| doc.get_dictionary(*id).is_ok()) else {
        return Err(Error::Unsupported(messages::UNREADABLE.into()));
    };
    let root = match doc.get_dictionary(catalog_id)?.get(b"Pages") {
        Ok(Object::Reference(id)) => {
            let id = *id;
            match target(doc, id)? {
                Some(found) if doc.get_dictionary(found).is_ok() => {
                    if found != id {
                        doc.get_dictionary_mut(catalog_id)?.set("Pages", found);
                    }
                    found
                }
                // No page tree: a document without pages, to every reader.
                _ => return Ok(()),
            }
        }
        // A root given directly: viewers read it, lopdf does not.
        Ok(Object::Dictionary(dict)) => {
            let id = doc.add_object(dict.clone());
            doc.get_dictionary_mut(catalog_id)?.set("Pages", id);
            id
        }
        _ => return Ok(()),
    };
    let root = if doc.get_dictionary(root)?.has(b"Kids") {
        root
    } else {
        // A root without /Kids is a document of one page, itself, to PDFium; give it a real root.
        let wrapper = doc.add_object(dictionary! { "Type" => "Pages", "Kids" => vec![Object::Reference(root)], "Count" => 1 });
        doc.get_dictionary_mut(catalog_id)?.set("Pages", wrapper);
        wrapper
    };
    let mut walk = Walk { placed: HashSet::from([root]), path: vec![root], copied: 0, holders: HashMap::new() };
    set_type(doc, root, b"Pages")?;
    if doc.get_dictionary(root)?.has(b"Parent") {
        walk.reparent(doc, root, None)?;
    }
    plain_rotation(doc, root)?;
    plain_boxes(doc, root)?;
    walk.node(doc, root)?;
    Ok(())
}

struct Walk {
    /// Every node and page already listed somewhere in the tree.
    placed: HashSet<ObjectId>,
    /// The nodes from the root down to the one being read.
    path: Vec<ObjectId>,
    /// How many values the copies made so far hold (see [`MAX_COPIED_VALUES`]).
    copied: usize,
    /// For each node a `/Parent` named, the node each inheritable attribute comes from above it.
    holders: HashMap<ObjectId, [Option<ObjectId>; 4]>,
}

impl Walk {
    /// Repairs the tree node `id` and everything below it, and returns how many pages it holds.
    fn node(&mut self, doc: &mut Document, id: ObjectId) -> Result<i64> {
        if self.path.len() > MAX_DEPTH {
            return Err(Error::Unsupported(messages::TOO_DEEP.into()));
        }
        let kids = match resolve(doc, doc.get_dictionary(id)?.get(b"Kids").ok()) {
            Some(Object::Array(kids)) => kids,
            _ => Vec::new(),
        };
        let mut listed = Vec::with_capacity(kids.len());
        let mut changed = false;
        let mut pages = 0i64;
        for kid in kids {
            let kid_id = match kid {
                Object::Reference(reference) => match target(doc, reference)? {
                    Some(found) if doc.get_dictionary(found).is_ok() => {
                        changed |= found != reference;
                        found
                    }
                    // Nothing a viewer could show: a number, a stream, an object that is not there.
                    _ => {
                        changed = true;
                        continue;
                    }
                },
                Object::Dictionary(dict) => {
                    changed = true;
                    doc.add_object(dict)
                }
                // A null, a number, a name...
                _ => {
                    changed = true;
                    continue;
                }
            };
            if self.path.contains(&kid_id) {
                // A loop back to an ancestor.
                changed = true;
                continue;
            }
            let is_node = doc.get_dictionary(kid_id)?.has(b"Kids");
            let kid_id = if self.placed.contains(&kid_id) {
                if is_node {
                    return Err(Error::Unsupported(messages::SHARED_BRANCH.into()));
                }
                // The same page listed twice: viewers show it twice, so it becomes two pages.
                changed = true;
                self.copy_page(doc, kid_id)?
            } else {
                kid_id
            };
            self.placed.insert(kid_id);
            set_type(doc, kid_id, if is_node { b"Pages" } else { b"Page" })?;
            if doc.get_dictionary(kid_id)?.get(b"Parent").ok() != Some(&Object::Reference(id)) {
                self.reparent(doc, kid_id, Some(id))?;
            }
            plain_rotation(doc, kid_id)?;
            plain_boxes(doc, kid_id)?;
            listed.push(Object::Reference(kid_id));
            pages += if is_node {
                self.path.push(kid_id);
                let below = self.node(doc, kid_id)?;
                self.path.pop();
                below
            } else {
                1
            };
        }
        let dict = doc.get_dictionary_mut(id)?;
        if changed {
            dict.set("Kids", listed);
        }
        if dict.get(b"Count").ok() != Some(&Object::Integer(pages)) {
            dict.set("Count", pages);
        }
        Ok(pages)
    }

    /// A new page object showing what page `id` shows, for another place in the tree. Its values
    /// are shared with the original, not copied (see [`share`]).
    fn copy_page(&mut self, doc: &mut Document, id: ObjectId) -> Result<ObjectId> {
        let keys: Vec<Vec<u8>> = doc.get_dictionary(id)?.iter().map(|(key, _)| key.clone()).collect();
        self.count_copies(keys.len() + 1)?;
        let mut copy = Dictionary::new();
        for key in keys {
            let value = share(doc, id, &key)?;
            copy.set(key, value);
        }
        Ok(doc.add_object(copy))
    }

    fn count_copies(&mut self, values: usize) -> Result<()> {
        self.copied += values;
        if self.copied > MAX_COPIED_VALUES {
            return Err(Error::Unsupported(messages::TOO_MANY_COPIES.into()));
        }
        Ok(())
    }

    /// Points `/Parent` of `id` at `parent` (removes it for the root). What the node inherited
    /// through its old `/Parent` is given to it first, so it shows exactly what it showed before.
    fn reparent(&mut self, doc: &mut Document, id: ObjectId, parent: Option<ObjectId>) -> Result<()> {
        let dict = doc.get_dictionary(id)?;
        let missing: Vec<usize> = (0..INHERITABLE.len()).filter(|&i| !dict.has(INHERITABLE[i])).collect();
        if let (false, Ok(old)) = (missing.is_empty(), dict.get(b"Parent").and_then(Object::as_reference)) {
            let holders = self.holders_above(doc, old);
            for i in missing {
                if let Some(holder) = holders[i] {
                    self.count_copies(1)?;
                    let value = share(doc, holder, INHERITABLE[i])?;
                    doc.get_dictionary_mut(id)?.set(INHERITABLE[i], value);
                }
            }
        }
        let dict = doc.get_dictionary_mut(id)?;
        match parent {
            Some(parent) => dict.set("Parent", parent),
            None => {
                dict.remove(b"Parent");
            }
        }
        Ok(())
    }

    /// The node each of [`INHERITABLE`] is taken from by something whose `/Parent` is `first`: the
    /// first node of the `/Parent` chain from `first` on that holds it. The chain may loop, and is
    /// followed no further than a tree can be deep. Worked out once for each `first`, so that
    /// pages naming one long chain cost no more than the chain itself.
    fn holders_above(&mut self, doc: &Document, first: ObjectId) -> [Option<ObjectId>; 4] {
        if let Some(found) = self.holders.get(&first) {
            return *found;
        }
        let mut found = [None; 4];
        let mut seen = HashSet::new();
        let mut next = Some(first);
        while let Some(id) = next.filter(|id| seen.len() <= MAX_DEPTH && seen.insert(*id)) {
            let Ok(node) = doc.get_dictionary(id) else { break };
            for (slot, key) in found.iter_mut().zip(INHERITABLE) {
                if slot.is_none() && node.has(key) {
                    *slot = Some(id);
                }
            }
            next = node.get(b"Parent").and_then(Object::as_reference).ok();
        }
        self.holders.insert(first, found);
        found
    }
}

/// The object a reference to `id` leads to the way PDFium follows it: by object number, whatever
/// the generation number says, and through objects that are themselves references. `None` when
/// there is no such object; an error when the file lists it but it could not be read.
fn target(doc: &Document, id: ObjectId) -> Result<Option<ObjectId>> {
    let mut current = id;
    for _ in 0..16 {
        let found = match doc.objects.get(&current) {
            Some(_) => Some(current),
            None => doc.objects.range((current.0, 0)..=(current.0, u16::MAX)).next().map(|(id, _)| *id),
        };
        match found.map(|id| (id, &doc.objects[&id])) {
            Some((_, Object::Reference(next))) => current = *next,
            Some((id, _)) => return Ok(Some(id)),
            None if matches!(
                doc.reference_table.entries.get(&current.0),
                Some(XrefEntry::Normal { .. } | XrefEntry::Compressed { .. })
            ) =>
            {
                return Err(Error::Unsupported(messages::UNREADABLE.into()))
            }
            None => return Ok(None),
        }
    }
    Ok(None)
}

/// The value of `key` in the dictionary `holder`, for another dictionary to hold as well. A small
/// one is copied; a larger direct array or dictionary is first moved into an object of its own
/// that both then refer to, and a reference to a larger object stays a reference, so that what a
/// copy costs does not depend on how large the value is.
pub(crate) fn share(doc: &mut Document, holder: ObjectId, key: &[u8]) -> Result<Object> {
    let value = doc.get_dictionary(holder)?.get(key)?;
    match value {
        Object::Reference(id) => match doc.get_object(*id) {
            Ok(target) if is_small(target) => Ok(target.clone()),
            _ => Ok(value.clone()),
        },
        value if is_small(value) => Ok(value.clone()),
        _ => {
            let value = doc.get_dictionary_mut(holder)?.remove(key).unwrap_or(Object::Null);
            let id = doc.add_object(value);
            doc.get_dictionary_mut(holder)?.set(key.to_vec(), id);
            Ok(Object::Reference(id))
        }
    }
}

/// True for an object of a few values at most, cheap to copy.
fn is_small(object: &Object) -> bool {
    fn values(object: &Object, left: &mut usize) -> bool {
        if *left == 0 {
            return false;
        }
        *left -= 1;
        match object {
            Object::Array(items) => items.iter().all(|item| values(item, left)),
            Object::Dictionary(dict) => dict.iter().all(|(_, value)| values(value, left)),
            Object::Stream(_) => false,
            Object::String(bytes, _) | Object::Name(bytes) => bytes.len() <= 256,
            _ => true,
        }
    }
    values(object, &mut 16)
}

/// Follows references (a bounded number of them) to a direct object.
fn resolve(doc: &Document, object: Option<&Object>) -> Option<Object> {
    let mut current = object?.clone();
    for _ in 0..16 {
        match current {
            Object::Reference(id) => current = doc.get_object(id).ok()?.clone(),
            other => return Some(other),
        }
    }
    None
}

fn set_type(doc: &mut Document, id: ObjectId, kind: &[u8]) -> Result<()> {
    let dict = doc.get_dictionary_mut(id)?;
    if dict.get(b"Type").and_then(Object::as_name).ok() != Some(kind) {
        dict.set("Type", Object::Name(kind.to_vec()));
    }
    Ok(())
}

/// Rewrites `/Rotate` of `id` as the plain integer PDFium reads from it, unless it already is a
/// whole number of quarter turns that every reader takes the same way.
fn plain_rotation(doc: &mut Document, id: ObjectId) -> Result<()> {
    let Ok(value) = doc.get_dictionary(id)?.get(b"Rotate") else { return Ok(()) };
    if matches!(value, Object::Integer(v) if v % 90 == 0 && i32::try_from(*v).is_ok()) {
        return Ok(());
    }
    let turn = resolve(doc, Some(value)).map_or(0, |value| rotation_value(&value));
    doc.get_dictionary_mut(id)?.set("Rotate", turn as i64);
    Ok(())
}

/// Brings the coordinates of the boxes of `id` within [`MAX_COORDINATE`]. Only a box with a
/// coordinate past it is rewritten, as the numbers PDFium reads from it, brought within the limit.
fn plain_boxes(doc: &mut Document, id: ObjectId) -> Result<()> {
    for key in BOXES {
        let Ok(value) = doc.get_dictionary(id)?.get(key) else { continue };
        let Some(Object::Array(items)) = resolve(doc, Some(value)) else { continue };
        let numbers: Vec<Option<f32>> = items.iter().map(|item| resolve(doc, Some(item)).and_then(|v| v.as_float().ok())).collect();
        if numbers.iter().all(|n| n.is_none_or(|n| n.abs() <= MAX_COORDINATE)) {
            continue;
        }
        let clamped: Vec<Object> = numbers
            .into_iter()
            .map(|n| match n {
                Some(n) if n.is_finite() => Object::Real(n.clamp(-MAX_COORDINATE, MAX_COORDINATE)),
                Some(n) if n.is_nan() => Object::Integer(0),
                Some(n) => Object::Real(MAX_COORDINATE.copysign(n)),
                None => Object::Integer(0),
            })
            .collect();
        doc.get_dictionary_mut(id)?.set(key, clamped);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A catalog whose page tree root is `root`, holding `objects`.
    fn document(objects: Vec<(u32, Object)>) -> Document {
        let mut doc = Document::with_version("1.7");
        for (number, object) in objects {
            doc.objects.insert((number, 0), object);
            doc.max_id = doc.max_id.max(number);
        }
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => (1, 0) });
        doc.trailer.set("Root", catalog);
        doc
    }

    fn page(parent: u32) -> Object {
        Object::Dictionary(dictionary! { "Type" => "Page", "Parent" => (parent, 0), "MediaBox" => vec![0.into(), 0.into(), 100.into(), 100.into()] })
    }

    fn pages(doc: &Document) -> Vec<ObjectId> {
        doc.get_pages().into_values().collect()
    }

    #[test]
    fn a_sound_tree_is_left_exactly_as_it_is() {
        let mut doc = document(vec![
            (1, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => vec![(2, 0).into(), (3, 0).into()], "Count" => 2 })),
            (2, page(1)),
            (3, Object::Dictionary(dictionary! { "Type" => "Pages", "Parent" => (1, 0), "Kids" => vec![(4, 0).into()], "Count" => 1 })),
            (4, page(3)),
        ]);
        let before = doc.objects.clone();
        repair(&mut doc).unwrap();
        assert_eq!(doc.objects, before);
    }

    #[test]
    fn loops_untyped_nodes_and_wrong_counts_are_repaired() {
        let mut doc = document(vec![
            // The root claims a million pages, lists itself, and has a /Parent of its own.
            (1, Object::Dictionary(dictionary! {
                "Type" => "Pages", "Parent" => (1, 0), "Count" => 1_000_000,
                "Kids" => vec![(2, 0).into(), (1, 0).into(), (3, 0).into(), Object::Null, (9, 0).into()],
            })),
            // A page without /Type, and an untyped node whose kid loops back to the root.
            (2, Object::Dictionary(dictionary! { "Parent" => (1, 0), "Rotate" => 90 })),
            (3, Object::Dictionary(dictionary! { "Parent" => (3, 0), "Kids" => vec![(4, 0).into(), (1, 0).into(), (4, 0).into()], "Count" => 7 })),
            (4, page(3)),
        ]);
        repair(&mut doc).unwrap();
        let listed = pages(&doc);
        assert_eq!(listed.len(), 3, "the untyped page and page 4 twice: {listed:?}");
        assert_eq!(listed[..2], [(2, 0), (4, 0)]);
        assert_ne!(listed[2], (4, 0), "the second listing is a page of its own");
        let root = doc.get_dictionary((1, 0)).unwrap();
        assert_eq!(root.get(b"Count").unwrap(), &Object::Integer(3));
        assert!(!root.has(b"Parent"));
        let node = doc.get_dictionary((3, 0)).unwrap();
        assert_eq!((node.get_type().unwrap(), node.get(b"Count").unwrap()), (&b"Pages"[..], &Object::Integer(2)));
        assert_eq!(node.get(b"Parent").unwrap(), &Object::Reference((1, 0)));
        assert_eq!(doc.get_dictionary(listed[2]).unwrap().get(b"Parent").unwrap(), &Object::Reference((3, 0)));
    }

    #[test]
    fn a_page_keeps_what_it_inherited_when_its_parent_is_corrected() {
        let mut doc = document(vec![
            (1, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => vec![(2, 0).into()], "Count" => 1 })),
            // Listed under the root, but its /Parent names a node that gives it a rotation.
            (2, Object::Dictionary(dictionary! { "Type" => "Page", "Parent" => (3, 0) })),
            (3, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => Vec::<Object>::new(), "Count" => 0, "Rotate" => 270, "Parent" => (3, 0) })),
        ]);
        repair(&mut doc).unwrap();
        let page = doc.get_dictionary((2, 0)).unwrap();
        assert_eq!(page.get(b"Parent").unwrap(), &Object::Reference((1, 0)));
        assert_eq!(page.get(b"Rotate").unwrap(), &Object::Integer(270));
    }

    #[test]
    fn rotations_are_rewritten_as_the_turn_viewers_read() {
        let turned = |rotate: Object| Object::Dictionary(dictionary! { "Type" => "Page", "Parent" => (1, 0), "Rotate" => rotate });
        let mut doc = document(vec![
            (1, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => vec![(2, 0).into(), (3, 0).into(), (4, 0).into(), (5, 0).into()], "Count" => 4, "Rotate" => Object::Real(180.0) })),
            (2, turned(Object::Integer(-90))),
            (3, turned(Object::Real(90.5))),
            (4, turned(Object::Integer(i64::MAX))),
            (5, turned(Object::Integer(45))),
        ]);
        repair(&mut doc).unwrap();
        let rotate = |id: u32| doc.get_dictionary((id, 0)).unwrap().get(b"Rotate").unwrap().clone();
        // A plain quarter turn is left as written; the rest become the turn PDFium reads.
        let expected = [(1, 180), (2, -90), (3, 90), (4, 0), (5, 0)];
        for (id, turn) in expected {
            assert_eq!(rotate(id), Object::Integer(turn), "object {id}");
        }
    }

    #[test]
    fn deep_trees_and_shared_branches_are_refused() {
        let mut objects = vec![];
        for n in 1..=(MAX_DEPTH as u32 + 2) {
            objects.push((n, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => vec![(n + 1, 0).into()], "Count" => 1 })));
        }
        objects.push((MAX_DEPTH as u32 + 3, page(MAX_DEPTH as u32 + 2)));
        assert_eq!(repair(&mut document(objects)).unwrap_err().to_string(), messages::TOO_DEEP);

        let mut doc = document(vec![
            (1, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => vec![(2, 0).into(), (2, 0).into()], "Count" => 2 })),
            (2, Object::Dictionary(dictionary! { "Type" => "Pages", "Parent" => (1, 0), "Kids" => vec![(3, 0).into()], "Count" => 1 })),
            (3, page(2)),
        ]);
        assert_eq!(repair(&mut doc).unwrap_err().to_string(), messages::SHARED_BRANCH);
    }

    #[test]
    fn a_root_without_kids_becomes_the_only_page() {
        let mut doc = document(vec![(1, Object::Dictionary(dictionary! { "Type" => "Pages", "MediaBox" => vec![0.into(), 0.into(), 10.into(), 10.into()] }))]);
        repair(&mut doc).unwrap();
        assert_eq!(pages(&doc), vec![(1, 0)]);
        assert_eq!(doc.get_dictionary((1, 0)).unwrap().get_type().unwrap(), b"Page");
    }
}
