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
//! own, a node that refers back to one of its ancestors loses that reference, a page listed twice
//! becomes two pages, every node is typed by whether it has `/Kids`, every `/Count` says how many
//! pages are really below it, and every `/Parent` names the node the page is listed under (with
//! what the page inherited through the old `/Parent` copied onto it first). A tree nested deeper
//! than [`MAX_DEPTH`], or one that lists the same branch twice, is refused.

use std::collections::HashSet;

use lopdf::{dictionary, Dictionary, Document, Object, ObjectId};

use crate::error::{Error, Result};

/// Deeper page trees are refused. Real files are a handful of levels deep.
pub const MAX_DEPTH: usize = 128;

/// Attributes a page takes from the nodes above it when it does not set them itself.
pub(crate) const INHERITABLE: [&[u8]; 4] = [b"Resources", b"MediaBox", b"CropBox", b"Rotate"];

pub mod messages {
    pub const TOO_DEEP: &str = "This document's page tree is nested too deeply to be read safely.";
    pub const SHARED_BRANCH: &str =
        "This document's page tree lists the same group of pages in two places, so its page order cannot be read safely.";
}

/// Repairs the page tree of `doc` as the module documentation describes. A tree that needs no
/// repair is left exactly as it is.
pub(crate) fn repair(doc: &mut Document) -> Result<()> {
    let Ok(catalog_id) = doc.trailer.get(b"Root").and_then(Object::as_reference) else { return Ok(()) };
    let root = match doc.get_dictionary(catalog_id).and_then(|c| c.get(b"Pages")) {
        Ok(Object::Reference(id)) => *id,
        // A root given directly: viewers read it, lopdf does not.
        Ok(Object::Dictionary(dict)) => {
            let id = doc.add_object(dict.clone());
            doc.get_dictionary_mut(catalog_id)?.set("Pages", id);
            id
        }
        _ => return Ok(()),
    };
    let Ok(dict) = doc.get_dictionary(root) else { return Ok(()) };
    let root = if dict.has(b"Kids") {
        root
    } else {
        // A root without /Kids is a document of one page, itself, to PDFium; give it a real root.
        let wrapper = doc.add_object(dictionary! { "Type" => "Pages", "Kids" => vec![Object::Reference(root)], "Count" => 1 });
        doc.get_dictionary_mut(catalog_id)?.set("Pages", wrapper);
        wrapper
    };
    set_type(doc, root, b"Pages")?;
    if doc.get_dictionary(root)?.has(b"Parent") {
        reparent(doc, root, None)?;
    }
    let mut walk = Walk { placed: HashSet::from([root]), path: vec![root] };
    walk.node(doc, root)?;
    Ok(())
}

struct Walk {
    /// Every node and page already listed somewhere in the tree.
    placed: HashSet<ObjectId>,
    /// The nodes from the root down to the one being read.
    path: Vec<ObjectId>,
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
                Object::Reference(kid_id) if doc.get_dictionary(kid_id).is_ok() => kid_id,
                Object::Dictionary(dict) => {
                    changed = true;
                    doc.add_object(dict)
                }
                // Nothing a viewer could show: a null, a number, a stream, a missing object.
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
                let copy = doc.get_object(kid_id)?.clone();
                doc.add_object(copy)
            } else {
                kid_id
            };
            self.placed.insert(kid_id);
            set_type(doc, kid_id, if is_node { b"Pages" } else { b"Page" })?;
            if doc.get_dictionary(kid_id)?.get(b"Parent").ok() != Some(&Object::Reference(id)) {
                reparent(doc, kid_id, Some(id))?;
            }
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

/// Points `/Parent` of `id` at `parent` (removes it for the root). What the node inherited through
/// its old `/Parent` is copied onto it first, so it shows exactly what it showed before.
fn reparent(doc: &mut Document, id: ObjectId, parent: Option<ObjectId>) -> Result<()> {
    let inherited: Vec<(&[u8], Object)> = {
        let dict = doc.get_dictionary(id)?;
        INHERITABLE
            .into_iter()
            .filter(|key| !dict.has(key))
            .filter_map(|key| Some((key, inherited_through_parent(doc, dict, key)?)))
            .collect()
    };
    let dict = doc.get_dictionary_mut(id)?;
    for (key, value) in inherited {
        dict.set(key, value);
    }
    match parent {
        Some(parent) => dict.set("Parent", parent),
        None => {
            dict.remove(b"Parent");
        }
    }
    Ok(())
}

/// `key` as `dict` inherits it through its `/Parent` chain, which may loop.
fn inherited_through_parent(doc: &Document, dict: &Dictionary, key: &[u8]) -> Option<Object> {
    let mut seen = HashSet::new();
    let mut next = dict.get(b"Parent").and_then(Object::as_reference).ok();
    while let Some(id) = next.filter(|id| seen.insert(*id)) {
        let node = doc.get_dictionary(id).ok()?;
        if let Ok(value) = node.get(key) {
            return resolve(doc, Some(value));
        }
        next = node.get(b"Parent").and_then(Object::as_reference).ok();
    }
    None
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
