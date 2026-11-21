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

/// A PDF whose body objects are `objects`, numbered from 1, with object 1 as the catalog.
fn handwritten(objects: &[&str]) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

/// Page labels after a tree that looped has been rebuilt: (first page, style) per range.
/// The rebuilt tree is one object and has no children.
fn repaired_label_styles(pdf: &Pdf) -> Vec<(i64, String)> {
    let catalog = pdf.document().catalog().unwrap();
    let Object::Reference(id) = catalog.get(b"PageLabels").unwrap() else {
        panic!("rebuilt page labels should be one object");
    };
    let dict = pdf.document().get_dictionary(*id).unwrap();
    assert!(
        !dict.has(b"Kids"),
        "a rebuilt label tree is a flat list, got {dict:?}"
    );
    dict.get(b"Nums")
        .unwrap()
        .as_array()
        .unwrap()
        .chunks(2)
        .map(|pair| {
            (
                pair[0].as_i64().unwrap(),
                String::from_utf8_lossy(
                    pair[1]
                        .as_dict()
                        .unwrap()
                        .get(b"S")
                        .unwrap()
                        .as_name()
                        .unwrap(),
                )
                .into_owned(),
            )
        })
        .collect()
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
fn repairing_a_looping_label_tree_keeps_the_numbers_on_the_node() {
    // The node lists itself, so opening rebuilds the tree. It has its own
    // numbers, stored as their own object, and a child as well: roman numerals
    // from page 0, while the child would letter page 2. The numbers are what
    // gets kept. Rebuilding from the child instead leaves A. Reading the child
    // as well leaves A beside the roman numerals. Leaving the number list
    // unread, because it is a separate object, leaves no labels. Moving a page
    // reads a sound tree whose numbers sit in the node, so that still passes.
    let pdf = Pdf::from_bytes(&handwritten(&[
        "<< /Type /Catalog /Pages 2 0 R /PageLabels 4 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>",
        "<< /Nums 6 0 R /Kids [4 0 R 5 0 R] >>",
        "<< /Nums [2 << /S /A >>] >>",
        "[0 << /S /r >>]",
    ]))
    .unwrap();
    assert_eq!(repaired_label_styles(&pdf), vec![(0, "r".into())]);
}

#[test]
fn repairing_a_looping_label_tree_keeps_the_first_number_for_a_page() {
    // The node lists itself, and page 0 is numbered twice. The first entry is
    // not a page number. The first real number is roman, and the later one is
    // decimal. Roman is kept. Treating the name as page 0 keeps the decimal
    // label. Keeping the last number for a page keeps the decimal label too.
    // A sound tree is not rebuilt, and a move rewrites one number per page, so
    // both still pass.
    let pdf = Pdf::from_bytes(&handwritten(&[
        "<< /Type /Catalog /Pages 2 0 R /PageLabels 4 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>",
        "<< /Nums [/NotAPage << /S /D >> 0 << /S /r >> 0 << /S /D /St 5 >>] /Kids [4 0 R] >>",
    ]))
    .unwrap();
    assert_eq!(repaired_label_styles(&pdf), vec![(0, "r".into())]);
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
