//! Follow-up checks on what a save counts as removed content.

use carbide_engine::SavedFile;
use lopdf::{dictionary, Document, Object, ObjectId, Stream, StringFormat};

/// A one-page file with an ink annotation whose points sit in an array object of their own.
/// Returns the file's bytes, the annotation and the points' object.
fn ink_with_referenced_points() -> (Vec<u8>, ObjectId, ObjectId, ObjectId) {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let page = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 300.into(), 300.into()],
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(
            dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 },
        ),
    );
    let points = doc.add_object(Object::Array(
        [10, 10, 50, 50, 90, 10]
            .into_iter()
            .map(Object::from)
            .collect(),
    ));
    let ink = doc.add_object(dictionary! {
        "Type" => "Annot", "Subtype" => "Ink", "P" => page,
        "Rect" => vec![0.into(), 0.into(), 100.into(), 100.into()],
        "InkList" => vec![points.into()],
    });
    doc.get_dictionary_mut(page)
        .unwrap()
        .set("Annots", vec![ink.into()]);
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    (bytes, ink, points, page)
}

#[test]
fn ink_points_shortened_in_a_referenced_object_count_as_removed() {
    let (bytes, _, points, _) = ink_with_referenced_points();
    let (mut pdf, saved) = SavedFile::open(bytes, None).unwrap();
    assert!(!saved.removed_from(&pdf));
    pdf.document_mut().objects.insert(
        points,
        Object::Array([10, 10, 50, 50].into_iter().map(Object::from).collect()),
    );
    assert!(
        saved.removed_from(&pdf),
        "ink points removed from a referenced array went unnoticed"
    );
}

#[test]
fn ink_points_that_differ_only_past_what_a_real_holds_count_as_removed() {
    let (bytes, _, points, _) = ink_with_referenced_points();
    let (mut pdf, _) = SavedFile::open(bytes, None).unwrap();
    pdf.document_mut()
        .objects
        .insert(points, Object::Array(vec![16_777_217.into(), 10.into()]));
    let mut changed = Vec::new();
    pdf.document_mut().save_to(&mut changed).unwrap();
    let (mut pdf, saved) = SavedFile::open(changed, None).unwrap();
    pdf.document_mut()
        .objects
        .insert(points, Object::Array(vec![16_777_216.into(), 10.into()]));
    assert!(
        saved.removed_from(&pdf),
        "a point moved by one past what a real holds went unnoticed"
    );
}

#[test]
fn changes_to_the_page_an_annotation_sits_on_are_not_removals() {
    let (bytes, _, _, page) = ink_with_referenced_points();
    let (mut pdf, saved) = SavedFile::open(bytes, None).unwrap();
    pdf.document_mut()
        .get_dictionary_mut(page)
        .unwrap()
        .set("Rotate", 90);
    assert!(
        !saved.removed_from(&pdf),
        "rotating a page was taken for removed content"
    );
}

/// A one-page file with an ink annotation whose points sit at the end of a chain of `links`
/// array objects, each holding a reference to the next. Returns the file's bytes and the object
/// that holds the points.
fn ink_behind_a_reference_chain(links: usize) -> (Vec<u8>, ObjectId) {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let page = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 300.into(), 300.into()],
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(
            dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 },
        ),
    );
    let points = doc.add_object(Object::Array(
        [10, 10, 50, 50, 90, 10]
            .into_iter()
            .map(Object::from)
            .collect(),
    ));
    let mut next = points;
    for _ in 0..links {
        next = doc.add_object(Object::Array(vec![next.into()]));
    }
    let ink = doc.add_object(dictionary! {
        "Type" => "Annot", "Subtype" => "Ink", "P" => page,
        "Rect" => vec![0.into(), 0.into(), 100.into(), 100.into()],
        "InkList" => vec![next.into()],
    });
    doc.get_dictionary_mut(page)
        .unwrap()
        .set("Annots", vec![ink.into()]);
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    (bytes, points)
}

