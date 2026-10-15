//! Watermarks and Bates numbers, added as extra content on existing pages.

use crate::error::{Error, Result};
use crate::fonts::{literal_string, win_ansi, StandardFont};
use crate::geometry::{fmt, matrix_string};
use crate::pdf::Pdf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PageAnchor {
    TopLeft,
    TopCenter,
    TopRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

impl PageAnchor {
    pub const ALL: [PageAnchor; 6] = [
        PageAnchor::TopLeft,
        PageAnchor::TopCenter,
        PageAnchor::TopRight,
        PageAnchor::BottomLeft,
        PageAnchor::BottomCenter,
        PageAnchor::BottomRight,
    ];

    fn is_top(self) -> bool {
        matches!(
            self,
            PageAnchor::TopLeft | PageAnchor::TopCenter | PageAnchor::TopRight
        )
    }
}

// ---- watermark -----------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct WatermarkOptions {
    pub text: String,
    pub font: StandardFont,
    pub font_size: f32,
    /// RGB, each 0.0..=1.0.
    pub color: [f32; 3],
    /// 0.0 (invisible) ..= 1.0 (opaque).
    pub opacity: f32,
    /// Counter-clockwise, in degrees, as seen on the displayed page.
    pub rotation_degrees: f32,
    /// Zero-based pages; `None` stamps every page.
    pub pages: Option<Vec<usize>>,
}

impl WatermarkOptions {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            font: StandardFont::HelveticaBold,
            font_size: 72.0,
            color: [0.6, 0.6, 0.6],
            opacity: 0.3,
            rotation_degrees: 45.0,
            pages: None,
        }
    }
}

impl WatermarkOptions {
    /// Checks that the watermark can be drawn: text, a size larger than 0 and no larger than a page
    /// can be, an angle, and an opacity and colour between 0 and 1.
    pub fn validate(&self) -> Result<()> {
        if self.text.trim().is_empty() {
            return Err(Error::Other("watermark text is empty".into()));
        }
        let fraction = |v: f32| (0.0..=1.0).contains(&v);
        let sized = self.font_size.is_finite()
            && self.font_size > 0.0
            && self.font_size <= crate::pdf::MAX_PAGE_SIDE;
        if !(sized
            && self.rotation_degrees.is_finite()
            && fraction(self.opacity)
            && self.color.iter().all(|&c| fraction(c)))
        {
            return Err(Error::Other(messages::WATERMARK.into()));
        }
        Ok(())
    }
}

impl Pdf {
    /// Centers a rotated, semi-transparent text watermark on the selected pages.
    pub fn apply_watermark(&mut self, options: &WatermarkOptions) -> Result<()> {
        options.validate()?;
        let count = self.page_count();
        // A page listed twice is stamped once.
        let selected: Vec<usize> = match &options.pages {
            Some(p) => crate::pdf::once_each(p),
            None => (0..count).collect(),
        };
        let width = options.font.text_width(&options.text, options.font_size);
        let (sin, cos) = options.rotation_degrees.to_radians().sin_cos();
        let drop = options.font.cap_height() * options.font_size / 2.0;
        let [r, g, b] = options.color;

        let ids = self.page_ids();
        for index in selected {
            let page_id = *ids
                .get(index)
                .ok_or(Error::PageOutOfRange { index, count })?;
            let geometry = self.geometry_of(page_id);
            let (w, h) = geometry.display_size();
            let font_name = self.add_standard_font(page_id, options.font)?;
            let state_name = self.add_alpha_state(page_id, options.opacity)?;

            let mut content: Vec<u8> = Vec::new();
            let header = format!(
                "q\n{}\n/{} gs\n{} {} {} rg\n{} {} {} {} {} {} cm\nBT\n/{} {} Tf\n{} {} Td\n",
                matrix_string(geometry.display_to_user()),
                state_name,
                fmt(r),
                fmt(g),
                fmt(b),
                fmt(cos),
                fmt(sin),
                fmt(-sin),
                fmt(cos),
                fmt(w / 2.0),
                fmt(h / 2.0),
                font_name,
                fmt(options.font_size),
                fmt(-width / 2.0),
                fmt(-drop),
            );
            content.extend_from_slice(header.as_bytes());
            content.extend_from_slice(&literal_string(&win_ansi(&options.text)));
            content.extend_from_slice(b" Tj\nET\nQ\n");
            self.append_content(page_id, content)?;
        }
        Ok(())
    }
}

// ---- Bates numbering -----------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BatesOptions {
    pub prefix: String,
    pub suffix: String,
    pub start_number: u64,
    /// Zero-padded width of the number.
    pub digits: usize,
    pub anchor: PageAnchor,
    pub font: StandardFont,
    pub font_size: f32,
    pub margin: f32,
}

impl Default for BatesOptions {
    fn default() -> Self {
        Self {
            prefix: String::new(),
            suffix: String::new(),
            start_number: 1,
            digits: 6,
            anchor: PageAnchor::BottomRight,
            font: StandardFont::Helvetica,
            font_size: 10.0,
            margin: 24.0,
        }
    }
}

