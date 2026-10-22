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
