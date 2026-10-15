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
