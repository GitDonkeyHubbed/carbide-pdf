//! An appended update points backward. Only the forward link inside a real linearized file is
//! not an update.

use carbide_engine::{SavePlan, SavedFile};
use lopdf::Object;

/// A one-page file whose first object (number 4) is `first`, which starts after `padding` bytes
/// of comment. With `two_sections`, the cross-reference data is split as in a linearized file: a
/// section near the start for the first object, linked by /Prev to the main one at the end.
fn file_starting_with(first: &str, padding: usize, two_sections: bool) -> Vec<u8> {
    let build = |prev: usize| {
        let mut bytes = b"%PDF-1.7\n".to_vec();
        if padding > 0 {
            bytes.push(b'%');
            bytes.extend(vec![b'x'; padding]);
            bytes.push(b'\n');
        }
        let mut offsets = [0usize; 5];
        let objects = [
            (4, first),
            (1, "<</Type/Catalog/Pages 2 0 R>>"),
            (2, "<</Type/Pages/Kids [3 0 R]/Count 1>>"),
            (3, "<</Type/Page/Parent 2 0 R/MediaBox [0 0 300 300]>>"),
        ];
        let mut first_section = 0;
        for (number, body) in objects {
            offsets[number] = bytes.len();
            bytes.extend(format!("{number} 0 obj\n{body}\nendobj\n").into_bytes());
            if two_sections && number == 4 {
                first_section = bytes.len();
                bytes.extend(
                    format!(
                        "xref\n4 1\n{:010} 00000 n \ntrailer\n<</Size 5/Root 1 0 R/Prev {prev:010}>>\nstartxref\n0\n%%EOF\n",
                        offsets[4]
                    )
                    .into_bytes(),
                );
            }
        }
        let main = bytes.len();
        let entries = if two_sections { 1..4 } else { 1..5 };
        let count = if two_sections { 4 } else { 5 };
        bytes.extend(format!("xref\n0 {count}\n0000000000 65535 f \n").into_bytes());
        for number in entries {
            bytes.extend(format!("{:010} 00000 n \n", offsets[number]).into_bytes());
        }
        let start = if two_sections { first_section } else { main };
        bytes.extend(
            format!("trailer\n<</Size 5/Root 1 0 R>>\nstartxref\n{start}\n%%EOF\n").into_bytes(),
        );
        (bytes, main)
    };
    let (_, main) = build(0);
    build(main).0
}

/// How many updates `file` reads as having before and after one more is appended.
fn updates_before_and_after_one_more(file: Vec<u8>) -> (usize, usize) {
    let (mut pdf, saved) = SavedFile::open(file, None).unwrap();
    let before = saved.appended_updates();
    let catalog = pdf
        .document()
        .trailer
        .get(b"Root")
        .unwrap()
        .as_reference()
        .unwrap();
    pdf.document_mut()
        .get_dictionary_mut(catalog)
        .unwrap()
        .set("Lang", Object::string_literal("en"));
    let updated = pdf.save_bytes(SavePlan::Incremental, Some(&saved)).unwrap();
    let (_, saved) = SavedFile::open(updated, None).unwrap();
    (before, saved.appended_updates())
}

#[test]
fn a_claimed_linearization_dictionary_does_not_hide_a_backward_update() {
    const WHOLE: &str = "<</Linearized 1/L 0/H [0000000000 30]/O 3/E 0/N 1/T 0>>";
    // A real linearized file: the link from its first-page section to the main one is no update.
    assert_eq!(
        updates_before_and_after_one_more(file_starting_with(WHOLE, 0, true)),
        (0, 1)
    );
    // A forward link is an update when the dictionary's version is not 1, and the update
    // appended after it is another.
    assert_eq!(
        updates_before_and_after_one_more(file_starting_with(
            &WHOLE.replace("Linearized 1", "Linearized 2"),
            0,
            true,
        )),
        (1, 2)
    );
    // A complete dictionary whose only link is an ordinary appended update, which points
    // backward, still has that update. So does a version other than 1.
    assert_eq!(
        updates_before_and_after_one_more(file_starting_with(WHOLE, 0, false)),
        (0, 1),
        "an appended update after a complete dictionary"
    );
    assert_eq!(
        updates_before_and_after_one_more(file_starting_with(
            &WHOLE.replace("Linearized 1", "Linearized 2"),
            0,
            false,
        )),
        (0, 1),
        "a version other than 1"
    );
}
