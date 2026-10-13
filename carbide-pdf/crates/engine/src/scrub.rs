//! Removing what removed pages leave behind.
//!
//! Deleting a page, redacting it or leaving it out of an extract takes it out of the page tree,
//! but other parts of the document can still refer to it or to things only it used: form fields
//! hold the values of its widgets, structure elements hold its tagged text, links point at it,
//! and a resource dictionary shared between pages keeps the images and forms only it drew.
//! Anything still referred to is kept when the document is written, so these are cleaned up too.

use std::collections::{HashMap, HashSet};

use lopdf::content::Content;
use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::error::Result;
use crate::limits::DecodeBudget;
use crate::pdf::Pdf;

/// Resource categories whose entries content refers to by name.
const NAMED_RESOURCES: [&[u8]; 7] = [b"Font", b"XObject", b"ExtGState", b"ColorSpace", b"Pattern", b"Shading", b"Properties"];

/// Deeper nesting than this is treated as damaged and left alone.
const MAX_DEPTH: usize = 64;

impl Pdf {
    /// The annotations placed on `pages`, with the page each is on.
    fn annotations_on(&self, pages: &HashSet<ObjectId>) -> HashMap<ObjectId, ObjectId> {
        let mut found = HashMap::new();
        for page in pages {
            let Ok(dict) = self.doc.get_dictionary(*page) else { continue };
            let Some(Object::Array(annots)) = dict.get(b"Annots").ok().map(|a| self.resolve(a)) else { continue };
            found.extend(annots.iter().filter_map(|a| a.as_reference().ok()).map(|a| (a, *page)));
        }
        found
    }

