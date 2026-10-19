//! Follow-up checks on what a save counts as removed content.

use carbide_engine::{SavePlan, SavedFile};
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

#[test]
fn an_unchanged_deep_annotation_is_not_removed_content() {
    let (bytes, _) = ink_behind_a_reference_chain(12);
    let (pdf, saved) = SavedFile::open(bytes, None).unwrap();
    assert!(
        !saved.removed_from(&pdf),
        "an unchanged annotation behind a long reference chain was taken for removed content"
    );
}

#[test]
fn a_deep_annotation_does_not_force_a_rewrite_that_drops_revisions() {
    let (bytes, _) = ink_behind_a_reference_chain(12);
    let (pdf, _) = SavedFile::open(bytes.clone(), None).unwrap();
    let revised = pdf
        .save_revision(&bytes, "First", "D:20261019110000+00'00'")
        .unwrap();
    let (mut pdf, saved) = SavedFile::open(revised, None).unwrap();
    let page = pdf.page_id(0).unwrap();
    pdf.document_mut()
        .get_dictionary_mut(page)
        .unwrap()
        .set("Rotate", 90);
    assert!(
        !saved.removed_from(&pdf),
        "rotating a page was taken for removed content because an annotation is deeply nested"
    );
    assert_eq!(
        pdf.save_plan(Some(&saved), false).unwrap(),
        SavePlan::Incremental,
        "a page rotation was saved as a full rewrite that drops the earlier revision"
    );
}
