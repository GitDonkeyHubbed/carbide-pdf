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
