//! What leads to pages from outside the page tree: the outline (bookmarks), named destinations,
//! the action run when the document opens, links, article threads and page labels.
//!
//! Deleting pages takes out the entries that lead only to them, so their titles and names do not
//! stay in the document, and gives the pages that stay the labels they had.

use std::collections::{HashMap, HashSet};

use lopdf::{Dictionary, Document, Object, ObjectId, StringFormat};

use crate::pdf::Pdf;

/// Deeper nesting than this is treated as damaged and left alone.
const MAX_DEPTH: usize = 64;
/// Longer chains of outline items or article beads than this are treated as damaged.
const MAX_CHAIN: usize = 100_000;

/// How one page is labelled: the numbering style, the prefix and the number.
type Label = (Option<Vec<u8>>, Option<Vec<u8>>, i64);

impl Pdf {
    /// Takes out what leads only to the pages `doomed`, which are about to be deleted: outline
    /// items (one that still has items below it stays, without its destination), named
    /// destinations, the open action, the destinations of links on other pages, and article beads.
    /// Destinations are matched to pages through their references, so this runs while the pages
    /// are still in the document.
    pub(crate) fn drop_navigation_to(&mut self, doomed: &HashSet<ObjectId>) {
        let named = self.named_destinations();
        let leads_away = |pdf: &Pdf, target: Option<&Object>| {
            target.and_then(|t| pdf.destination_page(t, &named, 0)).is_some_and(|page| doomed.contains(&page))
        };

        if let Ok(root) = self.doc.catalog().and_then(|c| c.get(b"Outlines")).and_then(Object::as_reference) {
            let mut visited = HashSet::new();
            self.prune_outline(root, true, &|pdf, item| leads_away(pdf, item_target(item)), &mut visited, 0);
        }

        let gone: HashSet<Vec<u8>> =
            named.iter().filter(|(_, target)| leads_away(self, Some(target))).map(|(name, _)| name.clone()).collect();
        if !gone.is_empty() {
            self.drop_named_destinations(&gone);
        }

        let open_action = self.doc.catalog().ok().and_then(|c| c.get(b"OpenAction").ok().cloned());
        if leads_away(self, open_action.as_ref()) {
            if let Ok(catalog) = self.doc.catalog_mut() {
                catalog.remove(b"OpenAction");
            }
        }

        // Links on the pages that stay lose a destination that is going.
        let staying: Vec<ObjectId> = self.page_ids().into_iter().filter(|page| !doomed.contains(page)).collect();
        for page in staying {
            self.clear_links_to(page, &|pdf, annotation| {
                annotation.get(b"Subtype").and_then(Object::as_name).is_ok_and(|s| s == b"Link") && leads_away(pdf, item_target(annotation))
            });
        }

        self.prune_threads(doomed);
    }

    /// The page a destination, a named destination or a go-to action leads to in this document.
    fn destination_page(&self, target: &Object, named: &HashMap<Vec<u8>, Object>, depth: usize) -> Option<ObjectId> {
        if depth > 8 {
            return None;
        }
        match self.resolve(target) {
            Object::Array(items) => items.first()?.as_reference().ok(),
            // A go-to action, or a named destination in its dictionary form.
            Object::Dictionary(dict) => {
                if dict.get(b"S").and_then(Object::as_name).is_ok_and(|s| s != b"GoTo") {
                    return None;
                }
                self.destination_page(dict.get(b"D").ok()?, named, depth + 1)
            }
            Object::Name(name) | Object::String(name, _) => self.destination_page(named.get(&name)?, named, depth + 1),
            _ => None,
        }
    }

    /// Every named destination: the catalog's /Dests dictionary and the /Dests name tree.
    fn named_destinations(&self) -> HashMap<Vec<u8>, Object> {
        let mut found = HashMap::new();
        let Ok(catalog) = self.doc.catalog() else { return found };
        if let Ok(Object::Dictionary(dests)) = catalog.get(b"Dests").map(|d| self.resolve(d)) {
            found.extend(dests.iter().map(|(name, target)| (name.clone(), target.clone())));
        }
        if let Some(tree) = self.name_tree(b"Dests") {
            let mut pairs = Vec::new();
            collect_pairs(&self.doc, &tree, b"Names", &mut pairs);
            for (name, target) in pairs {
                if let Object::String(name, _) = name {
                    found.insert(name, target);
                }
            }
        }
        found
    }

