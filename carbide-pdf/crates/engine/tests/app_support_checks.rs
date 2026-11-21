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
fn the_earlier_child_keeps_its_label_when_a_label_tree_loops() {
    // The node's children are roman for page 0, then decimal for page 0, and
    // then the node itself. Opening rebuilds the tree, and the earlier child
    // is the one that stays: roman. Reading the later child first, or keeping
    // the last label for a page, leaves decimal. A move of a sound tree has
    // one label per page.
    let pdf = Pdf::from_bytes(&handwritten(&[
        "<< /Type /Catalog /Pages 2 0 R /PageLabels 4 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>",
        "<< /Kids [5 0 R 6 0 R 4 0 R] >>",
        "<< /Nums [0 << /S /r >>] >>",
        "<< /Nums [0 << /S /D >>] >>",
    ]))
    .unwrap();
    let catalog = pdf.document().catalog().unwrap();
    let root = catalog.get(b"PageLabels").unwrap().as_reference().unwrap();
    let dict = pdf.document().get_dictionary(root).unwrap();
    assert!(
        !dict.has(b"Kids"),
        "a label node that lists itself was left in place: {dict:?}"
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
fn a_move_keeps_the_later_label_for_the_same_page() {
    // A sound tree lists page 0 twice, roman and then decimal, so it is left
    // as written. Moving a page applies the later label: the pages are
    // decimal 2, then 1. Keeping the earlier label numbers them ii, then i.
    // A looping tree is rebuilt and keeps the first label instead, which is
    // a different file.
    let mut pdf = blank(2);
    pdf.document_mut().catalog_mut().unwrap().set(
        "PageLabels",
        dictionary! { "Nums" => vec![
            0.into(), dictionary! { "S" => "r" }.into(),
            0.into(), dictionary! { "S" => "D" }.into(),
        ] },
    );
    pdf.move_page(1, 0).unwrap();
    assert_eq!(
        label_ranges(&pdf),
        vec![(0, "D".into(), 2), (1, "D".into(), 1)]
    );
}

#[test]
fn a_repeated_page_number_starts_its_own_range() {
    // 5, 5, 6. The second 5 does not follow the first. Moving 6 to the front
    // leaves 6, 5, 5, and the last 5 starts again. Treating a repeated
    // number as the next one joins that page on, so it is numbered 6.
    let mut pdf = blank(3);
    pdf.document_mut().catalog_mut().unwrap().set(
        "PageLabels",
        dictionary! { "Nums" => vec![
            0.into(), dictionary! { "S" => "D", "St" => 5 }.into(),
            1.into(), dictionary! { "S" => "D", "St" => 5 }.into(),
            2.into(), dictionary! { "S" => "D", "St" => 6 }.into(),
        ] },
    );
    pdf.move_page(2, 0).unwrap();
    assert_eq!(
        label_ranges(&pdf),
        vec![(0, "D".into(), 6), (1, "D".into(), 5), (2, "D".into(), 5)]
    );
}

#[test]
fn a_field_read_before_its_parent_is_not_taken_by_that_parent() {
    // The form lists the widget, then the parent that lists it again.
    // Readers meet the widget first, so it stays there and the parent loses
    // it. Walking the form's list from the end, or keeping whichever copy
    // sits under a parent, leaves the widget under the parent and drops it
    // from the form. The other order, parent before the later copy, is a
    // different file.
    let pdf = Pdf::from_bytes(&handwritten(&[
        "<< /Type /Catalog /Pages 2 0 R /AcroForm 4 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>",
        "<< /Fields [6 0 R 5 0 R] >>",
        "<< /T (Parent) /Kids [6 0 R] >>",
        "<< /T (Widget) >>",
    ]))
    .unwrap();
    assert_eq!(listed(&pdf, 4, b"Fields"), vec![id(6), id(5)]);
    assert_eq!(listed(&pdf, 5, b"Kids"), Vec::<lopdf::ObjectId>::new());
}

#[test]
fn an_inline_field_stays_when_a_repeated_field_is_dropped() {
    // The form lists a field written in place, then a widget, then another
    // field written in place with the same name, then the widget again. The
    // widget is kept once. Both in-place fields stay: they are not one
    // object, so they cannot be a loop. Dropping anything that is not a
    // reference leaves only the widget. The other field lists are all
    // references.
    let pdf = Pdf::from_bytes(&handwritten(&[
        "<< /Type /Catalog /Pages 2 0 R /AcroForm 4 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>",
        "<< /Fields [<< /T (A) >> 5 0 R << /T (A) >> 5 0 R] >>",
        "<< /T (B) >>",
    ]))
    .unwrap();
    let fields = pdf
        .document()
        .get_dictionary(id(4))
        .unwrap()
        .get(b"Fields")
        .unwrap()
        .as_array()
        .unwrap();
    let inline = fields
        .iter()
        .filter(|object| object.as_dict().is_ok())
        .count();
    let widget = fields
        .iter()
        .filter(|object| object.as_reference().ok() == Some(id(5)))
        .count();
    assert_eq!((fields.len(), inline, widget), (3, 2, 1), "{fields:?}");
    for object in fields.iter().filter(|object| object.as_dict().is_ok()) {
        assert_eq!(
            object
                .as_dict()
                .unwrap()
                .get(b"T")
                .unwrap()
                .as_str()
                .unwrap(),
            b"A",
            "{fields:?}"
        );
    }
}
