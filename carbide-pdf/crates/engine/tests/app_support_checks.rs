//! Checks for what the application and the command line rely on the engine for.

use carbide_engine::Pdf;
use lopdf::{dictionary, Object};

/// A document of `pages` blank pages.
fn blank(pages: usize) -> Pdf {
    let mut pdf = Pdf::new();
    for i in 0..pages {
        pdf.insert_blank_page(i, Some((612.0, 792.0))).unwrap();
    }
    pdf
}

/// The page labels as (first page, style, first number) per range, in page order.
fn label_ranges(pdf: &Pdf) -> Vec<(i64, String, i64)> {
    label_entries(pdf)
        .into_iter()
        .map(|(start, style, _prefix, number)| (start, style, number))
        .collect()
}

/// The page labels as (first page, style, prefix, first number) per range, in page order.
fn label_entries(pdf: &Pdf) -> Vec<(i64, String, String, i64)> {
    let catalog = pdf.document().catalog().unwrap();
    let nums = catalog
        .get(b"PageLabels")
        .unwrap()
        .as_dict()
        .unwrap()
        .get(b"Nums")
        .unwrap()
        .as_array()
        .unwrap();
    nums.chunks(2)
        .map(|pair| {
            let style = pair[1].as_dict().unwrap();
            let prefix = style
                .get(b"P")
                .ok()
                .and_then(|object| object.as_str().ok())
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
                .unwrap_or_default();
            (
                pair[0].as_i64().unwrap(),
                style
                    .get(b"S")
                    .map(|name| String::from_utf8_lossy(name.as_name().unwrap()).into_owned())
                    .unwrap_or_default(),
                prefix,
                style.get(b"St").and_then(Object::as_i64).unwrap_or(1),
            )
        })
        .collect()
}

#[test]
fn a_moved_page_keeps_its_page_label() {
    // i, ii, 1, 2
    let mut pdf = blank(4);
    pdf.document_mut().catalog_mut().unwrap().set(
        "PageLabels",
        dictionary! { "Nums" => vec![
            0.into(), dictionary! { "S" => "r" }.into(),
            2.into(), dictionary! { "S" => "D" }.into(),
        ] },
    );
    assert_eq!(
        label_ranges(&pdf),
        vec![(0, "r".into(), 1), (2, "D".into(), 1)]
    );

    // The last page, "2", goes first: 2, i, ii, 1.
    pdf.move_page(3, 0).unwrap();
    assert_eq!(
        label_ranges(&pdf),
        vec![(0, "D".into(), 2), (1, "r".into(), 1), (3, "D".into(), 1)]
    );

    // And back again: i, ii, 1, 2.
    pdf.move_page(0, 3).unwrap();
    assert_eq!(
        label_ranges(&pdf),
        vec![(0, "r".into(), 1), (2, "D".into(), 1)]
    );
}

#[test]
fn a_moved_page_keeps_labels_stored_in_a_child_node() {
    // The same i, ii, 1, 2 as the older move, but the ranges live in a child and the
    // list of children is its own object. Moving the last page to the front still
    // leaves 2, i, ii, 1. The older move reads numbers written on the catalog itself.
    let mut pdf = blank(4);
    let doc = pdf.document_mut();
    let leaf = doc.add_object(dictionary! {
        "Nums" => vec![
            0.into(), dictionary! { "S" => "r" }.into(),
            2.into(), dictionary! { "S" => "D" }.into(),
        ]
    });
    let kids = doc.add_object(Object::Array(vec![Object::Reference(leaf)]));
    let root = doc.add_object(dictionary! { "Kids" => Object::Reference(kids) });
    doc.catalog_mut()
        .unwrap()
        .set("PageLabels", Object::Reference(root));

    pdf.move_page(3, 0).unwrap();
    let labels = pdf
        .document()
        .catalog()
        .unwrap()
        .get(b"PageLabels")
        .unwrap();
    assert!(
        labels.as_dict().is_ok_and(|dict| dict.has(b"Nums")),
        "labels stored under a child were not carried over: {labels:?}"
    );
    assert_eq!(
        label_ranges(&pdf),
        vec![(0, "D".into(), 2), (1, "r".into(), 1), (3, "D".into(), 1)]
    );
}

