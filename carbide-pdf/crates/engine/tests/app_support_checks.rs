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
            (
                pair[0].as_i64().unwrap(),
                style
                    .get(b"S")
                    .map(|s| String::from_utf8_lossy(s.as_name().unwrap()).into_owned())
                    .unwrap_or_default(),
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
fn deleting_a_page_keeps_a_style_when_its_number_follows() {
    // 1, 2, iii, iv. Roman numerals start at 3, so iii is the next number
    // after 2, in another style. Deleting iv leaves 1, 2, iii, and iii still
    // starts its own range. Joining a page to the previous one whenever the
    // number follows, whatever the style, makes that page decimal 3. The
    // older deletion keeps decimal 2 after roman ii, and 2 does not follow 2.
    // The older move keeps roman i after decimal 2, and 1 does not follow 2.
    let mut pdf = blank(4);
    pdf.document_mut().catalog_mut().unwrap().set(
        "PageLabels",
        dictionary! { "Nums" => vec![
            0.into(), dictionary! { "S" => "D" }.into(),
            2.into(), dictionary! { "S" => "r", "St" => 3 }.into(),
        ] },
    );

    pdf.delete_pages(&[3]).unwrap();
    assert_eq!(pdf.page_count(), 3);
    assert_eq!(
        label_ranges(&pdf),
        vec![(0, "D".into(), 1), (2, "r".into(), 3)]
    );
}

#[test]
fn moving_a_page_keeps_a_label_numbered_zero() {
    // 0, 1. The page numbered 0 goes last: 1, 0. A start is recorded for
    // every number except 1, so 0 stays 0. Recording a start only when the
    // number is greater than 1 leaves that page numbered 1. The older move
    // records a start of 2, which is still recorded either way.
    let mut pdf = blank(2);
    pdf.document_mut().catalog_mut().unwrap().set(
        "PageLabels",
        dictionary! { "Nums" => vec![
            0.into(), dictionary! { "S" => "D", "St" => 0 }.into(),
        ] },
    );

    pdf.move_page(0, 1).unwrap();
    assert_eq!(
        label_ranges(&pdf),
        vec![(0, "D".into(), 1), (1, "D".into(), 0)]
    );
}

#[test]
fn moving_a_page_of_a_document_without_page_labels_adds_none() {
    let mut pdf = blank(3);
    pdf.move_page(0, 2).unwrap();
    assert!(!pdf.document().catalog().unwrap().has(b"PageLabels"));
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
