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

#[test]
fn repairing_a_label_loop_reads_the_child_through_its_kids_array() {
    // The node does not list itself in its own dictionary. Its children are
    // a separate object, and that object names the node and then the only
    // labels: roman numerals from page 0. The page index is an object of its
    // own. Opening rebuilds the tree into that one range. Leaving the
    // separate children unread keeps the loop, so the tree still has kids.
    // Rebuilding without reading those children, or without reading the page
    // index, leaves no labels. A sound tree stores its children and its
    // numbers directly, and a move rewrites that tree.
    let pdf = Pdf::from_bytes(&handwritten(&[
        "<< /Type /Catalog /Pages 2 0 R /PageLabels 4 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>",
        "<< /Kids 5 0 R >>",
        "[4 0 R 6 0 R]",
        "<< /Nums [7 0 R << /S /r >>] >>",
        "0",
    ]))
    .unwrap();
    let catalog = pdf.document().catalog().unwrap();
    let root = catalog.get(b"PageLabels").unwrap().as_reference().unwrap();
    let dict = pdf.document().get_dictionary(root).unwrap();
    assert!(
        !dict.has(b"Kids"),
        "a label loop behind a separate kids array was left in place: {dict:?}"
    );
    let nums = dict.get(b"Nums").unwrap().as_array().unwrap();
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
fn a_field_read_under_an_earlier_parent_is_not_kept_again_later() {
    // The form lists a parent, then another field, then the widget the
    // parent already lists. Readers walk everything under the parent before
    // the next field, so the widget stays under the parent and the later
    // copy goes. Walking the form's own list first drops the widget from
    // the parent and keeps the later copy. The fields are the form's own
    // object, and the parent's children sit in the parent.
    let pdf = Pdf::from_bytes(&handwritten(&[
        "<< /Type /Catalog /Pages 2 0 R /AcroForm 4 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>",
        "<< /Fields [5 0 R 7 0 R 6 0 R] >>",
        "<< /T (First) /Kids [6 0 R] >>",
        "<< /T (Widget) >>",
        "<< /T (Other) >>",
    ]))
    .unwrap();
    assert_eq!(listed(&pdf, 4, b"Fields"), vec![id(5), id(7)]);
    assert_eq!(listed(&pdf, 5, b"Kids"), vec![id(6)]);
}

#[test]
fn deduping_fields_leaves_an_array_another_field_still_shares() {
    // A parent lists one widget twice, and the list is an object of its own.
    // A field the form does not list names that same object. Opening keeps
    // the widget once, on the parent, and the shared object still lists it
    // twice: the other field was not part of the repair. Rewriting the
    // shared object leaves the other field with one child. A list with no
    // repeated field is left as a reference, which this parent no longer is.
    let pdf = Pdf::from_bytes(&handwritten(&[
        "<< /Type /Catalog /Pages 2 0 R /AcroForm 4 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>",
        "<< /Fields [8 0 R] >>",
        "[6 0 R 6 0 R]",
        "<< /T (Widget) >>",
        "<< /T (Other) /Kids 5 0 R >>",
        "<< /T (Parent) /Kids 5 0 R >>",
    ]))
    .unwrap();
    let doc = pdf.document();
    let kids = doc.get_dictionary(id(8)).unwrap().get(b"Kids").unwrap();
    assert_eq!(
        kids,
        &Object::Array(vec![Object::Reference(id(6))]),
        "the parent should name the widget once, without the shared list: {kids:?}"
    );
    assert_eq!(
        doc.get_object(id(5)).unwrap(),
        &Object::Array(vec![Object::Reference(id(6)), Object::Reference(id(6))]),
        "the shared field list was rewritten"
    );
    assert_eq!(
        doc.get_dictionary(id(7)).unwrap().get(b"Kids").unwrap(),
        &Object::Reference(id(5))
    );
}