#[test]
fn ink_points_changed_beyond_the_reference_depth_limit_count_as_removed() {
    let (bytes, points) = ink_behind_a_reference_chain(12);
    let (mut pdf, saved) = SavedFile::open(bytes, None).unwrap();
    pdf.document_mut().objects.insert(
        points,
        Object::Array([10, 10, 50, 50].into_iter().map(Object::from).collect()),
    );
    assert!(
        saved.removed_from(&pdf),
        "ink points removed behind a long reference chain went unnoticed"
    );
}

/// A one-page file whose ink annotation points at the page, or at the page tree, under `key`.
/// `key` is not one of the names that lead out of an annotation. Returns the file, the
/// annotation, the page and the page tree.
fn ink_linked(key: &str, to_pages: bool) -> (Vec<u8>, ObjectId, ObjectId, ObjectId) {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let page = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 300.into(), 300.into()],
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page.into()],
            "Count" => 1,
        }),
    );
    let points = doc.add_object(Object::Array(
        [10, 10, 50, 50, 90, 10]
            .into_iter()
            .map(Object::from)
            .collect(),
    ));
    let mut ink_dict = dictionary! {
        "Type" => "Annot",
        "Subtype" => "Ink",
        "P" => page,
        "Rect" => vec![0.into(), 0.into(), 100.into(), 100.into()],
        "InkList" => vec![points.into()],
    };
    ink_dict.set(key, if to_pages { pages_id } else { page });
    let ink = doc.add_object(ink_dict);
    doc.get_dictionary_mut(page)
        .unwrap()
        .set("Annots", vec![ink.into()]);
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    (bytes, ink, page, pages_id)
}

fn nudge_rectangle(doc: &mut Document, ink: ObjectId) {
    doc.get_dictionary_mut(ink)
        .unwrap()
        .get_mut(b"Rect")
        .unwrap()
        .as_array_mut()
        .unwrap()[2] = 80.into();
}

#[test]
fn rotating_a_page_the_annotation_points_at_is_not_removed_content() {
    // /See is not one of the names left out of the comparison, and it points at the
    // page. Rotating that page is not removed content. Dropping the exclusion of a
    // page dictionary takes the rotation for a change to the stroke. The older
    // rotation check names the page with /P, which is left out by name, so it stays
    // green. Moving the rectangle is removed content.
    let (bytes, ink, page, _) = ink_linked("See", false);
    let (mut pdf, saved) = SavedFile::open(bytes, None).unwrap();
    let see = pdf
        .document()
        .get_dictionary(ink)
        .unwrap()
        .get(b"See")
        .unwrap()
        .as_reference()
        .unwrap();
    assert_eq!(see, page);
    assert_eq!(
        pdf.document()
            .get_dictionary(page)
            .unwrap()
            .get(b"Type")
            .unwrap()
            .as_name()
            .unwrap(),
        b"Page"
    );
    assert!(!saved.removed_from(&pdf));
    pdf.document_mut()
        .get_dictionary_mut(page)
        .unwrap()
        .set("Rotate", 90);
    assert!(
        !saved.removed_from(&pdf),
        "rotating a page named by the annotation was taken for removed content"
    );
    nudge_rectangle(pdf.document_mut(), ink);
    assert!(
        saved.removed_from(&pdf),
        "moving the rectangle went unnoticed"
    );
}

#[test]
fn a_page_tree_an_annotation_names_is_not_removed_content() {
    // /Tree points at the page tree. Changing its count is not removed content.
    // Dropping only the page-tree exclusion leaves the page check green. Moving the
    // rectangle is removed content.
    let (bytes, ink, _, pages) = ink_linked("Tree", true);
    let (mut pdf, saved) = SavedFile::open(bytes, None).unwrap();
    let tree = pdf
        .document()
        .get_dictionary(ink)
        .unwrap()
        .get(b"Tree")
        .unwrap()
        .as_reference()
        .unwrap();
    assert_eq!(tree, pages);
    assert_eq!(
        pdf.document()
            .get_dictionary(pages)
            .unwrap()
            .get(b"Type")
            .unwrap()
            .as_name()
            .unwrap(),
        b"Pages"
    );
    assert_eq!(
        pdf.document()
            .get_dictionary(pages)
            .unwrap()
            .get(b"Count")
            .unwrap()
            .as_i64()
            .unwrap(),
        1
    );
    assert!(!saved.removed_from(&pdf));
    pdf.document_mut()
        .get_dictionary_mut(pages)
        .unwrap()
        .set("Count", 2);
    assert!(
        !saved.removed_from(&pdf),
        "a page tree named by the annotation was taken for removed content"
    );
    nudge_rectangle(pdf.document_mut(), ink);
    assert!(
        saved.removed_from(&pdf),
        "moving the rectangle went unnoticed"
    );
}

