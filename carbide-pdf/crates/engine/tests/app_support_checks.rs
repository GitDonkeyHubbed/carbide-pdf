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
fn moving_a_page_of_a_document_without_page_labels_adds_none() {
    let mut pdf = blank(3);
    pdf.move_page(0, 2).unwrap();
    assert!(!pdf.document().catalog().unwrap().has(b"PageLabels"));
}