#[test]
fn a_moved_page_keeps_labels_from_a_node_and_its_child() {
    // Roman numbers are written on the root and decimal numbers in its child.
    // Moving the last page to the front leaves 2, i, ii, 1. Reading only the root
    // makes the whole file roman. Reading only the child leaves the first pages
    // blank. The older move has no child, and the child-only tree has nothing on
    // the root.
    let mut pdf = blank(4);
    let doc = pdf.document_mut();
    let leaf = doc.add_object(dictionary! {
        "Nums" => vec![2.into(), dictionary! { "S" => "D" }.into()]
    });
    doc.catalog_mut().unwrap().set(
        "PageLabels",
        dictionary! {
            "Nums" => vec![0.into(), dictionary! { "S" => "r" }.into()],
            "Kids" => vec![Object::Reference(leaf)],
        },
    );

    pdf.move_page(3, 0).unwrap();
    assert_eq!(
        label_ranges(&pdf),
        vec![(0, "D".into(), 2), (1, "r".into(), 1), (3, "D".into(), 1)]
    );
}

#[test]
fn a_moved_page_uses_page_labels_stored_out_of_order() {
    // Decimal from page 2 is written before roman from page 0. Moving the last
    // page to the front still leaves 2, i, ii, 1. Keeping the file order makes
    // that page roman 4, because the earlier entry is the one found first.
    // The older move already stores the lower page first.
    let mut pdf = blank(4);
    pdf.document_mut().catalog_mut().unwrap().set(
        "PageLabels",
        dictionary! { "Nums" => vec![
            2.into(), dictionary! { "S" => "D" }.into(),
            0.into(), dictionary! { "S" => "r" }.into(),
        ] },
    );

    pdf.move_page(3, 0).unwrap();
    assert_eq!(
        label_ranges(&pdf),
        vec![(0, "D".into(), 2), (1, "r".into(), 1), (3, "D".into(), 1)]
    );
}

#[test]
fn a_moved_page_keeps_a_page_index_stored_as_its_own_object() {
    // Page 2's index is an object of its own. Moving the last page to the front
    // still leaves 2, i, ii, 1. Leaving the index as a reference drops the decimal
    // range, so the moved page is roman 4. The older move writes every index as a
    // number in the array.
    let mut pdf = blank(4);
    let doc = pdf.document_mut();
    let decimal_at = doc.add_object(Object::Integer(2));
    doc.catalog_mut().unwrap().set(
        "PageLabels",
        dictionary! { "Nums" => vec![
            0.into(), dictionary! { "S" => "r" }.into(),
            Object::Reference(decimal_at), dictionary! { "S" => "D" }.into(),
        ] },
    );

    pdf.move_page(3, 0).unwrap();
    assert_eq!(
        label_ranges(&pdf),
        vec![(0, "D".into(), 2), (1, "r".into(), 1), (3, "D".into(), 1)]
    );
}

#[test]
fn an_unstyled_prefix_stays_with_its_pages_across_a_number_gap() {
    // A-, A-8, 1. Neither A page has a numbering style, and 8 does not follow 1.
    // Moving A-8 to the front leaves A-, A-, 1 as two ranges: the prefix is kept
    // and the gap does not start a third range. A style is what would record the
    // 8. The older move has a style and no prefix, so the next number is what
    // joins its pages.
    let mut pdf = blank(3);
    pdf.document_mut().catalog_mut().unwrap().set(
        "PageLabels",
        dictionary! { "Nums" => vec![
            0.into(), dictionary! { "P" => Object::string_literal("A-") }.into(),
            1.into(), dictionary! {
                "P" => Object::string_literal("A-"),
                "St" => 8,
            }.into(),
            2.into(), dictionary! { "S" => "D" }.into(),
        ] },
    );

    pdf.move_page(1, 0).unwrap();
    assert_eq!(
        label_entries(&pdf),
        vec![
            (0, "".into(), "A-".into(), 1),
            (2, "D".into(), "".into(), 1)
        ]
    );
}