/// The most digits a Bates number is padded to: as many as the largest number has.
pub const MAX_BATES_DIGITS: usize = 20;

pub mod messages {
    pub const BATES_DIGITS: &str = "Bates numbers can be padded to between 1 and 20 digits.";
    pub const BATES_RANGE: &str =
        "The Bates numbers would go past 18446744073709551615, the largest that can be stamped.";
    pub const BATES_TEXT: &str =
        "The Bates text size and margin have to be numbers, and the size larger than 0.";
    pub const WATERMARK: &str =
        "The watermark size and angle have to be numbers, the size larger than 0, and the opacity \
and colour between 0 and 1.";
}

impl BatesOptions {
    /// The label for the page at `page_index` (zero-based, relative to the first stamped page).
    /// Numbers past the largest one stop there, and the padding is at most [`MAX_BATES_DIGITS`];
    /// [`BatesOptions::validate`] refuses options that would need either.
    pub fn label(&self, page_index: usize) -> String {
        let number = self.start_number.saturating_add(page_index as u64);
        format!(
            "{}{:0width$}{}",
            self.prefix,
            number,
            self.suffix,
            width = self.digits.clamp(1, MAX_BATES_DIGITS)
        )
    }

    /// Checks that `pages` pages can be numbered with these options.
    pub fn validate(&self, pages: usize) -> Result<()> {
        if !(1..=MAX_BATES_DIGITS).contains(&self.digits) {
            return Err(Error::Other(messages::BATES_DIGITS.into()));
        }
        if self
            .start_number
            .checked_add(pages.saturating_sub(1) as u64)
            .is_none()
        {
            return Err(Error::Other(messages::BATES_RANGE.into()));
        }
        if !(self.font_size.is_finite() && self.font_size > 0.0 && self.margin.is_finite()) {
            return Err(Error::Other(messages::BATES_TEXT.into()));
        }
        Ok(())
    }
}

impl Pdf {
    /// Stamps a sequential label on every page.
    pub fn apply_bates(&mut self, options: &BatesOptions) -> Result<()> {
        options.validate(self.page_count())?;
        for (index, page_id) in self.page_ids().into_iter().enumerate() {
            let geometry = self.geometry_of(page_id);
            let (w, h) = geometry.display_size();
            let label = options.label(index);
            let text_width = options.font.text_width(&label, options.font_size);
            let x = match options.anchor {
                PageAnchor::TopLeft | PageAnchor::BottomLeft => options.margin,
                PageAnchor::TopCenter | PageAnchor::BottomCenter => w / 2.0 - text_width / 2.0,
                PageAnchor::TopRight | PageAnchor::BottomRight => w - options.margin - text_width,
            };
            let y = if options.anchor.is_top() {
                h - options.margin - options.font.cap_height() * options.font_size
            } else {
                options.margin
            };
            let font_name = self.add_standard_font(page_id, options.font)?;

            let mut content: Vec<u8> = Vec::new();
            let header = format!(
                "q\n{}\n0 g\nBT\n/{} {} Tf\n{} {} Td\n",
                matrix_string(geometry.display_to_user()),
                font_name,
                fmt(options.font_size),
                fmt(x),
                fmt(y),
            );
            content.extend_from_slice(header.as_bytes());
            content.extend_from_slice(&literal_string(&win_ansi(&label)));
            content.extend_from_slice(b" Tj\nET\nQ\n");
            self.append_content(page_id, content)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bates_labels_are_sequential_and_padded() {
        let options = BatesOptions {
            prefix: "ABC-".into(),
            start_number: 41,
            digits: 5,
            ..Default::default()
        };
        assert_eq!(options.label(0), "ABC-00041");
        assert_eq!(options.label(2), "ABC-00043");
    }

    #[test]
    fn bates_options_that_cannot_be_stamped_are_refused() {
        let fine = BatesOptions::default();
        assert!(fine.validate(3).is_ok());
        for digits in [0, 21, 70_000, usize::MAX] {
            let options = BatesOptions {
                digits,
                ..Default::default()
            };
            assert_eq!(
                options.validate(3).unwrap_err().to_string(),
                messages::BATES_DIGITS
            );
            // Formatting never panics, whatever the width asked for.
            assert!(!options.label(0).is_empty());
        }
        let last = BatesOptions {
            start_number: u64::MAX,
            ..Default::default()
        };
        assert!(last.validate(1).is_ok());
        assert_eq!(
            last.validate(2).unwrap_err().to_string(),
            messages::BATES_RANGE
        );
        assert_eq!(last.label(5), u64::MAX.to_string());
        let size = BatesOptions {
            font_size: f32::NAN,
            ..Default::default()
        };
        assert_eq!(
            size.validate(1).unwrap_err().to_string(),
            messages::BATES_TEXT
        );
    }
}
