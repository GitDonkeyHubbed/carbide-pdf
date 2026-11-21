//! Follow-up checks on what a save counts as removed content.

use carbide_engine::SavedFile;
use lopdf::{dictionary, Document, Object, ObjectId};

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

/// A one-page document. The catalog points at the page tree.
fn one_page() -> (Document, ObjectId) {
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
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    (doc, page)
}

fn saved_bytes(doc: &mut Document) -> Vec<u8> {
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

/// Ink whose list names the same points object twice. Returns the file, the annotation and the points.
fn ink_with_repeated_points() -> (Vec<u8>, ObjectId, ObjectId) {
    let (mut doc, page) = one_page();
    let points = doc.add_object(Object::Array(
        [10, 10, 50, 50, 90, 10]
            .into_iter()
            .map(Object::from)
            .collect(),
    ));
    let ink = doc.add_object(dictionary! {
        "Type" => "Annot", "Subtype" => "Ink", "P" => page,
        "Rect" => vec![0.into(), 0.into(), 100.into(), 100.into()],
        "InkList" => vec![Object::Reference(points), Object::Reference(points)],
    });
    doc.get_dictionary_mut(page)
        .unwrap()
        .set("Annots", vec![ink.into()]);
    (saved_bytes(&mut doc), ink, points)
}

#[test]
fn dropping_a_repeated_reference_to_ink_points_counts_as_removed() {
    // The ink list names one points object twice. That is not a change. Replacing the
    // second name with null is: one of the two strokes is gone. The walk remembers an
    // object only while it is inside it, so the second name is compared too. Leaving
    // the object marked after the first visit makes that name look like null, and the
    // removal goes unnoticed. The older chain is a line of different arrays, and the
    // older shortening replaces the array, so both still pass when the second visit is
    // skipped.
    let (bytes, ink, points) = ink_with_repeated_points();
    let (mut pdf, saved) = SavedFile::open(bytes, None).unwrap();
    let list = pdf
        .document()
        .get_dictionary(ink)
        .unwrap()
        .get(b"InkList")
        .unwrap()
        .as_array()
        .unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].as_reference().unwrap(), points);
    assert_eq!(list[1].as_reference().unwrap(), points);
    assert!(
        !saved.removed_from(&pdf),
        "naming the same ink points twice was taken for removed content"
    );
    pdf.document_mut()
        .get_dictionary_mut(ink)
        .unwrap()
        .get_mut(b"InkList")
        .unwrap()
        .as_array_mut()
        .unwrap()[1] = Object::Null;
    assert!(
        saved.removed_from(&pdf),
        "dropping one of two references to the same ink points went unnoticed"
    );
}

/// Ink whose first coordinate is an object of its own. Returns the file, the points array and
/// that coordinate.
fn ink_with_indirect_coordinate() -> (Vec<u8>, ObjectId, ObjectId) {
    let (mut doc, page) = one_page();
    let coord = doc.add_object(Object::Integer(10));
    let points = doc.add_object(Object::Array(vec![
        Object::Reference(coord),
        10.into(),
        50.into(),
        50.into(),
        90.into(),
        10.into(),
    ]));
    let ink = doc.add_object(dictionary! {
        "Type" => "Annot", "Subtype" => "Ink", "P" => page,
        "Rect" => vec![0.into(), 0.into(), 100.into(), 100.into()],
        "InkList" => vec![points.into()],
    });
    doc.get_dictionary_mut(page)
        .unwrap()
        .set("Annots", vec![ink.into()]);
    (saved_bytes(&mut doc), points, coord)
}

#[test]
fn an_indirect_ink_coordinate_counts_as_removed() {
    // The first coordinate is an integer object. Changing it is removed content.
    // Following a reference only when it leads to an array leaves the number unread:
    // the points array still holds the same reference. The older shortening replaces
    // that array, so it still passes.
    let (bytes, points, coord) = ink_with_indirect_coordinate();
    let (mut pdf, saved) = SavedFile::open(bytes, None).unwrap();
    let listed = pdf
        .document()
        .get_object(points)
        .unwrap()
        .as_array()
        .unwrap();
    assert_eq!(listed[0].as_reference().unwrap(), coord);
    assert_eq!(
        pdf.document().get_object(coord).unwrap().as_i64().unwrap(),
        10
    );
    assert!(!saved.removed_from(&pdf));
    pdf.document_mut()
        .objects
        .insert(coord, Object::Integer(11));
    assert!(
        saved.removed_from(&pdf),
        "an ink coordinate changed in its own object went unnoticed"
    );
}

/// Ink whose points are the real number 10.5. Returns the file and the points object.
fn ink_with_fractional_points() -> (Vec<u8>, ObjectId) {
    let (mut doc, page) = one_page();
    let points = doc.add_object(Object::Array(vec![Object::Real(10.5), Object::Real(10.5)]));
    let ink = doc.add_object(dictionary! {
        "Type" => "Annot", "Subtype" => "Ink", "P" => page,
        "Rect" => vec![0.into(), 0.into(), 100.into(), 100.into()],
        "InkList" => vec![points.into()],
    });
    doc.get_dictionary_mut(page)
        .unwrap()
        .set("Annots", vec![ink.into()]);
    (saved_bytes(&mut doc), points)
}

#[test]
fn the_sign_of_a_fractional_ink_point_counts_as_removed() {
    // 10.5 and -10.5 are different points. Hashing the bits of a real's absolute value
    // leaves the sign change unnoticed. The older check moves an integer by one, so it
    // still passes.
    let (bytes, points) = ink_with_fractional_points();
    let (mut pdf, saved) = SavedFile::open(bytes, None).unwrap();
    let listed = pdf
        .document_mut()
        .get_object_mut(points)
        .unwrap()
        .as_array_mut()
        .unwrap();
    assert!(
        listed
            .iter()
            .all(|point| matches!(point, Object::Real(value) if *value == 10.5)),
        "points were {listed:?}, not the real 10.5"
    );
    listed[0] = Object::Real(-10.5);
    assert!(
        saved.removed_from(&pdf),
        "the sign of a fractional ink point went unnoticed"
    );
}

/// A page whose `/Notes` array holds another array, and that one holds the string `keep`.
/// Returns the file and the page.
fn page_with_nested_note() -> (Vec<u8>, ObjectId) {
    let (mut doc, page) = one_page();
    doc.get_dictionary_mut(page).unwrap().set(
        "Notes",
        Object::Array(vec![Object::Array(vec![Object::string_literal("keep")])]),
    );
    (saved_bytes(&mut doc), page)
}

#[test]
fn a_string_nested_in_arrays_counts_as_removed() {
    // The only string sits two arrays down. Replacing it is removed content. Reading
    // strings that are direct values, or only one array down, leaves the new text
    // unnoticed. The ink checks have no strings.
    let (bytes, page) = page_with_nested_note();
    let (mut pdf, saved) = SavedFile::open(bytes, None).unwrap();
    assert!(!saved.removed_from(&pdf));
    let note = pdf
        .document_mut()
        .get_dictionary_mut(page)
        .unwrap()
        .get_mut(b"Notes")
        .unwrap()
        .as_array_mut()
        .unwrap();
    let inner = note[0].as_array_mut().unwrap();
    match &inner[0] {
        Object::String(text, _) => assert_eq!(text, b"keep"),
        other => panic!("note was {other:?}"),
    }
    inner[0] = Object::string_literal("no");
    assert!(
        saved.removed_from(&pdf),
        "a string nested in arrays went unnoticed when it was replaced"
    );
}
