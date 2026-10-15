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
    pub(crate) fn drop_structure_of(&mut self, pages: &HashSet<ObjectId>) {
        let Ok(catalog) = self.doc.catalog() else {
            return;
        };
        let Ok(root_id) = catalog
            .get(b"StructTreeRoot")
            .and_then(Object::as_reference)
        else {
            return;
        };
        let Ok(mut root) = self.doc.get_dictionary(root_id).cloned() else {
            return;
        };
        let annotations: HashSet<ObjectId> = self.annotations_on(pages).into_keys().collect();
        let mut removed = HashSet::new();
        if let Ok(mut kids) = root.get(b"K").cloned() {
            let scope = Scope {
                pages,
                annotations: &annotations,
            };
            if prune_structure(
                &mut self.doc,
                &mut kids,
                None,
                &scope,
                &mut removed,
                &mut HashSet::new(),
                0,
            ) {
                root.remove(b"K");
            } else {
                root.set("K", kids);
            }
        }
        if !removed.is_empty() {
            for (tree, key) in [(&b"ParentTree"[..], &b"Nums"[..]), (b"IDTree", b"Names")] {
                if let Ok(mut node) = root.get(tree).cloned() {
                    prune_tree(
                        &mut self.doc,
                        &mut node,
                        key,
                        &removed,
                        &mut HashSet::new(),
                        0,
                    );
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
    /// An annotation appearance with no resources of its own is drawn with the page's, so the
    /// names it uses are kept too; one that cannot be read keeps the page's resources whole.
    ///
    /// Each page's content, with the forms it draws, is decoded within the limit for one page in
    /// [`crate::limits`]; past it, with forms nested deeper than a page may draw them, or with
    /// more different resource names than [`MAX_NAMES`], this fails with [`Error::TooLarge`]
    /// rather than keep resources that may be what removed content alone used.
    pub(crate) fn trim_resources(&mut self) -> Result<()> {
        let pages = self.page_ids();
        let mut shared = HashSet::new();
        let mut seen = HashSet::new();
        for page in &pages {
            if let Ok(id) = self
                .doc
                .get_dictionary(*page)
                .and_then(|d| d.get(b"Resources"))
                .and_then(Object::as_reference)
            {
                if !seen.insert(id) {
                    shared.insert(id);
                }
            }
        }
        let mut changes = Vec::new();
        let mut scan = NameScan {
            budget: DecodeBudget::for_page(),
            names_left: MAX_NAMES,
            names: HashMap::new(),
            joined: HashMap::new(),
            trimmed: HashSet::new(),
        };
        // Forms, patterns, Type 3 fonts and annotation appearances with resources of their own,
        // trimmed in turn.
        let mut nested = HashMap::new();
        for page in &pages {
            let Some(Object::Dictionary(resources)) = self.inherited(*page, b"Resources") else {
                continue;
            };
            let own = self
                .doc
                .get_dictionary(*page)?
                .get(b"Resources")
                .ok()
                .cloned();
            scan.budget = DecodeBudget::for_page();
            scan.names_left = MAX_NAMES;
            let group = self.group_colour_space(self.doc.get_dictionary(*page)?);
            let mut kept = match self.names_used_by_page(*page, &resources, &mut scan)? {
                Some(mut uses) => {
                    uses.extend(group);
                    if self.include_inherited_appearance_uses(
                        *page, &resources, &mut uses, &mut scan,
                    )? {
                        self.keep_named(&resources, &uses)
                    } else {
                        resources.clone()
                    }
                }
                None => resources.clone(),
            };
            self.trim_nested(&mut kept, &mut scan, &mut nested, 0)?;
            self.trim_appearances(*page, &mut scan, &mut nested)?;
            let independent = match &own {
                Some(Object::Reference(id)) => !shared.contains(id),
                Some(_) => true,
                None => false,
            };
            let unchanged = kept == resources || kept == self.with_categories_resolved(&resources);
            if !(independent && unchanged) {
                changes.push((*page, kept));
            }
        }
        self.doc.objects.extend(nested);
        for (page, resources) in changes {
            self.doc
                .get_dictionary_mut(page)?
                .set("Resources", resources);
        }
        let mut nodes = vec![self.pages_root()?];
        let mut visited = HashSet::new();
        while let Some(node) = nodes.pop() {
            if !visited.insert(node) {
                continue;
            }
            let Ok(dict) = self.doc.get_dictionary_mut(node) else {
                continue;
            };
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

    /// The resources the page's content uses, following forms, patterns and Type 3 fonts that use
    /// the page's resources instead of their own. `None` when some content cannot be read; an
    /// error when decoding it would go past the limits in [`crate::limits`].
    fn names_used_by_page(
        &self,
        page: ObjectId,
        resources: &Dictionary,
        scan: &mut NameScan,
    ) -> Result<Option<Uses>> {
        let streams = self.doc.get_page_contents(page);
        let Some(uses) = scan.page_uses(&self.doc, streams)? else {
            return Ok(None);
        };
        self.scope_uses(uses, resources, scan)
    }

    /// `uses`, the resources some content uses directly, together with what the forms, patterns
    /// and Type 3 fonts among them that have no resources of their own use from `resources`.
    fn scope_uses(
        &self,
        mut uses: Uses,
        resources: &Dictionary,
        scan: &mut NameScan,
    ) -> Result<Option<Uses>> {
        // Resources that are drawn without resources of their own use the enclosing ones.
        let mut followed = HashSet::new();
        loop {
            let mut more = Vec::new();
            for category in NAMED_RESOURCES {
                let Object::Dictionary(entries) = resources
                    .get(category)
                    .map(|o| self.resolve(o))
                    .unwrap_or(Object::Null)
                else {
                    continue;
                };
                for (name, value) in entries.iter() {
                    let key = (category, name.clone());
                    if !uses.contains(&key) || !followed.insert(key) {
                        continue;
                    }
                    let Some(resolved) = self.resolved(value) else {
                        continue;
                    };
                    // Names in the resource's own dictionary or array can only stand for colour
                    // spaces: an image's, or the base of an indexed colour space.
                    let values: Vec<&Object> = match resolved {
                        Object::Dictionary(dict) => dict.iter().map(|(_, v)| v).collect(),
                        Object::Stream(stream) => stream.dict.iter().map(|(_, v)| v).collect(),
                        Object::Array(items) => items.iter().collect(),
                        _ => Vec::new(),
                    };
                    for value in values {
                        let items = match value {
                            Object::Array(items) => items.iter().collect(),
                            other => vec![other],
                        };
                        more.extend(
                            items
                                .into_iter()
                                .filter_map(|v| v.as_name().ok())
                                .map(|n| (COLOR_SPACE, n.to_vec())),
                        );
                    }
                    if category == EXT_G_STATE {
                        if let Some(id) = self.soft_mask_form(value) {
                            if let Ok(Object::Stream(mask)) = self.doc.get_object(id) {
                                if !mask.dict.has(b"Resources") && is_content(&mask.dict) {
                                    let Some(found) = scan.uses_in(Some(id), mask)? else {
                                        return Ok(None);
                                    };
                                    more.extend(found);
                                }
                            }
                        }
                    }
                    match resolved {
                        Object::Stream(stream)
                            if !stream.dict.has(b"Resources") && is_content(&stream.dict) =>
                        {
                            let Some(found) = scan.uses_in(value.as_reference().ok(), stream)?
                            else {
                                return Ok(None);
                            };
                            more.extend(found);
                        }
                        Object::Dictionary(font)
                            if font
                                .get(b"Subtype")
                                .and_then(Object::as_name)
                                .is_ok_and(|s| s == b"Type3")
                                && !font.has(b"Resources") =>
                        {
                            let Object::Dictionary(procs) = font
                                .get(b"CharProcs")
                                .map(|o| self.resolve(o))
                                .unwrap_or(Object::Null)
                            else {
                                continue;
                            };
                            for (_, glyph) in procs.iter() {
                                if let Some(Object::Stream(stream)) = self.resolved(glyph) {
                                    let Some(found) =
                                        scan.uses_in(glyph.as_reference().ok(), stream)?
                                    else {
                                        return Ok(None);
                                    };
                                    more.extend(found);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            let before = uses.len();
            uses.extend(more);
            if uses.len() == before {
                return Ok(Some(uses));
            }
        }
    }

    /// `resources` with only the named entries in `uses` and the default colour spaces, as a
    /// direct dictionary.
    fn keep_named(&self, resources: &Dictionary, uses: &Uses) -> Dictionary {
        let mut kept = Dictionary::new();
        for (key, value) in resources.iter() {
            let Some(category) = NAMED_RESOURCES.into_iter().find(|c| *c == key.as_slice()) else {
                kept.set(key.clone(), value.clone());
                continue;
            };
            let Object::Dictionary(entries) = self.resolve(value) else {
                kept.set(key.clone(), value.clone());
                continue;
            };
            let used: Dictionary = entries
                .iter()
                .filter(|(name, _)| {
                    uses.contains(&(category, name.to_vec()))
                        || (category == COLOR_SPACE && DEFAULT_SPACES.contains(&name.as_slice()))
                })
                .map(|(n, v)| (n.clone(), v.clone()))
                .collect();
            if !used.is_empty() {
                kept.set(key.clone(), used);
            }
        }
        kept
    }

    /// Trims the resources of each form, tiling pattern and Type 3 font in `resources` that has
    /// resources of its own to what its own content uses, and so on down. Objects that change go
    /// into `changed`, entries written straight into `resources` change in place. An object is
    /// trimmed once however often it is drawn, so shared and looping resources end too.
    fn trim_nested(
        &self,
        resources: &mut Dictionary,
        scan: &mut NameScan,
        changed: &mut HashMap<ObjectId, Object>,
        depth: usize,
    ) -> Result<()> {
        for category in [XOBJECT, PATTERN, FONT] {
            let Ok(value) = resources.get(category) else {
                continue;
            };
            let Object::Dictionary(mut entries) = self.resolve(value) else {
                continue;
            };
            let mut edited = false;
            for (_, entry) in entries.iter_mut() {
                if let Object::Reference(id) = entry {
                    self.trim_referenced(*id, scan, changed, depth)?;
                } else {
                    edited |= self.trim_own_resources(None, entry, scan, changed, depth)?;
                }
            }
            if edited {
                resources.set(category, entries);
            }
        }
        // A soft mask's form is drawn through the graphics state that names it.
        if let Some(Object::Dictionary(states)) = resources
            .get(EXT_G_STATE)
            .ok()
            .and_then(|s| self.resolved(s))
        {
            for (_, state) in states.iter() {
                if let Some(id) = self.soft_mask_form(state) {
                    self.trim_referenced(id, scan, changed, depth)?;
                }
            }
        }
        Ok(())
    }

    /// [`Self::trim_own_resources`] for the object `id`, once however often it is reached.
    fn trim_referenced(
        &self,
        id: ObjectId,
        scan: &mut NameScan,
        changed: &mut HashMap<ObjectId, Object>,
        depth: usize,
    ) -> Result<()> {
        if !scan.trimmed.insert(id) {
            return Ok(());
        }
        let Ok(object) = self.doc.get_object(id) else {
            return Ok(());
        };
        let mut object = object.clone();
        if self.trim_own_resources(Some(id), &mut object, scan, changed, depth)? {
            changed.insert(id, object);
        }
        Ok(())
    }

    /// The form a graphics state's soft mask (`/SMask` `/G`) draws, if it has one.
    fn soft_mask_form(&self, state: &Object) -> Option<ObjectId> {
        let Some(Object::Dictionary(state)) = self.resolved(state) else {
            return None;
        };
        let Some(Object::Dictionary(mask)) =
            state.get(b"SMask").ok().and_then(|m| self.resolved(m))
        else {
            return None;
        };
        mask.get(b"G").and_then(Object::as_reference).ok()
    }

    /// The appearance streams of the annotations on `page`: normal, rollover and down, and each
    /// state where an appearance has several. Streams are indirect objects, so each entry is an
    /// object id.
    fn appearance_ids(&self, page: ObjectId) -> Result<Vec<ObjectId>> {
        let mut ids = Vec::new();
        let Some(Object::Array(annotations)) = self
            .doc
            .get_dictionary(page)?
            .get(b"Annots")
            .ok()
            .map(|a| self.resolve(a))
        else {
            return Ok(ids);
        };
        for annotation in &annotations {
            let Object::Dictionary(annotation) = self.resolve(annotation) else {
                continue;
            };
            let Some(Object::Dictionary(appearances)) =
                annotation.get(b"AP").ok().map(|ap| self.resolve(ap))
            else {
                continue;
            };
            for key in [&b"N"[..], b"R", b"D"] {
                let Ok(appearance) = appearances.get(key) else {
                    continue;
                };
                match self.resolve(appearance) {
                    Object::Dictionary(states) => ids.extend(
                        states
                            .iter()
                            .filter_map(|(_, state)| state.as_reference().ok()),
                    ),
                    _ => ids.extend(appearance.as_reference()),
                }
            }
        }
        ids.sort_unstable();
        ids.dedup();
        Ok(ids)
    }

    /// Adds what annotation appearances on `page` draw from the page's `resources` to `uses`.
    /// An appearance with a resource dictionary of its own is trimmed on its own. Returns false
    /// when an appearance that draws from the page cannot be read, so the caller keeps the
    /// page's resources whole.
    fn include_inherited_appearance_uses(
        &self,
        page: ObjectId,
        resources: &Dictionary,
        uses: &mut Uses,
        scan: &mut NameScan,
    ) -> Result<bool> {
        for id in self.appearance_ids(page)? {
            let Ok(Object::Stream(stream)) = self.doc.get_object(id) else {
                continue;
            };
            if stream.dict.has(b"Resources") {
                continue;
            }
            let Some(found) = scan.uses_in(Some(id), stream)? else {
                return Ok(false);
            };
            let Some(found) = self.scope_uses(found, resources, scan)? else {
                return Ok(false);
            };
            uses.extend(found);
        }
        Ok(true)
    }

    /// Trims the resources of the appearances of the annotations on `page` (normal, rollover and
    /// down, one for each state of the annotation where it has several) as [`Self::trim_nested`]
    /// trims those of the forms the page draws. An annotation that stays keeps its appearances,
    /// and their resources can be shared with content that went.
    fn trim_appearances(
        &self,
        page: ObjectId,
        scan: &mut NameScan,
        changed: &mut HashMap<ObjectId, Object>,
    ) -> Result<()> {
        for id in self.appearance_ids(page)? {
            if !scan.trimmed.insert(id) {
                continue;
            }
            let Ok(object) = self.doc.get_object(id) else {
                continue;
            };
            let mut object = object.clone();
            if self.trim_own_resources(Some(id), &mut object, scan, changed, 0)? {
                changed.insert(id, object);
            }
        }
        Ok(())
    }

    /// Trims the resources `owner` (a form, a tiling pattern or a Type 3 font; object `id` when
    /// it is one of its own) has of its own to what its content uses, and those of what they hold
    /// in turn. True when `owner` changed. When its content cannot be read, its resources are kept
    /// whole, but what they hold is still trimmed. `depth` is how many such owners enclose it;
    /// one nested deeper than a page may draw forms (see [`MAX_FORM_DEPTH`]) is an error rather
    /// than left with resources that may be what removed content alone used.
    fn trim_own_resources(
        &self,
        id: Option<ObjectId>,
        owner: &mut Object,
        scan: &mut NameScan,
        changed: &mut HashMap<ObjectId, Object>,
        depth: usize,
    ) -> Result<bool> {
        let dict = match owner {
            Object::Stream(stream) => &stream.dict,
            Object::Dictionary(dict) => &*dict,
            _ => return Ok(false),
        };
        let Some(Object::Dictionary(resources)) =
            dict.get(b"Resources").ok().map(|r| self.resolve(r))
        else {
            return Ok(false);
        };
        let group = self.group_colour_space(dict);
        let content = match &*owner {
            Object::Stream(stream) if is_content(&stream.dict) => scan.uses_in(id, stream)?,
            Object::Dictionary(font)
                if font
                    .get(b"Subtype")
                    .and_then(Object::as_name)
                    .is_ok_and(|s| s == b"Type3") =>
            {
                self.glyph_uses(font, scan)?
            }
            _ => return Ok(false),
        };
        if depth >= MAX_FORM_DEPTH {
            return Err(Error::TooLarge(messages::TOO_COMPLEX.into()));
        }
        let uses = match content {
            Some(mut uses) => {
                uses.extend(group);
                self.scope_uses(uses, &resources, scan)?
            }
            None => None,
        };
        let mut kept = match &uses {
            Some(uses) => self.keep_named(&resources, uses),
            None => resources.clone(),
        };
        self.trim_nested(&mut kept, scan, changed, depth + 1)?;
        if kept == resources || kept == self.with_categories_resolved(&resources) {
            return Ok(false);
        }
        match owner {
            Object::Stream(stream) => stream.dict.set("Resources", kept),
            Object::Dictionary(dict) => dict.set("Resources", kept),
            _ => {}
        }
        Ok(true)
    }

    /// The colour space a transparency group (`/Group` of a page or form) names from its resources
    /// as its blending space, which its content need not use.
    fn group_colour_space(&self, owner: &Dictionary) -> Option<(&'static [u8], Vec<u8>)> {
        let Object::Dictionary(group) = owner.get(b"Group").map(|g| self.resolve(g)).ok()? else {
            return None;
        };
        let name = group.get(b"CS").and_then(Object::as_name).ok()?;
        Some((COLOR_SPACE, name.to_vec()))
    }

    /// The resources the glyphs of the Type 3 font `font` use; `None` when one cannot be read.
    fn glyph_uses(&self, font: &Dictionary, scan: &mut NameScan) -> Result<Option<Uses>> {
        let mut uses = HashSet::new();
        let Object::Dictionary(procs) = font
            .get(b"CharProcs")
            .map(|o| self.resolve(o))
            .unwrap_or(Object::Null)
        else {
            return Ok(Some(uses));
        };
        for (_, glyph) in procs.iter() {
            let Some(Object::Stream(stream)) = self.resolved(glyph) else {
                continue;
            };
            let Some(found) = scan.uses_in(glyph.as_reference().ok(), stream)? else {
                return Ok(None);
            };
            uses.extend(found);
        }
        Ok(Some(uses))
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

/// The resources each content stream uses, found once per stream however many pages draw it,
/// each page's streams decoded within the budget of one page.
struct NameScan {
    budget: DecodeBudget,
    /// How many more different names the page being read may use (see [`MAX_NAMES`]).
    names_left: usize,
    names: HashMap<ObjectId, Option<Uses>>,
    /// What the content of pages split over several streams uses, by the streams, in order.
    joined: HashMap<Vec<ObjectId>, Option<Uses>>,
    /// Forms, patterns and fonts whose own resources were already trimmed.
    trimmed: HashSet<ObjectId>,
}

impl NameScan {
    /// The resources a page's content, the streams `streams`, uses; `None` when one cannot be
    /// decoded, and an error when decoding them would go past the budget. The streams are read
    /// as one, as the page's content is: an operator can take its operands from the stream
    /// before its own, so what one stream uses on its own is not what it uses on a page.
    fn page_uses(&mut self, doc: &Document, streams: Vec<ObjectId>) -> Result<Option<Uses>> {
        if let [id] = streams[..] {
            let Ok(content) = doc.get_object(id).and_then(Object::as_stream) else {
                return Ok(None);
            };
            return self.uses_in(Some(id), content);
        }
        if let Some(found) = self.joined.get(&streams) {
            let found = found.clone();
            self.count(&found)?;
            return Ok(found);
        }
        let mut joined = Vec::new();
        let mut readable = true;
        for id in &streams {
            let Ok(content) = doc.get_object(*id).and_then(Object::as_stream) else {
                readable = false;
                break;
            };
            match self.budget.stream(content) {
                Ok(mut plain) => {
                    joined.append(&mut plain);
                    joined.push(b'\n');
                }
                Err(error @ Error::TooLarge(_)) => return Err(error),
                Err(_) => {
                    readable = false;
                    break;
                }
            }
        }
        let found = if readable {
            Some(uses_in(&joined, &mut self.names_left)?)
        } else {
            None
        };
        self.joined.insert(streams, found.clone());
        Ok(found)
    }

    /// The resources `stream` (object `id`, when it is one of its own) uses; `None` when it
    /// cannot be decoded, and an error when decoding it would go past the budget.
    fn uses_in(&mut self, id: Option<ObjectId>, stream: &lopdf::Stream) -> Result<Option<Uses>> {
        if let Some(found) = id.and_then(|id| self.names.get(&id)) {
            let found = found.clone();
            self.count(&found)?;
            return Ok(found);
        }
        let found = match self.budget.stream(stream) {
            Ok(plain) => Some(uses_in(&plain, &mut self.names_left)?),
            Err(error @ Error::TooLarge(_)) => return Err(error),
            Err(_) => None,
        };
        if let Some(id) = id {
            self.names.insert(id, found.clone());
        }
        Ok(found)
    }

    /// Counts the names in `found`, found before, against those the page being read may use: the
    /// copy of them it is given takes as much memory as reading them did.
    fn count(&mut self, found: &Option<Uses>) -> Result<()> {
        let used = found.as_ref().map_or(0, HashSet::len);
        self.names_left = self
            .names_left
            .checked_sub(used)
            .ok_or_else(|| Error::TooLarge(messages::TOO_COMPLEX.into()))?;
        Ok(())
    }
}

/// The area a widget covers on its page, in user space; `None` for an invisible one.
pub(crate) fn widget_area(widget: &Dictionary) -> Option<[f32; 4]> {
    let rect = widget.get(b"Rect").and_then(Object::as_array).ok()?;
    let values: Vec<f32> = rect.iter().filter_map(|v| v.as_float().ok()).collect();
    let [x0, y0, x1, y1] = values[..] else {
        return None;
    };
    let area = [x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)];
    (area[2] > area[0] && area[3] > area[1]).then_some(area)
}

/// The resources `content` uses: the names given to the operators that look a resource up, each
/// in the category that operator looks in. Names inside an inline image's dictionary count as
/// colour spaces, and every name given to a marked-content operator as properties.
///
/// `left` is how many more different names may be kept, and goes down by those this content
/// uses; past it, or with more different names before one operator than [`MAX_NAMES`], this
/// fails with [`Error::TooLarge`].
fn uses_in(content: &[u8], left: &mut usize) -> Result<Uses> {
    let mut uses = HashSet::new();
    // Which operand a name is, or how often it is given, does not matter: each is kept once, so
    // a name given millions of times before an operator takes no more memory than one.
    let mut operands: HashSet<&[u8]> = HashSet::new();
    let mut over = false;
    tokens(content, |token| match token {
        _ if over => {}
        Token::Name(written) => {
            over = operands.insert(written) && operands.len() > MAX_NAMES;
        }
        Token::Operator(operator) => {
            let category = match operator {
                b"Tf" => Some(FONT),
                b"Do" => Some(XOBJECT),
                b"gs" => Some(EXT_G_STATE),
                b"cs" | b"CS" | b"ID" => Some(COLOR_SPACE),
                b"scn" | b"SCN" => Some(PATTERN),
                b"sh" => Some(SHADING),
                b"BDC" | b"DP" => Some(PROPERTIES),
                _ => None,
            };
            if let Some(category) = category {
                for written in &operands {
                    if uses.insert((category, name(written).into_owned())) {
                        match left.checked_sub(1) {
                            Some(fewer) => *left = fewer,
                            None => {
                                over = true;
                                break;
                            }
                        }
                    }
                }
            }
            operands.clear();
        }
        _ => {}
    });
    if over {
        return Err(Error::TooLarge(messages::TOO_COMPLEX.into()));
    }
    Ok(uses)
}

/// Whether `dict` is that of a stream that holds content: a form or a tiling pattern.
fn is_content(dict: &Dictionary) -> bool {
    dict.has(b"PatternType")
        || dict
            .get(b"Subtype")
            .and_then(Object::as_name)
            .is_ok_and(|s| s == b"Form")
}

/// Runs `edit` on the array `holder` is, or refers to, and stores the result where it came from.
/// `None` when `holder` is not an array.
fn edit_array<T>(
    doc: &mut Document,
    holder: &mut Object,
    edit: impl FnOnce(&mut Document, &mut Vec<Object>) -> T,
) -> Option<T> {
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
/// (`/FT`) the node inherits. A node met again (in `seen`: the tree loops, or lists a field twice)
/// is left as it is the second time.
fn prune_field(
    doc: &mut Document,
    item: &mut Object,
    scope: &WidgetScope<'_>,
    field_type: Option<&[u8]>,
    removed: &mut HashSet<ObjectId>,
    seen: &mut HashSet<ObjectId>,
    depth: usize,
) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    let (id, mut dict) = match item {
        Object::Reference(id) if !seen.insert(*id) => return false,
        Object::Reference(id) => match doc.get_dictionary(*id) {
            Ok(dict) => (Some(*id), dict.clone()),
            Err(_) => return false,
        },
        Object::Dictionary(dict) => (None, dict.clone()),
        _ => return false,
    };
    let field_type = dict
        .get(b"FT")
        .and_then(Object::as_name)
        .ok()
        .map(<[u8]>::to_vec)
        .or(field_type.map(<[u8]>::to_vec));
    if dict
        .get(b"Subtype")
        .and_then(Object::as_name)
        .is_ok_and(|s| s == b"Widget")
    {
        let page = dict
            .get(b"P")
            .and_then(Object::as_reference)
            .ok()
            .filter(|p| scope.pages.contains(p))
            .or_else(|| id.and_then(|id| scope.annotations.get(&id).copied()));
        if page.is_some_and(|page| (scope.goes)(page, &dict, field_type.as_deref() == Some(b"Sig")))
        {
            removed.extend(id);
            return true;
        }
    }
    let Ok(mut kids) = dict.get(b"Kids").cloned() else {
        return false;
    };
    let counts = edit_array(doc, &mut kids, |doc, items| {
        let before = items.len();
        items.retain_mut(|kid| {
            !prune_field(
                doc,
                kid,
                scope,
                field_type.as_deref(),
                removed,
                seen,
                depth + 1,
            )
        });
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
/// goes. `page` is the page the enclosing element is on. An element met again (in `seen`: the tree
/// loops, or lists an element twice) is left as it is the second time.
fn prune_structure(
    doc: &mut Document,
    item: &mut Object,
    page: Option<ObjectId>,
    scope: &Scope<'_>,
    removed: &mut HashSet<ObjectId>,
    seen: &mut HashSet<ObjectId>,
    depth: usize,
) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    if let Object::Array(items) = item {
        items.retain_mut(|kid| !prune_structure(doc, kid, page, scope, removed, seen, depth + 1));
        return false;
    }
    let (id, mut dict) = match item {
        Object::Reference(id) if !seen.insert(*id) => return false,
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
        let annotation = dict
            .get(b"Obj")
            .and_then(Object::as_reference)
            .is_ok_and(|o| scope.annotations.contains(&o));
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
        if prune_structure(doc, &mut kids, page, scope, removed, seen, depth + 1) {
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
                Ok(other @ Object::Array(_)) => {
                    return collect_elements(doc, other, removed, depth + 1)
                }
                _ => return,
            }
        }
        Object::Dictionary(dict) => dict,
        Object::Array(items) => {
            items
                .iter()
                .for_each(|kid| collect_elements(doc, kid, removed, depth + 1));
            return;
        }
        _ => return,
    };
    if let Ok(kids) = dict.get(b"K") {
        collect_elements(doc, kids, removed, depth + 1);
    }
}

/// Drops the entries of a number tree (`key` = `Nums`) or name tree (`Names`) that refer to
/// removed elements; in arrays of elements those become null. A node met again (in `seen`) is
/// already done.
fn prune_tree(
    doc: &mut Document,
    node: &mut Object,
    key: &[u8],
    removed: &HashSet<ObjectId>,
    seen: &mut HashSet<ObjectId>,
    depth: usize,
) {
    if depth > MAX_DEPTH {
        return;
    }
    let (id, mut dict) = match node {
        Object::Reference(id) if !seen.insert(*id) => return,
        Object::Reference(id) => match doc.get_dictionary(*id) {
            Ok(dict) => (Some(*id), dict.clone()),
            Err(_) => return,
        },
        Object::Dictionary(dict) => (None, dict.clone()),
        _ => return,
    };
    if let Ok(mut kids) = dict.get(b"Kids").cloned() {
        edit_array(doc, &mut kids, |doc, items| {
            items
                .iter_mut()
                .for_each(|kid| prune_tree(doc, kid, key, removed, seen, depth + 1))
        });
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
