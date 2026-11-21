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

fn id(number: u32) -> lopdf::ObjectId {
    (number, 0)
}

/// The object ids in the array `key` of object `owner`.
fn listed(pdf: &Pdf, owner: u32, key: &[u8]) -> Vec<lopdf::ObjectId> {
    pdf.document()
        .get_dictionary(id(owner))
        .unwrap()
        .get(key)
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|object| object.as_reference().unwrap())
        .collect()
}

/// The rebuilt page-label numbers. A node that still lists children was not rebuilt.
fn rebuilt_numbers(pdf: &Pdf) -> Vec<lopdf::Object> {
    let catalog = pdf.document().catalog().unwrap();
    let root = catalog.get(b"PageLabels").unwrap().as_reference().unwrap();
    let dict = pdf.document().get_dictionary(root).unwrap();
    assert!(
        !dict.has(b"Kids"),
        "a label node that lists itself was left in place: {dict:?}"
    );
    dict.get(b"Nums").unwrap().as_array().unwrap().clone()
}

#[test]
fn a_field_after_a_repeated_one_stays_in_the_form() {
    // The form lists A, A again, then a parent. The second A is dropped and
    // the parent stays. The parent lists A and then B, so A is dropped there
    // too and B stays. Stopping at the repeated A leaves the parent out.
    // Leaving the parent's own list unread keeps A beside B.
    let pdf = Pdf::from_bytes(&handwritten(&[
        "<< /Type /Catalog /Pages 2 0 R /AcroForm 4 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>",
        "<< /Fields [5 0 R 5 0 R 6 0 R] >>",
        "<< /T (A) >>",
        "<< /T (Parent) /Kids [5 0 R 7 0 R] >>",
        "<< /T (B) >>",
    ]))
    .unwrap();
    assert_eq!(listed(&pdf, 4, b"Fields"), vec![id(5), id(6)]);
    assert_eq!(listed(&pdf, 6, b"Kids"), vec![id(7)]);
}

#[test]
fn a_page_index_that_is_not_a_number_does_not_start_a_range() {
    // Roman from page 0, then a name where the next page index belongs, with
    // a decimal label. The name is not a page, so moving a page leaves roman
    // numerals. Counting the name as page 0 lets the decimal label win.
    let mut pdf = blank(2);
    pdf.document_mut().catalog_mut().unwrap().set(
        "PageLabels",
        dictionary! { "Nums" => vec![
            0.into(), dictionary! { "S" => "r" }.into(),
            Object::Name(b"NotAPage".to_vec()), dictionary! { "S" => "D" }.into(),
        ] },
    );
    pdf.move_page(1, 0).unwrap();
    assert_eq!(
        label_ranges(&pdf),
        vec![(0, "r".into(), 2), (1, "r".into(), 1)]
    );
}

#[test]
fn a_rebuilt_label_tree_skips_a_page_index_that_is_not_a_number() {
    // The node lists itself, so opening rebuilds it. Its numbers are a name
    // with a decimal label, then roman from page 0. Roman is what remains.
    // Counting the name as page 0 keeps the decimal label instead.
    let pdf = Pdf::from_bytes(&handwritten(&[
        "<< /Type /Catalog /Pages 2 0 R /PageLabels 4 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>",
        "<< /Nums [/NotAPage << /S /D >> 0 << /S /r >>] /Kids [4 0 R] >>",
    ]))
    .unwrap();
    let nums = rebuilt_numbers(&pdf);
    assert_eq!(nums.len(), 2, "{nums:?}");
    assert_eq!(nums[0].as_i64().unwrap(), 0, "{nums:?}");
    assert_eq!(
        nums[1]
            .as_dict()
            .unwrap()
            .get(b"S")
            .unwrap()
            .as_name()
            .unwrap(),
        b"r",
        "{nums:?}"
    );
}

#[test]
fn a_label_node_with_null_numbers_does_not_keep_its_child() {
    // The node's numbers are null, and it lists a child that would number
    // page 0 in roman, then itself. Opening rebuilds the tree with no
    // labels. Reading the child when the numbers are not an array leaves
    // roman.
    let pdf = Pdf::from_bytes(&handwritten(&[
        "<< /Type /Catalog /Pages 2 0 R /PageLabels 4 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>",
        "<< /Nums null /Kids [5 0 R 4 0 R] >>",
        "<< /Nums [0 << /S /r >>] >>",
    ]))
    .unwrap();
    assert!(
        rebuilt_numbers(&pdf).is_empty(),
        "a child label was kept: {:?}",
        rebuilt_numbers(&pdf)
    );
}

#[test]
fn a_label_node_with_an_empty_number_list_does_not_keep_its_child() {
    // The same shape as a null numbers entry, except the list is an empty
    // array. The child is still not a second list of labels. Reading
    // children whenever that array is empty leaves roman.
    let pdf = Pdf::from_bytes(&handwritten(&[
        "<< /Type /Catalog /Pages 2 0 R /PageLabels 4 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>",
        "<< /Nums [] /Kids [5 0 R 4 0 R] >>",
        "<< /Nums [0 << /S /r >>] >>",
    ]))
    .unwrap();
    assert!(
        rebuilt_numbers(&pdf).is_empty(),
        "a child label was kept: {:?}",
        rebuilt_numbers(&pdf)
    );
}