    /// The root of the catalog's /Names entry `key` (`Dests`, `EmbeddedFiles`, `JavaScript`...).
    pub(crate) fn name_tree(&self, key: &[u8]) -> Option<Object> {
        let names = self.resolve(self.doc.catalog().ok()?.get(b"Names").ok()?);
        Some(names.as_dict().ok()?.get(key).ok()?.clone())
    }

    /// Removes the named destinations called `gone` from the catalog's /Dests and the name tree.
    fn drop_named_destinations(&mut self, gone: &HashSet<Vec<u8>>) {
        let Ok(catalog_id) = self.doc.trailer.get(b"Root").and_then(Object::as_reference) else { return };
        if let Ok(dests) = self.doc.get_dictionary(catalog_id).and_then(|c| c.get(b"Dests")).cloned() {
            edit_dictionary(&mut self.doc, catalog_id, b"Dests", dests, |dict| {
                let names: Vec<Vec<u8>> = dict.iter().map(|(k, _)| k.clone()).filter(|k| gone.contains(k)).collect();
                for name in names {
                    dict.remove(&name);
                }
            });
        }
        if let Some(mut tree) = self.name_tree(b"Dests") {
            let goes = |name: &Object, _: &Object| matches!(name, Object::String(n, _) if gone.contains(n));
            let held = prune_pairs(&mut self.doc, &mut tree, b"Names", &goes, &mut HashSet::new(), 0);
            let names_entry = self.doc.get_dictionary(catalog_id).and_then(|c| c.get(b"Names")).cloned();
            if let Ok(names) = names_entry {
                edit_dictionary(&mut self.doc, catalog_id, b"Names", names, |dict| match held {
                    // A tree left without a name goes altogether.
                    Held::Nothing => {
                        dict.remove(b"Dests");
                    }
                    _ => dict.set("Dests", tree),
                });
            }
        }
    }

    /// Prunes the outline items below `parent` that `goes` gives up, relinks the ones that stay and
    /// returns how many items show below `parent` when it is open. An item that goes but still has
    /// items below it stays, without its destination.
    fn prune_outline(
        &mut self,
        parent: ObjectId,
        is_root: bool,
        goes: &dyn Fn(&Pdf, &Dictionary) -> bool,
        visited: &mut HashSet<ObjectId>,
        depth: usize,
    ) -> i64 {
        let Ok(dict) = self.doc.get_dictionary(parent) else { return 0 };
        if depth > MAX_DEPTH {
            return dict.get(b"Count").and_then(Object::as_i64).map(i64::abs).unwrap_or(0);
        }
        let mut children = Vec::new();
        let mut next = dict.get(b"First").and_then(Object::as_reference).ok();
        while let Some(id) = next {
            if !visited.insert(id) || children.len() >= MAX_CHAIN {
                break;
            }
            children.push(id);
            next = self.doc.get_dictionary(id).and_then(|d| d.get(b"Next")).and_then(Object::as_reference).ok();
        }

        let mut kept = Vec::new();
        let mut shown = 0;
        for child in children {
            let below = self.prune_outline(child, false, goes, visited, depth + 1);
            let Ok(item) = self.doc.get_dictionary(child) else { continue };
            let has_children = item.has(b"First");
            let open = item.get(b"Count").and_then(Object::as_i64).is_ok_and(|c| c > 0);
            if goes(self, item) {
                if !has_children {
                    continue;
                }
                if let Ok(item) = self.doc.get_dictionary_mut(child) {
                    item.remove(b"Dest");
                    item.remove(b"A");
                }
            }
            shown += 1 + if open { below } else { 0 };
            kept.push(child);
        }

        for (i, &id) in kept.iter().enumerate() {
            let Ok(item) = self.doc.get_dictionary_mut(id) else { continue };
            item.set("Parent", parent);
            match i.checked_sub(1).map(|p| kept[p]) {
                Some(previous) => item.set("Prev", previous),
                None => {
                    item.remove(b"Prev");
                }
            }
            match kept.get(i + 1) {
                Some(&following) => item.set("Next", following),
                None => {
                    item.remove(b"Next");
                }
            }
        }
        let Ok(dict) = self.doc.get_dictionary_mut(parent) else { return shown };
        match (kept.first(), kept.last()) {
            (Some(&first), Some(&last)) => {
                dict.set("First", first);
                dict.set("Last", last);
            }
            _ => {
                dict.remove(b"First");
                dict.remove(b"Last");
            }
        }
        // An open item counts the items it shows; a closed one, negated, those it would show.
        let closed = !is_root && dict.get(b"Count").and_then(Object::as_i64).is_ok_and(|c| c < 0);
        if shown == 0 && !is_root {
            dict.remove(b"Count");
        } else {
            dict.set("Count", if closed { -shown } else { shown });
        }
        shown
    }

