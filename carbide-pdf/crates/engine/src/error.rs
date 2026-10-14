use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("PDF error: {0}")]
    Pdf(#[from] lopdf::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// `index` is zero-based, as everywhere in the API; the message names the page the way people
    /// count, from 1.
    #[error("page {} does not exist (the document has {count} pages)", .index.saturating_add(1))]
    PageOutOfRange { index: usize, count: usize },

    #[error("invalid page range: {0}")]
    InvalidRange(String),

    #[error("the document is password protected")]
    PasswordRequired,

    #[error("the password is incorrect")]
    WrongPassword,

    #[error("rendering backend: {0}")]
    Render(String),

    /// The document would need more memory than is safe to give it (see [`crate::limits`]).
    #[error("{0}")]
    TooLarge(String),

    #[error("{0}")]
    Unsupported(String),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