#[test]
fn reordering_an_annotations_entries_is_not_removed_content() {
    // The entries are stored in the opposite order. That is not removed content:
    // they are compared by key. Moving the rectangle still is. The ink-point checks
    // do not reorder a dictionary.
    let (bytes, ink, _, _) = ink_with_referenced_points();
    let (mut pdf, saved) = SavedFile::open(bytes, None).unwrap();
    let dict = pdf.document_mut().get_dictionary_mut(ink).unwrap();
    let before = drawing_keys(dict);
    let reversed: Vec<_> = dict
        .as_hashmap()
        .iter()
        .rev()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    {
        let map = dict.as_hashmap_mut();
        map.clear();
        for (key, value) in reversed {
            map.insert(key, value);
        }
    }
    let after = drawing_keys(dict);
    assert_ne!(before, after, "the entries were not reordered");
    assert!(before.len() >= 2, "{before:?}");
    assert!(
        !saved.removed_from(&pdf),
        "reordering the entries was taken for removed content"
    );
    nudge_rectangle(pdf.document_mut(), ink);
    assert!(
        saved.removed_from(&pdf),
        "moving the rectangle went unnoticed"
    );
}

fn drawing_keys(dict: &lopdf::Dictionary) -> Vec<Vec<u8>> {
    const STRUCTURAL: [&[u8]; 8] = [
        b"P", b"Parent", b"Popup", b"IRT", b"Dest", b"Next", b"Prev", b"Kids",
    ];
    dict.as_hashmap()
        .keys()
        .filter(|key| !STRUCTURAL.contains(&key.as_slice()))
        .cloned()
        .collect()
}

#[test]
fn ink_points_in_a_different_order_count_as_removed() {
    // The same points in the other order are a different stroke. Sorting them, or
    // keeping only how many there are, leaves the swap unnoticed. Shortening the
    // list still fails the older check, because that changes which points are there.
    let (bytes, _, points, _) = ink_with_referenced_points();
    let (mut pdf, saved) = SavedFile::open(bytes, None).unwrap();
    let listed = pdf
        .document_mut()
        .get_object_mut(points)
        .unwrap()
        .as_array_mut()
        .unwrap();
    assert_eq!(
        listed,
        &vec![
            10.into(),
            10.into(),
            50.into(),
            50.into(),
            90.into(),
            10.into()
        ]
    );
    listed.swap(0, 4);
    assert_eq!(
        listed,
        &vec![
            90.into(),
            10.into(),
            50.into(),
            50.into(),
            10.into(),
            10.into()
        ]
    );
    assert!(
        saved.removed_from(&pdf),
        "ink points swapped into the other order went unnoticed"
    );
}

/// A one-page file whose ink list points back at the annotation. Returns the file, the list and
/// the annotation.
fn ink_that_refers_to_itself() -> (Vec<u8>, ObjectId, ObjectId) {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let page = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 300.into(), 300.into()],
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page.into()],
            "Count" => 1,
        }),
    );
    let ink = doc.new_object_id();
    let points = doc.add_object(Object::Array(vec![
        10.into(),
        10.into(),
        50.into(),
        90.into(),
        Object::Reference(ink),
    ]));
    doc.objects.insert(
        ink,
        Object::Dictionary(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Ink",
            "P" => page,
            "Rect" => vec![0.into(), 0.into(), 100.into(), 100.into()],
            "InkList" => points,
        }),
    );
    doc.get_dictionary_mut(page)
        .unwrap()
        .set("Annots", vec![ink.into()]);
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    (bytes, points, ink)
}