    /// Removes the destination of every link on `page` that `goes` gives up.
    fn clear_links_to(&mut self, page: ObjectId, goes: &dyn Fn(&Pdf, &Dictionary) -> bool) {
        let annotations = match self.doc.get_dictionary(page).and_then(|d| d.get(b"Annots")).map(|a| self.resolve(a)) {
            Ok(Object::Array(items)) => items,
            _ => return,
        };
        let mut direct = Vec::new();
        for (i, annotation) in annotations.iter().enumerate() {
            match annotation {
                Object::Reference(id) => {
                    let Ok(dict) = self.doc.get_dictionary(*id) else { continue };
                    if goes(self, dict) {
                        if let Ok(dict) = self.doc.get_dictionary_mut(*id) {
                            dict.remove(b"Dest");
                            dict.remove(b"A");
                        }
                    }
                }
                Object::Dictionary(dict) if goes(self, dict) => direct.push(i),
                _ => {}
            }
        }
        if direct.is_empty() {
            return;
        }
        let mut annotations = annotations;
        for i in direct {
            if let Object::Dictionary(dict) = &mut annotations[i] {
                dict.remove(b"Dest");
                dict.remove(b"A");
            }
        }
        if let Ok(Object::Reference(id)) = self.doc.get_dictionary(page).and_then(|d| d.get(b"Annots")).cloned() {
            self.doc.objects.insert(id, Object::Array(annotations));
        } else if let Ok(dict) = self.doc.get_dictionary_mut(page) {
            dict.set("Annots", annotations);
        }
    }

    /// Takes the article beads on `doomed` pages out of their threads, and threads left without a
    /// bead out of the document.
    fn prune_threads(&mut self, doomed: &HashSet<ObjectId>) {
        let Ok(catalog_id) = self.doc.trailer.get(b"Root").and_then(Object::as_reference) else { return };
        let Ok(threads) = self.doc.get_dictionary(catalog_id).and_then(|c| c.get(b"Threads")).cloned() else { return };
        let Object::Array(list) = self.resolve(&threads) else { return };
        let mut kept_threads = Vec::new();
        let mut changed = false;
        for thread in list {
            let Ok(thread_id) = thread.as_reference() else {
                kept_threads.push(thread);
                continue;
            };
            let mut beads = Vec::new();
            let mut next = self.doc.get_dictionary(thread_id).and_then(|t| t.get(b"F")).and_then(Object::as_reference).ok();
            while let Some(bead) = next {
                if beads.contains(&bead) || beads.len() >= MAX_CHAIN {
                    break;
                }
                beads.push(bead);
                next = self.doc.get_dictionary(bead).and_then(|b| b.get(b"N")).and_then(Object::as_reference).ok();
            }
            let on_doomed =
                |pdf: &Pdf, bead: ObjectId| pdf.doc.get_dictionary(bead).and_then(|b| b.get(b"P")).and_then(Object::as_reference).is_ok_and(|p| doomed.contains(&p));
            if !beads.iter().any(|&b| on_doomed(self, b)) {
                kept_threads.push(thread);
                continue;
            }
            changed = true;
            let kept: Vec<ObjectId> = beads.iter().copied().filter(|&b| !on_doomed(self, b)).collect();
            if kept.is_empty() {
                continue;
            }
            for (i, &bead) in kept.iter().enumerate() {
                if let Ok(dict) = self.doc.get_dictionary_mut(bead) {
                    dict.set("N", kept[(i + 1) % kept.len()]);
                    dict.set("V", kept[(i + kept.len() - 1) % kept.len()]);
                }
            }
            if let Ok(dict) = self.doc.get_dictionary_mut(thread_id) {
                dict.set("F", kept[0]);
            }
            kept_threads.push(thread);
        }
        if !changed {
            return;
        }
        match (threads, kept_threads.is_empty()) {
            (_, true) => {
                if let Ok(catalog) = self.doc.get_dictionary_mut(catalog_id) {
                    catalog.remove(b"Threads");
                }
            }
            (Object::Reference(id), false) => {
                self.doc.objects.insert(id, Object::Array(kept_threads));
            }
            (_, false) => {
                if let Ok(catalog) = self.doc.get_dictionary_mut(catalog_id) {
                    catalog.set("Threads", kept_threads);
                }
            }
        }
    }

