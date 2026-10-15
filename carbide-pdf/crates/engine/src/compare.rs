//! Text-level comparison of two documents, page by page. Layout and image changes are not detected.

use similar::{capture_diff_slices, Algorithm, DiffOp};

use crate::error::Result;
use crate::pdf::Pdf;
use crate::render::Renderer;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageDiff {
    /// One-based page number.
    pub page_number: usize,
    pub removed: Vec<String>,
    pub added: Vec<String>,
}

impl PageDiff {
    pub fn is_identical(&self) -> bool {
        self.removed.is_empty() && self.added.is_empty()
    }
}

fn lines(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Compares the extracted text of `original` and `revised`.
pub fn compare_documents(
    renderer: &Renderer,
    original: &mut Pdf,
    revised: &mut Pdf,
) -> Result<Vec<PageDiff>> {
    for pdf in [&*original, &*revised] {
        for index in 0..pdf.page_count() {
            pdf.check_drawable(index)?;
        }
    }
    let old_bytes = original.to_bytes()?;
    let new_bytes = revised.to_bytes()?;
    let (old_count, new_count) = (original.page_count(), revised.page_count());

    (0..old_count.max(new_count))
        .map(|index| {
            let old = if index < old_count {
                lines(&renderer.page_text(&old_bytes, index)?)
            } else {
                Vec::new()
            };
            let new = if index < new_count {
                lines(&renderer.page_text(&new_bytes, index)?)
            } else {
                Vec::new()
            };
            let mut removed = Vec::new();
            let mut added = Vec::new();
            for op in capture_diff_slices(Algorithm::Myers, &old, &new) {
                match op {
                    DiffOp::Equal { .. } => {}
                    DiffOp::Delete {
                        old_index, old_len, ..
                    } => {
                        removed.extend_from_slice(&old[old_index..old_index + old_len]);
                    }
                    DiffOp::Insert {
                        new_index, new_len, ..
                    } => {
                        added.extend_from_slice(&new[new_index..new_index + new_len]);
                    }
                    DiffOp::Replace {
                        old_index,
                        old_len,
                        new_index,
                        new_len,
                    } => {
                        removed.extend_from_slice(&old[old_index..old_index + old_len]);
                        added.extend_from_slice(&new[new_index..new_index + new_len]);
                    }
                }
            }
            Ok(PageDiff {
                page_number: index + 1,
                removed,
                added,
            })
        })
        .collect()
}