#[test]
fn pages_before_numbering_starts_keep_a_blank_label() {
    // Numbering starts at page 2, so the first two pages have no label. Moving
    // the middle page to the front puts those two together even though their
    // numbers do not follow, and neither gains the decimal style. Giving them
    // that style, or starting a range wherever the number does not follow,
    // leaves a different tree. The older move numbers every page from the start.
    let mut pdf = blank(3);
    pdf.document_mut().catalog_mut().unwrap().set(
        "PageLabels",
        dictionary! { "Nums" => vec![2.into(), dictionary! { "S" => "D" }.into()] },
    );

    pdf.move_page(1, 0).unwrap();
    assert_eq!(
        label_entries(&pdf),
        vec![(0, "".into(), "".into(), 1), (2, "D".into(), "".into(), 1)]
    );
}

#[test]
fn moving_a_page_of_a_document_without_page_labels_adds_none() {
    let mut pdf = blank(3);
    pdf.move_page(0, 2).unwrap();
    assert!(!pdf.document().catalog().unwrap().has(b"PageLabels"));
}

#[test]
fn moving_a_page_leaves_an_unreadable_label_tree_unchanged() {
    // A label list with a page index and no style is not a range. Moving a page
    // still reorders the pages and leaves that list where it was. Replacing it
    // with an empty list drops the index. A document with no label entry at all
    // is the older check.
    let mut pdf = blank(3);
    let labels = pdf
        .document_mut()
        .add_object(dictionary! { "Nums" => vec![0.into()] });
    pdf.document_mut()
        .catalog_mut()
        .unwrap()
        .set("PageLabels", Object::Reference(labels));
    let stored = pdf.document().get_object(labels).unwrap().clone();
    let pages = pdf.page_ids();

    pdf.move_page(2, 0).unwrap();
    assert_eq!(pdf.page_ids(), vec![pages[2], pages[0], pages[1]]);
    assert_eq!(
        pdf.document()
            .catalog()
            .unwrap()
            .get(b"PageLabels")
            .unwrap()
            .as_reference()
            .unwrap(),
        labels
    );
    assert_eq!(pdf.document().get_object(labels).unwrap(), &stored);
}

#[test]
fn a_file_is_renamed_only_to_a_name_nothing_has() {
    let dir = tempfile::tempdir().unwrap();
    let from = dir.path().join("from.pdf");
    let to = dir.path().join("to.pdf");
    std::fs::write(&from, b"new").unwrap();

    // Free: the file moves.
    carbide_engine::fs::rename_no_replace(&from, &to).unwrap();
    assert!(!from.exists());
    assert_eq!(std::fs::read(&to).unwrap(), b"new");

    // Taken, by a file or by a link that leads nowhere: nothing is replaced.
    std::fs::write(&from, b"newer").unwrap();
    let error = carbide_engine::fs::rename_no_replace(&from, &to).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(&to).unwrap(), b"new");
    let dangling = dir.path().join("dangling.pdf");
    std::os::unix::fs::symlink(dir.path().join("nowhere.pdf"), &dangling).unwrap();
    let error = carbide_engine::fs::rename_no_replace(&from, &dangling).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert!(dangling
        .symlink_metadata()
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(std::fs::read(&from).unwrap(), b"newer");
}

#[test]
fn moving_a_page_to_where_it_is_changes_nothing() {
    let mut pdf = blank(3);
    let labels = pdf
        .document_mut()
        .add_object(dictionary! { "Nums" => vec![0.into(), dictionary! { "S" => "r" }.into()] });
    pdf.document_mut()
        .catalog_mut()
        .unwrap()
        .set("PageLabels", labels);
    let before = pdf.document_mut().objects.clone();
    pdf.move_page(1, 1).unwrap();
    assert!(
        pdf.document_mut().objects == before,
        "moving a page in place edited the document"
    );
}