    /// Takes widgets on `pages` out of the form, together with every field left without a widget,
    /// so their values and appearances are no longer part of the document. `goes` decides for each
    /// widget, given its page, its dictionary and whether it belongs to a signature field. A field
    /// that still has a widget elsewhere keeps its value: it is still shown there.
    pub(crate) fn drop_form_fields_on(&mut self, pages: &HashSet<ObjectId>, goes: &dyn Fn(ObjectId, &Dictionary, bool) -> bool) {
        let annotations = self.annotations_on(pages);
        let widgets = WidgetScope { pages, annotations: &annotations, goes };
        let Ok(catalog_id) = self.doc.trailer.get(b"Root").and_then(Object::as_reference) else { return };
        let Ok(form) = self.doc.get_dictionary(catalog_id).and_then(|c| c.get(b"AcroForm")).cloned() else { return };
        let (form_id, mut form) = match form {
            Object::Reference(id) => match self.doc.get_dictionary(id) {
                Ok(dict) => (Some(id), dict.clone()),
                Err(_) => return,
            },
            Object::Dictionary(dict) => (None, dict),
            _ => return,
        };
        let Ok(mut fields) = form.get(b"Fields").cloned() else { return };
        let mut removed = HashSet::new();
        let left = edit_array(&mut self.doc, &mut fields, |doc, items| {
            items.retain_mut(|item| !prune_field(doc, item, &widgets, None, &mut removed, 0));
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
    pub(crate) fn drop_structure_of(&mut self, pages: &HashSet<ObjectId>) {
        let Ok(catalog) = self.doc.catalog() else { return };
        let Ok(root_id) = catalog.get(b"StructTreeRoot").and_then(Object::as_reference) else { return };
        let Ok(mut root) = self.doc.get_dictionary(root_id).cloned() else { return };
        let annotations: HashSet<ObjectId> = self.annotations_on(pages).into_keys().collect();
        let mut removed = HashSet::new();
        if let Ok(mut kids) = root.get(b"K").cloned() {
            let scope = Scope { pages, annotations: &annotations };
            if prune_structure(&mut self.doc, &mut kids, None, &scope, &mut removed, 0) {
                root.remove(b"K");
            } else {
                root.set("K", kids);
            }
        }
        if !removed.is_empty() {
            for (tree, key) in [(&b"ParentTree"[..], &b"Nums"[..]), (b"IDTree", b"Names")] {
                if let Ok(mut node) = root.get(tree).cloned() {
                    prune_tree(&mut self.doc, &mut node, key, &removed, 0);
                    root.set(tree, node);
                }
            }
        }
        self.doc.objects.insert(root_id, Object::Dictionary(root));
    }

    /// Removes every reference to `ids` from the document: array items and dictionary entries
    /// that point at them. Links to a dropped page lose their target, for instance.
    pub(crate) fn drop_references_to(&mut self, ids: &HashSet<ObjectId>) {
        fn strip(object: &mut Object, ids: &HashSet<ObjectId>) {
            match object {
                Object::Array(items) => {
                    items.retain(|item| !item.as_reference().is_ok_and(|id| ids.contains(&id)));
                    items.iter_mut().for_each(|item| strip(item, ids));
                }
                Object::Dictionary(dict) => strip_dict(dict, ids),
                Object::Stream(stream) => strip_dict(&mut stream.dict, ids),
                _ => {}
            }
        }
        fn strip_dict(dict: &mut Dictionary, ids: &HashSet<ObjectId>) {
            let doomed: Vec<Vec<u8>> = dict
                .iter()
                .filter(|(_, value)| value.as_reference().is_ok_and(|id| ids.contains(&id)))
                .map(|(key, _)| key.clone())
                .collect();
            for key in doomed {
                dict.remove(&key);
            }
            dict.iter_mut().for_each(|(_, value)| strip(value, ids));
        }
        for object in self.doc.objects.values_mut() {
            strip(object, ids);
        }
        strip_dict(&mut self.doc.trailer, ids);
    }

    /// Gives every page a resource dictionary of its own that holds only what its content uses,
    /// and stops page-tree nodes from passing resources down. Images, forms and fonts that only
    /// removed content used are then no longer part of the document, even when the pages shared
    /// one resource dictionary. A page whose content cannot be read keeps all of its resources.
    pub(crate) fn trim_resources(&mut self) -> Result<()> {
        let pages = self.page_ids();
        let mut shared = HashSet::new();
        let mut seen = HashSet::new();
        for page in &pages {
            if let Ok(id) = self.doc.get_dictionary(*page).and_then(|d| d.get(b"Resources")).and_then(Object::as_reference) {
                if !seen.insert(id) {
                    shared.insert(id);
                }
            }
        }
        let mut changes = Vec::new();
        let mut scan = NameScan { budget: DecodeBudget::new(), names: HashMap::new() };
        for page in &pages {
            let Some(Object::Dictionary(resources)) = self.inherited(*page, b"Resources") else { continue };
            let own = self.doc.get_dictionary(*page)?.get(b"Resources").ok().cloned();
            let kept = match self.names_used_by_page(*page, &resources, &mut scan) {
                Some(names) => self.keep_named(&resources, &names),
                None => resources.clone(),
            };
            let independent = match &own {
                Some(Object::Reference(id)) => !shared.contains(id),
                Some(_) => true,
                None => false,
            };
            if !(independent && kept == self.with_categories_resolved(&resources)) {
                changes.push((*page, kept));
            }
        }
        for (page, resources) in changes {
            self.doc.get_dictionary_mut(page)?.set("Resources", resources);
        }
        let mut nodes = vec![self.pages_root()?];
        let mut visited = HashSet::new();
        while let Some(node) = nodes.pop() {
            if !visited.insert(node) {
                continue;
            }
            let Ok(dict) = self.doc.get_dictionary_mut(node) else { continue };
            if !dict.has_type(b"Pages") {
                continue;
            }
            dict.remove(b"Resources");
            if let Ok(Object::Array(kids)) = dict.get(b"Kids") {
                nodes.extend(kids.iter().filter_map(|k| k.as_reference().ok()));
            }
        }
        Ok(())
    }

    /// The resource names the page's content refers to, following forms, patterns and Type 3
    /// fonts that use the page's resources instead of their own. `None` when some content cannot
    /// be read, or when decoding it would go past the limits in [`crate::limits`].
    fn names_used_by_page(&self, page: ObjectId, resources: &Dictionary, scan: &mut NameScan) -> Option<HashSet<Vec<u8>>> {
        let mut names = HashSet::new();
        for stream in self.doc.get_page_contents(page) {
            let content = self.doc.get_object(stream).and_then(Object::as_stream).ok()?;
            names.extend(scan.names_in(Some(stream), content)?);
        }
        // Resources that are drawn without resources of their own use the page's.
        let mut followed = HashSet::new();
        loop {
            let mut more = Vec::new();
            for category in NAMED_RESOURCES {
                let Object::Dictionary(entries) = resources.get(category).map(|o| self.resolve(o)).unwrap_or(Object::Null) else {
                    continue;
                };
                for (name, value) in entries.iter() {
                    if !names.contains(name) || !followed.insert((category, name.clone())) {
                        continue;
                    }
                    let Some(resolved) = self.resolved(value) else { continue };
                    // Names in the resource's own dictionary: an image's colour space, say.
                    if let Some(dict) = resolved.as_dict().ok().or_else(|| resolved.as_stream().ok().map(|s| &s.dict)) {
                        more.extend(dict.iter().filter_map(|(_, v)| v.as_name().ok().map(<[u8]>::to_vec)));
                    }
                    match resolved {
                        Object::Stream(stream) if !stream.dict.has(b"Resources") => {
                            more.extend(scan.names_in(value.as_reference().ok(), stream)?);
                        }
                        Object::Dictionary(font) if font.get(b"Subtype").and_then(Object::as_name).is_ok_and(|s| s == b"Type3") && !font.has(b"Resources") => {
                            let Object::Dictionary(procs) = font.get(b"CharProcs").map(|o| self.resolve(o)).unwrap_or(Object::Null) else { continue };
                            for (_, glyph) in procs.iter() {
                                if let Some(Object::Stream(stream)) = self.resolved(glyph) {
                                    more.extend(scan.names_in(glyph.as_reference().ok(), stream)?);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            let before = names.len();
            names.extend(more);
            if names.len() == before {
                return Some(names);
            }
        }
    }

    /// `resources` with only the named entries in `names`, as a direct dictionary.
    fn keep_named(&self, resources: &Dictionary, names: &HashSet<Vec<u8>>) -> Dictionary {
        let mut kept = Dictionary::new();
        for (key, value) in resources.iter() {
            if !NAMED_RESOURCES.contains(&key.as_slice()) {
                kept.set(key.clone(), value.clone());
                continue;
            }
            let Object::Dictionary(entries) = self.resolve(value) else {
                kept.set(key.clone(), value.clone());
                continue;
            };
            let used: Dictionary = entries.iter().filter(|(name, _)| names.contains(*name)).map(|(n, v)| (n.clone(), v.clone())).collect();
            if !used.is_empty() {
                kept.set(key.clone(), used);
            }
        }
        kept
    }

    /// `resources` with its categories as direct dictionaries, for comparing with `keep_named`.
    fn with_categories_resolved(&self, resources: &Dictionary) -> Dictionary {
        resources
            .iter()
            .map(|(key, value)| match self.resolve(value) {
                Object::Dictionary(entries) if NAMED_RESOURCES.contains(&key.as_slice()) => {
                    (key.clone(), Object::Dictionary(entries))
                }
                _ => (key.clone(), value.clone()),
            })
            .filter(|(_, value)| !matches!(value, Object::Dictionary(d) if d.is_empty()))
            .collect()
    }
}

impl Pdf {
    /// `object`, or the object it refers to, without copying it.
    fn resolved<'a>(&'a self, object: &'a Object) -> Option<&'a Object> {
        match object {
            Object::Reference(id) => self.doc.get_object(*id).ok(),
            other => Some(other),
        }
    }
}

/// The names each content stream uses, found once per stream however many pages draw it, and
/// decoded within one budget for the whole document.
struct NameScan {
    budget: DecodeBudget,
    names: HashMap<ObjectId, Option<HashSet<Vec<u8>>>>,
}

impl NameScan {
    /// The names `stream` (object `id`, when it is one of its own) uses as operands; `None` when
    /// it cannot be read or decoding it would go past the budget.
    fn names_in(&mut self, id: Option<ObjectId>, stream: &lopdf::Stream) -> Option<HashSet<Vec<u8>>> {
        if let Some(found) = id.and_then(|id| self.names.get(&id)) {
            return found.clone();
        }
        let found = self.budget.stream(stream).ok().and_then(|plain| {
            let mut names = HashSet::new();
            collect_names(&plain, &mut names).map(|_| names)
        });
        if let Some(id) = id {
            self.names.insert(id, found.clone());
        }
        found
    }
}

/// The area a widget covers on its page, in user space; `None` for an invisible one.
pub(crate) fn widget_area(widget: &Dictionary) -> Option<[f32; 4]> {
    let rect = widget.get(b"Rect").and_then(Object::as_array).ok()?;
    let values: Vec<f32> = rect.iter().filter_map(|v| v.as_float().ok()).collect();
    let [x0, y0, x1, y1] = values[..] else { return None };
    let area = [x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)];
    (area[2] > area[0] && area[3] > area[1]).then_some(area)
}

/// Adds every name `content` uses as an operand, including inside dictionaries (marked-content
/// properties, inline images). `None` when the content cannot be parsed.
fn collect_names(content: &[u8], names: &mut HashSet<Vec<u8>>) -> Option<()> {
    fn walk(object: &Object, names: &mut HashSet<Vec<u8>>) {
        match object {
            Object::Name(name) => {
                names.insert(name.clone());
            }
            Object::Array(items) => items.iter().for_each(|item| walk(item, names)),
            Object::Dictionary(dict) => dict.iter().for_each(|(_, value)| walk(value, names)),
            _ => {}
        }
    }
    let content = Content::decode(content).ok()?;
    for operation in &content.operations {
        operation.operands.iter().for_each(|operand| walk(operand, names));
    }
    Some(())
}

/// Runs `edit` on the array `holder` is, or refers to, and stores the result where it came from.
/// `None` when `holder` is not an array.
fn edit_array<T>(doc: &mut Document, holder: &mut Object, edit: impl FnOnce(&mut Document, &mut Vec<Object>) -> T) -> Option<T> {
    match holder {
        Object::Array(items) => Some(edit(doc, items)),
        Object::Reference(id) => {
            let id = *id;
            let mut items = doc.get_object(id).and_then(Object::as_array).ok()?.clone();
            let result = edit(doc, &mut items);
            doc.objects.insert(id, Object::Array(items));
            Some(result)
        }
        _ => None,
    }
}

struct WidgetScope<'a> {
    pages: &'a HashSet<ObjectId>,
    /// Annotations on those pages, with their page.
    annotations: &'a HashMap<ObjectId, ObjectId>,
    goes: &'a dyn Fn(ObjectId, &Dictionary, bool) -> bool,
}

/// Prunes one node of the form's field tree. True when the node goes: a widget on one of the
/// pages that `goes` gives up, or a field all of whose widgets went. `field_type` is the type
/// (`/FT`) the node inherits.
fn prune_field(
    doc: &mut Document,
    item: &mut Object,
    scope: &WidgetScope<'_>,
    field_type: Option<&[u8]>,
    removed: &mut HashSet<ObjectId>,
    depth: usize,
) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    let (id, mut dict) = match item {
        Object::Reference(id) => match doc.get_dictionary(*id) {
            Ok(dict) => (Some(*id), dict.clone()),
            Err(_) => return false,
        },
        Object::Dictionary(dict) => (None, dict.clone()),
        _ => return false,
    };
    let field_type = dict.get(b"FT").and_then(Object::as_name).ok().map(<[u8]>::to_vec).or(field_type.map(<[u8]>::to_vec));
    if dict.get(b"Subtype").and_then(Object::as_name).is_ok_and(|s| s == b"Widget") {
        let page = dict
            .get(b"P")
            .and_then(Object::as_reference)
            .ok()
            .filter(|p| scope.pages.contains(p))
            .or_else(|| id.and_then(|id| scope.annotations.get(&id).copied()));
        if page.is_some_and(|page| (scope.goes)(page, &dict, field_type.as_deref() == Some(b"Sig"))) {
            removed.extend(id);
            return true;
        }
    }
    let Ok(mut kids) = dict.get(b"Kids").cloned() else { return false };
    let counts = edit_array(doc, &mut kids, |doc, items| {
        let before = items.len();
        items.retain_mut(|kid| !prune_field(doc, kid, scope, field_type.as_deref(), removed, depth + 1));
        (before, items.len())
    });
    dict.set("Kids", kids);
    match id {
        Some(id) => {
            doc.objects.insert(id, Object::Dictionary(dict));
        }
        None => *item = Object::Dictionary(dict),
    }
    if matches!(counts, Some((before, 0)) if before > 0) {
        removed.extend(id);
        return true;
    }
    false
}

struct Scope<'a> {
    pages: &'a HashSet<ObjectId>,
    annotations: &'a HashSet<ObjectId>,
}

/// Prunes one structure-tree item (`/K` of an element, or one of its kids). True when the item
/// goes. `page` is the page the enclosing element is on.
fn prune_structure(
    doc: &mut Document,
    item: &mut Object,
    page: Option<ObjectId>,
    scope: &Scope<'_>,
    removed: &mut HashSet<ObjectId>,
    depth: usize,
) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    if let Object::Array(items) = item {
        items.retain_mut(|kid| !prune_structure(doc, kid, page, scope, removed, depth + 1));
        return false;
    }
    let (id, mut dict) = match item {
        Object::Reference(id) => match doc.get_dictionary(*id) {
            Ok(dict) => (Some(*id), dict.clone()),
            Err(_) => return false,
        },
        Object::Dictionary(dict) => (None, dict.clone()),
        // Marked-content identifiers belong to the enclosing element's page, which stays.
        _ => return false,
    };
    let page = dict.get(b"Pg").and_then(Object::as_reference).ok().or(page);
    let on_dropped_page = page.is_some_and(|p| scope.pages.contains(&p));
    if dict.has_type(b"MCR") {
        return on_dropped_page;
    }
    if dict.has_type(b"OBJR") {
        let annotation = dict.get(b"Obj").and_then(Object::as_reference).is_ok_and(|o| scope.annotations.contains(&o));
        return on_dropped_page || annotation;
    }
    if on_dropped_page {
        collect_elements(doc, item, removed, depth);
        return true;
    }
    if let Ok(mut kids) = dict.get(b"K").cloned() {
        if let Object::Reference(kids_id) = kids {
            if let Ok(Object::Array(_)) = doc.get_object(kids_id) {
                kids = doc.get_object(kids_id).cloned().unwrap_or(Object::Null);
            }
        }
        if prune_structure(doc, &mut kids, page, scope, removed, depth + 1) {
            dict.remove(b"K");
        } else {
            dict.set("K", kids);
        }
    }
    match id {
        Some(id) => {
            doc.objects.insert(id, Object::Dictionary(dict));
        }
        None => *item = Object::Dictionary(dict),
    }
    false
}

/// Records `item` and every structure element below it as removed.
fn collect_elements(doc: &Document, item: &Object, removed: &mut HashSet<ObjectId>, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let dict = match item {
        Object::Reference(id) => {
            if !removed.insert(*id) {
                return;
            }
            match doc.get_object(*id) {
                Ok(Object::Dictionary(dict)) => dict,
                Ok(other @ Object::Array(_)) => return collect_elements(doc, other, removed, depth + 1),
                _ => return,
            }
        }
        Object::Dictionary(dict) => dict,
        Object::Array(items) => {
            items.iter().for_each(|kid| collect_elements(doc, kid, removed, depth + 1));
            return;
        }
        _ => return,
    };
    if let Ok(kids) = dict.get(b"K") {
        collect_elements(doc, kids, removed, depth + 1);
    }
}

/// Drops the entries of a number tree (`key` = `Nums`) or name tree (`Names`) that refer to
/// removed elements; in arrays of elements those become null.
fn prune_tree(doc: &mut Document, node: &mut Object, key: &[u8], removed: &HashSet<ObjectId>, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let (id, mut dict) = match node {
        Object::Reference(id) => match doc.get_dictionary(*id) {
            Ok(dict) => (Some(*id), dict.clone()),
            Err(_) => return,
        },
        Object::Dictionary(dict) => (None, dict.clone()),
        _ => return,
    };
    if let Ok(mut kids) = dict.get(b"Kids").cloned() {
        edit_array(doc, &mut kids, |doc, items| items.iter_mut().for_each(|kid| prune_tree(doc, kid, key, removed, depth + 1)));
        dict.set("Kids", kids);
    }
    if let Ok(mut pairs) = dict.get(key).cloned() {
        edit_array(doc, &mut pairs, |doc, items| {
            let mut kept = Vec::with_capacity(items.len());
            for pair in items.chunks(2) {
                let [name, value] = pair else { continue };
                let mut value = value.clone();
                if value.as_reference().is_ok_and(|v| removed.contains(&v)) {
                    continue;
                }
                edit_array(doc, &mut value, |_, elements| {
                    for element in elements.iter_mut() {
                        if element.as_reference().is_ok_and(|e| removed.contains(&e)) {
                            *element = Object::Null;
                        }
                    }
                });
                kept.push(name.clone());
                kept.push(value);
            }
            *items = kept;
        });
        dict.set(key.to_vec(), pairs);
    }
    match id {
        Some(id) => {
            doc.objects.insert(id, Object::Dictionary(dict));
        }
        None => *node = Object::Dictionary(dict),
    }
}