    /// The page labels the pages left after deleting `deleted` (sorted, zero-based) should have:
    /// the labels they have now, as a new /PageLabels number tree. `None` when the document has no
    /// page labels.
    pub(crate) fn page_labels_without(&self, deleted: &[usize]) -> Option<Object> {
        let tree = self.doc.catalog().ok()?.get(b"PageLabels").ok()?.clone();
        let mut pairs = Vec::new();
        collect_pairs(&self.doc, &tree, b"Nums", &mut pairs);
        let mut ranges: Vec<(i64, Dictionary)> = pairs
            .into_iter()
            .filter_map(|(start, style)| Some((start.as_i64().ok()?, self.resolve(&style).as_dict().ok()?.clone())))
            .collect();
        if ranges.is_empty() {
            // Nothing readable to carry over: the tree is left as it is.
            return None;
        }
        ranges.sort_by_key(|(start, _)| *start);
        let label = |index: usize| -> Label {
            let index = index as i64;
            match ranges.iter().rev().find(|(start, _)| *start <= index) {
                Some((start, style)) => {
                    let name = style.get(b"S").ok().and_then(|s| self.resolve(s).as_name().ok().map(<[u8]>::to_vec));
                    let prefix = style.get(b"P").ok().and_then(|p| self.resolve(p).as_str().ok().map(<[u8]>::to_vec));
                    let first = style.get(b"St").ok().and_then(|n| self.resolve(n).as_i64().ok()).unwrap_or(1);
                    (name, prefix, first + index - start)
                }
                None => (None, None, index + 1),
            }
        };

        let mut nums = Vec::new();
        let mut previous: Option<Label> = None;
        let kept = (0..self.page_count()).filter(|page| deleted.binary_search(page).is_err());
        for (new_index, old_index) in kept.enumerate() {
            let (name, prefix, number) = label(old_index);
            let continues = previous
                .as_ref()
                .is_some_and(|(n, p, last)| *n == name && *p == prefix && (name.is_none() || *last + 1 == number));
            if !continues {
                let mut style = Dictionary::new();
                if let Some(name) = &name {
                    style.set("S", Object::Name(name.clone()));
                    if number != 1 {
                        style.set("St", number);
                    }
                }
                if let Some(prefix) = &prefix {
                    style.set("P", Object::String(prefix.clone(), StringFormat::Literal));
                }
                nums.push(Object::Integer(new_index as i64));
                nums.push(Object::Dictionary(style));
            }
            previous = Some((name, prefix, number));
        }
        let mut labels = Dictionary::new();
        labels.set("Nums", nums);
        Some(Object::Dictionary(labels))
    }
}

/// What an outline item or a link leads to: its destination, or else its action.
fn item_target(item: &Dictionary) -> Option<&Object> {
    item.get(b"Dest").ok().or_else(|| item.get(b"A").ok())
}

/// Runs `edit` on the dictionary `entry` (the value of `key` in the dictionary `owner`) is or
/// refers to, and stores it where it came from.
fn edit_dictionary(doc: &mut Document, owner: ObjectId, key: &[u8], entry: Object, edit: impl FnOnce(&mut Dictionary)) {
    match entry {
        Object::Reference(id) => {
            if let Ok(dict) = doc.get_dictionary_mut(id) {
                edit(dict);
            }
        }
        Object::Dictionary(mut dict) => {
            edit(&mut dict);
            if let Ok(owner) = doc.get_dictionary_mut(owner) {
                owner.set(key.to_vec(), dict);
            }
        }
        _ => {}
    }
}

/// The key and value pairs of a name tree (`key` = `Names`) or number tree (`Nums`), values as
/// stored. A node listed again (the tree loops, or lists a node twice) is read only once.
pub(crate) fn collect_pairs(doc: &Document, node: &Object, key: &[u8], out: &mut Vec<(Object, Object)>) {
    collect_pairs_below(doc, node, key, out, &mut HashSet::new(), 0);
}