#[test]
fn an_annotation_that_refers_to_itself_is_not_a_change() {
    // The ink list points back at the annotation. That loop is not a change, and the
    // comparison returns. Following the loop without remembering it never matches, so
    // an unchanged file would count as removed. The older depth check is a chain, not
    // a loop, and does not look at the unchanged file. Shortening the list is still
    // removed content.
    let (bytes, points, ink) = ink_that_refers_to_itself();
    let (mut pdf, saved) = SavedFile::open(bytes, None).unwrap();
    let listed = pdf
        .document()
        .get_object(points)
        .unwrap()
        .as_array()
        .unwrap();
    assert_eq!(listed.last().unwrap().as_reference().unwrap(), ink);
    assert!(
        !saved.removed_from(&pdf),
        "a loop in the ink list was taken for removed content"
    );
    pdf.document_mut().objects.insert(
        points,
        Object::Array(vec![10.into(), Object::Reference(ink)]),
    );
    assert!(
        saved.removed_from(&pdf),
        "ink points removed from a list that points back at the annotation went unnoticed"
    );
}

/// A one-page file whose content stream carries the string `keep`. Returns the file and the stream.
fn page_with_note() -> (Vec<u8>, ObjectId) {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let content = doc.add_object(Stream::new(
        dictionary! { "Note" => Object::string_literal("keep") },
        b"q Q".to_vec(),
    ));
    let page = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 300.into(), 300.into()],
        "Contents" => content,
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page.into()],
            "Count" => 1,
        }),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    (bytes, content)
}

fn note(doc: &mut Document, content: ObjectId) -> &mut Object {
    match doc.get_object_mut(content).unwrap() {
        Object::Stream(stream) => stream.dict.get_mut(b"Note").unwrap(),
        other => panic!("contents were {other:?}, not a stream"),
    }
}

#[test]
fn an_added_string_is_not_removed_content() {
    // A second string on the content stream is not removed content. Added strings
    // do not count. Replacing the original with another string of the same length
    // does. Hashing a string by its length leaves that replacement unnoticed. The
    // ink checks have no strings.
    let (bytes, content) = page_with_note();
    let (mut pdf, saved) = SavedFile::open(bytes, None).unwrap();
    assert!(!saved.removed_from(&pdf));
    {
        let Object::Stream(stream) = pdf.document_mut().get_object_mut(content).unwrap() else {
            panic!("contents were not a stream");
        };
        stream.dict.set("Also", Object::string_literal("extra"));
    }
    assert!(
        !saved.removed_from(&pdf),
        "a string added to the content stream was taken for removed content"
    );
    match note(pdf.document_mut(), content) {
        Object::String(bytes, _) => {
            assert_eq!(bytes, b"keep");
            bytes.clear();
            bytes.extend_from_slice(b"kept");
        }
        other => panic!("note was {other:?}"),
    }
    assert!(
        saved.removed_from(&pdf),
        "a string replaced by one of the same length went unnoticed"
    );
}

#[test]
fn rewriting_a_string_in_the_other_format_is_not_removed_content() {
    // The same bytes written in the other string format are not removed content.
    // Hashing the format leaves the rewrite noticed. Removing the string is
    // removed content, so the format check is not vacuous. The same-length
    // replacement is a different check.
    let (bytes, content) = page_with_note();
    let (mut pdf, saved) = SavedFile::open(bytes, None).unwrap();
    assert!(!saved.removed_from(&pdf));
    match note(pdf.document_mut(), content) {
        Object::String(text, format) => {
            assert_eq!(text, b"keep");
            *format = match *format {
                StringFormat::Literal => StringFormat::Hexadecimal,
                StringFormat::Hexadecimal => StringFormat::Literal,
            };
        }
        other => panic!("note was {other:?}"),
    }
    assert!(
        !saved.removed_from(&pdf),
        "rewriting a string in the other format was taken for removed content"
    );
    match pdf.document_mut().get_object_mut(content).unwrap() {
        Object::Stream(stream) => {
            assert!(stream.dict.remove(b"Note").is_some());
        }
        other => panic!("contents were {other:?}"),
    }
    assert!(
        saved.removed_from(&pdf),
        "removing the string went unnoticed"
    );
}
