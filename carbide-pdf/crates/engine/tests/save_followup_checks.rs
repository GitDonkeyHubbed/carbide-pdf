//! Follow-up checks on what a save counts as removed content.

use carbide_engine::SavedFile;
use lopdf::{dictionary, Document, Object, ObjectId, Stream};

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

/// A one-page file. `build` adds the objects the test checks and returns whatever it needs later.
fn one_page<T>(build: impl FnOnce(&mut Document, ObjectId) -> T) -> (Vec<u8>, T) {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let page = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
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
    let built = build(&mut doc, page);
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    (bytes, built)
}

#[test]
fn an_ink_annotation_without_a_type_still_counts_a_removed_point() {
    // No /Type. A name /Subtype and a /Rect are what make the stroke content.
    // Requiring /Type leaves the shortened stroke unnoticed. The typed ink
    // tests still pass, because those annotations name their type.
    let (bytes, ink) = one_page(|doc, page| {
        let ink = doc.add_object(dictionary! {
            "Subtype" => "Ink",
            "Rect" => vec![0.into(), 0.into(), 100.into(), 100.into()],
            "InkList" => vec![Object::Array(
                [10, 10, 50, 50, 90, 10].into_iter().map(Object::from).collect(),
            )],
        });
        doc.get_dictionary_mut(page)
            .unwrap()
            .set("Annots", vec![ink.into()]);
        ink
    });
    let (mut pdf, saved) = SavedFile::open(bytes, None).unwrap();
    let dict = pdf.document().get_dictionary(ink).unwrap();
    assert!(
        !dict.has(b"Type"),
        "the ink annotation gained a type, so this no longer tests the subtype rule"
    );
    assert!(!saved.removed_from(&pdf));
    pdf.document_mut().get_dictionary_mut(ink).unwrap().set(
        "InkList",
        vec![Object::Array(
            [10, 10, 50, 50].into_iter().map(Object::from).collect(),
        )],
    );
    assert!(
        saved.removed_from(&pdf),
        "a shortened ink stroke without /Type went unnoticed"
    );
}

#[test]
fn a_rectangle_whose_subtype_is_text_is_not_drawing_content() {
    // /Subtype is the text (Ink), not the name /Ink. A changed number is not
    // a removed stroke. Treating every subtype as a name fails only this case.
    let (bytes, id) = one_page(|doc, page| {
        let id = doc.add_object(dictionary! {
            "Subtype" => Object::string_literal("Ink"),
            "Rect" => vec![0.into(), 0.into(), 10.into(), 10.into()],
            "W" => 1,
            "Note" => Object::string_literal("keep"),
        });
        doc.get_dictionary_mut(page)
            .unwrap()
            .set("Annots", vec![id.into()]);
        id
    });
    let (mut pdf, saved) = SavedFile::open(bytes, None).unwrap();
    let subtype = pdf
        .document()
        .get_dictionary(id)
        .unwrap()
        .get(b"Subtype")
        .unwrap();
    assert!(subtype.as_str().is_ok(), "{subtype:?}");
    assert!(subtype.as_name().is_err(), "{subtype:?}");
    assert!(!saved.removed_from(&pdf));
    pdf.document_mut()
        .get_dictionary_mut(id)
        .unwrap()
        .set("W", 2);
    assert!(
        !saved.removed_from(&pdf),
        "a text subtype was read as an annotation name"
    );
    pdf.document_mut()
        .get_dictionary_mut(id)
        .unwrap()
        .remove(b"Note");
    assert!(
        saved.removed_from(&pdf),
        "the text was never recorded, so the number check proved nothing"
    );
}

#[test]
fn a_font_is_not_drawing_content() {
    // A font has a name subtype and no rectangle. Changing its flags is not
    // removed drawing. Dropping the rectangle requirement fails only this case.
    let (bytes, font) = one_page(|doc, page| {
        let font = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
            "Flags" => 32,
            "Note" => Object::string_literal("keep"),
        });
        doc.get_dictionary_mut(page).unwrap().set(
            "Resources",
            dictionary! { "Font" => dictionary! { "F" => font } },
        );
        font
    });
    let (mut pdf, saved) = SavedFile::open(bytes, None).unwrap();
    let dict = pdf.document().get_dictionary(font).unwrap();
    assert!(!dict.has(b"Rect"));
    assert_eq!(dict.get(b"Subtype").unwrap().as_name().unwrap(), b"Type1");
    assert!(!saved.removed_from(&pdf));
    pdf.document_mut()
        .get_dictionary_mut(font)
        .unwrap()
        .set("Flags", 33);
    assert!(
        !saved.removed_from(&pdf),
        "a font was treated as an annotation"
    );
    pdf.document_mut()
        .get_dictionary_mut(font)
        .unwrap()
        .remove(b"Note");
    assert!(
        saved.removed_from(&pdf),
        "the font text was never recorded, so the flag check proved nothing"
    );
}

