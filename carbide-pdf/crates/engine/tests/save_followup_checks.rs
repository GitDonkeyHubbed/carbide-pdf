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

/// True when an ink list of `before`, saved and opened, no longer matches `after`.
fn replacing_points_counts_as_removed(before: Vec<Object>, after: Vec<Object>) -> bool {
    let (bytes, _, points, _) = ink_with_referenced_points();
    let (mut pdf, _) = SavedFile::open(bytes, None).unwrap();
    pdf.document_mut()
        .objects
        .insert(points, Object::Array(before));
    let mut changed = Vec::new();
    pdf.document_mut().save_to(&mut changed).unwrap();
    let (mut pdf, saved) = SavedFile::open(changed, None).unwrap();
    pdf.document_mut()
        .objects
        .insert(points, Object::Array(after));
    saved.removed_from(&pdf)
}

#[test]
fn ink_points_past_what_a_double_holds_count_as_removed() {
    // 16777217 and 16777216 are different double-precision numbers, so rounding every
    // integer through a double still passes that test. 2^53 + 1 is not a different
    // double from 2^53, and moving the point by that one must still count.
    const EXACT: i64 = 9_007_199_254_740_992;
    assert!(
        replacing_points_counts_as_removed(
            vec![(EXACT + 1).into(), 10.into()],
            vec![EXACT.into(), 10.into()]
        ),
        "a point moved by one past what a double holds went unnoticed"
    );
}

#[test]
fn whole_reals_match_integers_only_inside_the_range_an_integer_can_hold() {
    // A whole real is the same point as that integer, until it is too large to be one.
    // Past that, its cast is the largest integer, and treating it as that integer
    // would hide the difference.
    let Object::Real(within) = Object::Real(1.0e18) else {
        unreachable!("a real stays a real");
    };
    assert!(
        within.fract() == 0.0 && within.abs() < 9.0e18,
        "{within} is not a whole number inside the integer range"
    );
    assert!(
        !replacing_points_counts_as_removed(
            vec![(within as i64).into()],
            vec![Object::Real(within)]
        ),
        "a whole real inside the integer range was not the same point"
    );

    // Used only in memory. Writing this real out and reading it back is not what this checks.
    let Object::Real(too_big) = Object::Real(1.0e20) else {
        unreachable!("a real stays a real");
    };
    assert!(too_big.fract() == 0.0 && too_big.abs() >= 9.0e18);
    assert_eq!(too_big as i64, i64::MAX);
    assert!(
        replacing_points_counts_as_removed(vec![i64::MAX.into()], vec![Object::Real(too_big)]),
        "a real past the integer range was taken as the largest integer"
    );
}

#[test]
fn changes_under_an_annotations_structural_keys_are_not_removals() {
    // The page an annotation names is already left out of the comparison, because a
    // page dictionary is not drawing data. These keys are left out by name: a number
    // stored under one of them is not part of the annotation, and changing it is not
    // content removed from the file. Changing the rectangle is.
    let keys: [&[u8]; 8] = [
        b"P", b"Parent", b"Popup", b"IRT", b"Dest", b"Next", b"Prev", b"Kids",
    ];
    let (bytes, ink, _, _) = ink_with_referenced_points();
    let (mut pdf, _) = SavedFile::open(bytes, None).unwrap();
    {
        let dict = pdf.document_mut().get_dictionary_mut(ink).unwrap();
        for key in keys {
            dict.set(key.to_vec(), Object::Integer(1));
        }
    }
    let mut changed = Vec::new();
    pdf.document_mut().save_to(&mut changed).unwrap();

    for key in keys {
        let (mut pdf, saved) = SavedFile::open(changed.clone(), None).unwrap();
        pdf.document_mut()
            .get_dictionary_mut(ink)
            .unwrap()
            .set(key.to_vec(), Object::Integer(2));
        assert!(
            !saved.removed_from(&pdf),
            "{} was taken for removed content",
            String::from_utf8_lossy(key)
        );
    }

    let (mut pdf, saved) = SavedFile::open(changed, None).unwrap();
    let rect = pdf
        .document_mut()
        .get_dictionary_mut(ink)
        .unwrap()
        .get_mut(b"Rect")
        .unwrap()
        .as_array_mut()
        .unwrap();
    rect[0] = 5.into();
    assert!(
        saved.removed_from(&pdf),
        "moving the rectangle went unnoticed"
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