fn collect_pairs_below(
    doc: &Document,
    node: &Object,
    key: &[u8],
    out: &mut Vec<(Object, Object)>,
    seen: &mut HashSet<ObjectId>,
    depth: usize,
) {
    if depth > MAX_DEPTH {
        return;
    }
    let dict = match node {
        Object::Reference(id) if !seen.insert(*id) => return,
        Object::Reference(id) => match doc.get_dictionary(*id) {
            Ok(dict) => dict,
            Err(_) => return,
        },
        Object::Dictionary(dict) => dict,
        _ => return,
    };
    let resolved = |object: &Object| match object {
        Object::Reference(id) => doc.get_object(*id).cloned().unwrap_or(Object::Null),
        other => other.clone(),
    };
    if let Object::Array(pairs) = dict.get(key).map(resolved).unwrap_or(Object::Null) {
        for pair in pairs.chunks(2) {
            if let [name, value] = pair {
                out.push((resolved(name), value.clone()));
            }
        }
    }
    if let Object::Array(kids) = dict.get(b"Kids").map(resolved).unwrap_or(Object::Null) {
        for kid in &kids {
            collect_pairs_below(doc, kid, key, out, seen, depth + 1);
        }
    }
}

/// What a node of a name or number tree holds once pruned.
enum Held {
    /// No key at all: the node can go.
    Nothing,
    /// Its first and last key, in tree order.
    Keys(Box<(Object, Object)>),
    /// A node that could not be read, or too deep, is left as it is.
    Unknown,
}

/// Removes the pairs of a name or number tree that `goes` gives up, wherever in the tree they are.
/// Kids left without a key are taken out, and every node that has /Limits gets them set to the
/// first and last key it still holds, so no key that went stays there. A node met again (in
/// `seen`: the tree loops, or lists a node twice) is left as it is the second time.
fn prune_pairs(
    doc: &mut Document,
    node: &mut Object,
    key: &[u8],
    goes: &dyn Fn(&Object, &Object) -> bool,
    seen: &mut HashSet<ObjectId>,
    depth: usize,
) -> Held {
    if depth > MAX_DEPTH {
        return Held::Unknown;
    }
    let (id, mut dict) = match node {
        Object::Reference(id) if !seen.insert(*id) => return Held::Unknown,
        Object::Reference(id) => match doc.get_dictionary(*id) {
            Ok(dict) => (Some(*id), dict.clone()),
            Err(_) => return Held::Unknown,
        },
        Object::Dictionary(dict) => (None, dict.clone()),
        _ => return Held::Unknown,
    };
    let mut held: Vec<Held> = Vec::new();
    if let Ok(mut kids) = dict.get(b"Kids").cloned() {
        if let Object::Reference(kids_id) = kids {
            kids = doc.get_object(kids_id).cloned().unwrap_or(Object::Null);
        }
        if let Object::Array(items) = kids {
            let mut kept = Vec::with_capacity(items.len());
            for mut kid in items {
                match prune_pairs(doc, &mut kid, key, goes, seen, depth + 1) {
                    Held::Nothing => {}
                    other => {
                        held.push(other);
                        kept.push(kid);
                    }
                }
            }
            if kept.is_empty() {
                dict.remove(b"Kids");
            } else {
                dict.set("Kids", kept);
            }
        } else {
            held.push(Held::Unknown);
        }
    }
    if let Ok(mut pairs) = dict.get(key).cloned() {
        if let Object::Reference(pairs_id) = pairs {
            pairs = doc.get_object(pairs_id).cloned().unwrap_or(Object::Null);
        }
        if let Object::Array(items) = &pairs {
            let mut kept = Vec::with_capacity(items.len());
            for pair in items.chunks(2) {
                let [name, value] = pair else { continue };
                if !goes(name, value) {
                    kept.push(name.clone());
                    kept.push(value.clone());
                }
            }
            if let (Some(first), Some(last)) = (kept.first(), kept.len().checked_sub(2).and_then(|i| kept.get(i))) {
                held.push(Held::Keys(Box::new((first.clone(), last.clone()))));
            }
            dict.set(key.to_vec(), kept);
        } else {
            held.push(Held::Unknown);
        }
    }
    let held = match (held.first(), held.last()) {
        _ if held.iter().any(|h| matches!(h, Held::Unknown)) => Held::Unknown,
        (Some(Held::Keys(first)), Some(Held::Keys(last))) => Held::Keys(Box::new((first.0.clone(), last.1.clone()))),
        _ => Held::Nothing,
    };
    match &held {
        Held::Keys(keys) if dict.has(b"Limits") => dict.set("Limits", vec![keys.0.clone(), keys.1.clone()]),
        Held::Nothing => {
            dict.remove(b"Limits");
        }
        _ => {}
    }
    match id {
        Some(id) => {
            doc.objects.insert(id, Object::Dictionary(dict));
        }
        None => *node = Object::Dictionary(dict),
    }
    held
}
