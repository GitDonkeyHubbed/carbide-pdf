//! Limits on what a document can make the engine allocate.
//!
//! A stream compressed a thousand to one is a few hundred kilobytes in the file and gigabytes once
//! decoded, and several pages can share it. Every stream the engine decodes itself is decoded
//! against a budget for the whole operation, which fails with [`Error::TooLarge`] once it is spent;
//! lopdf decodes object and cross-reference streams while it reads a file, each against
//! [`MAX_STREAM_ON_LOAD`]. Rendering has its own limits, in [`crate::render`].

use lopdf::{DecompressError, Document, ObjectId, Stream};

use crate::error::{Error, Result};

/// The most one object or cross-reference stream may decode to while a file is read. A stream
/// beyond it is left unread, as lopdf leaves other streams it cannot decode.
pub const MAX_STREAM_ON_LOAD: usize = 128 << 20;

/// The most one page's content, or one stream, may decode to.
pub const MAX_DECODED_AT_ONCE: usize = 256 << 20;

/// The most content one operation (imposing, reducing, trimming resources) decodes in total.
pub const MAX_DECODED: usize = 1 << 30;

pub mod messages {
    pub const TOO_LARGE: &str = "This document's content is too large to process safely: it decompresses to more than \
256 MB for one page or object, or more than 1 GB in all.";
}

/// What is left of the decoding budget of one operation.
pub(crate) struct DecodeBudget {
    left: usize,
}

impl DecodeBudget {
    pub fn new() -> DecodeBudget {
        DecodeBudget { left: MAX_DECODED }
    }

    /// The decoded data of `stream`, counted against the budget.
    pub fn stream(&mut self, stream: &Stream) -> Result<Vec<u8>> {
        let data = stream.get_plain_content_with_limit(self.at_once()).map_err(too_large)?;
        self.left = self.left.saturating_sub(data.len());
        Ok(data)
    }

    /// The content of the page `page`, its streams decoded and joined, counted against the budget.
    /// A stream that cannot be decoded is used as it is stored, as lopdf does.
    pub fn page_content(&mut self, doc: &Document, page: ObjectId) -> Result<Vec<u8>> {
        let data = doc.get_page_content_with_limit(page, self.at_once()).map_err(too_large)?;
        self.left = self.left.saturating_sub(data.len());
        Ok(data)
    }

    fn at_once(&self) -> usize {
        self.left.min(MAX_DECODED_AT_ONCE)
    }
}

fn too_large(error: lopdf::Error) -> Error {
    match error {
        lopdf::Error::Decompress(DecompressError::MemoryLimitExceeded { .. }) => Error::TooLarge(messages::TOO_LARGE.into()),
        other => Error::Pdf(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Dictionary};

    #[test]
    fn the_budget_covers_every_stream_of_an_operation() {
        let stream = Stream::new(Dictionary::new(), vec![b'x'; 600]);
        let mut budget = DecodeBudget { left: 1000 };
        assert_eq!(budget.stream(&stream).unwrap().len(), 600);
        assert!(matches!(budget.stream(&stream), Err(Error::TooLarge(message)) if message == messages::TOO_LARGE));

        // A stream that cannot be decoded is an error of its own, not a limit.
        let unknown = Stream::new(dictionary! { "Filter" => "JBIG2Decode" }, b"data".to_vec());
        assert!(matches!(DecodeBudget::new().stream(&unknown), Err(Error::Pdf(_))));
    }
}