#[test]
fn closing_a_popup_counts_as_removed_content() {
    // /Open is a boolean. true and false are different drawings. Hashing
    // every boolean the same leaves the closed popup unnoticed. The ink
    // tests change numbers only, so they still pass.
    let (bytes, popup) = one_page(|doc, page| {
        let popup = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Popup",
            "Rect" => vec![0.into(), 0.into(), 10.into(), 10.into()],
            "Open" => true,
        });
        doc.get_dictionary_mut(page)
            .unwrap()
            .set("Annots", vec![popup.into()]);
        popup
    });
    let (mut pdf, saved) = SavedFile::open(bytes, None).unwrap();
    assert!(pdf
        .document()
        .get_dictionary(popup)
        .unwrap()
        .get(b"Open")
        .unwrap()
        .as_bool()
        .unwrap());
    assert!(!saved.removed_from(&pdf));
    pdf.document_mut()
        .get_dictionary_mut(popup)
        .unwrap()
        .set("Open", false);
    assert!(saved.removed_from(&pdf), "closing a popup went unnoticed");
}

#[test]
fn renaming_a_stamp_counts_as_removed_content() {
    // /Name /Draft and /Final are the same length. Hashing every name the
    // same, or hashing only its length, leaves the new name unnoticed.
    // The ink tests have no names.
    let (bytes, stamp) = one_page(|doc, page| {
        let stamp = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Stamp",
            "Rect" => vec![0.into(), 0.into(), 10.into(), 10.into()],
            "Name" => "Draft",
        });
        doc.get_dictionary_mut(page)
            .unwrap()
            .set("Annots", vec![stamp.into()]);
        stamp
    });
    let (mut pdf, saved) = SavedFile::open(bytes, None).unwrap();
    assert_eq!(
        pdf.document()
            .get_dictionary(stamp)
            .unwrap()
            .get(b"Name")
            .unwrap()
            .as_name()
            .unwrap(),
        b"Draft"
    );
    assert!(!saved.removed_from(&pdf));
    pdf.document_mut()
        .get_dictionary_mut(stamp)
        .unwrap()
        .set("Name", Object::Name(b"Final".to_vec()));
    assert!(saved.removed_from(&pdf), "renaming a stamp went unnoticed");
}

#[test]
fn a_changed_appearance_stream_counts_as_removed_content() {
    // The drawn bytes change and the length does not. Hashing the stream
    // dictionary, or only its length, leaves the new drawing unnoticed.
    // The ink tests have no stream.
    let (bytes, appearance) = one_page(|doc, page| {
        let appearance = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Form",
                "BBox" => vec![0.into(), 0.into(), 10.into(), 10.into()],
            },
            b"0 0 1 rg".to_vec(),
        ));
        let stamp = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Stamp",
            "Rect" => vec![0.into(), 0.into(), 10.into(), 10.into()],
            "AP" => dictionary! { "N" => appearance },
        });
        doc.get_dictionary_mut(page)
            .unwrap()
            .set("Annots", vec![stamp.into()]);
        appearance
    });
    let (mut pdf, saved) = SavedFile::open(bytes, None).unwrap();
    {
        let stream = pdf.document_mut().get_object_mut(appearance).unwrap();
        let Object::Stream(stream) = stream else {
            panic!("appearance was not a stream");
        };
        assert_eq!(stream.content, b"0 0 1 rg");
    }
    assert!(!saved.removed_from(&pdf));
    let stream = pdf.document_mut().get_object_mut(appearance).unwrap();
    let Object::Stream(stream) = stream else {
        panic!("appearance was not a stream");
    };
    stream.content = b"0 0 0 rg".to_vec();
    assert!(
        saved.removed_from(&pdf),
        "a changed appearance drawing went unnoticed"
    );
}
