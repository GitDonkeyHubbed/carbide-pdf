//! The ribbon: tabs, groups and commands.
//!
//! The arrangement (which tab holds which group, and which commands sit in which group) follows
//! the original product's layout files; only commands this application implements are listed.

use crate::icons::Glyph;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RibbonTab {
    Home,
    Edit,
    View,
    Comment,
    Advanced,
    Security,
    Forms,
}

impl RibbonTab {
    pub const ALL: [RibbonTab; 7] = [
        RibbonTab::Home,
        RibbonTab::Edit,
        RibbonTab::View,
        RibbonTab::Comment,
        RibbonTab::Advanced,
        RibbonTab::Security,
        RibbonTab::Forms,
    ];

    pub fn label(self) -> &'static str {
        match self {
            RibbonTab::Home => "Home",
            RibbonTab::Edit => "Edit",
            RibbonTab::View => "View",
            RibbonTab::Comment => "Comment",
            RibbonTab::Advanced => "Advanced Processing",
            RibbonTab::Security => "Security",
            RibbonTab::Forms => "Forms",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Cmd {
    // quick access and file
    Open,
    Save,
    SaveAs,
    Undo,
    Redo,
    SelectTool,
    ToggleSidebar,
    NewBlank,
    Combine,
    // pages
    PageInsert,
    PageExtract,
    PageDelete,
    Merge,
    Split,
    Imposition,
    RotateLeft,
    RotateRight,
    MoveUp,
    MoveDown,
    // content
    Typewriter,
    Watermark,
    Bates,
    // view
    ZoomIn,
    ZoomOut,
    FitWidth,
    FitPage,
    ActualSize,
    FirstPage,
    PrevPage,
    NextPage,
    LastPage,
    // comment
    Note,
    Highlight,
    HighlightArea,
    Underline,
    Strikeout,
    // advanced
    Reduce,
    Compare,
    Ocr,
    ExportImages,
    ExportText,
    SaveRevision,
    OpenRevision,
    // security
    MarkRedaction,
    ApplyRedactions,
    RemoveHidden,
    SignVisible,
    SignInvisible,
    Certify,
    TimeStamp,
    Verify,
    Protect,
    TrustCertificate,
    // preferences
    AutoSaveSettings,
    AutoRecovery,
    Quit,
}

impl Cmd {
    pub fn label(self) -> &'static str {
        match self {
            Cmd::Open => "Open",
            Cmd::Save => "Save",
            Cmd::SaveAs => "Save As",
            Cmd::Undo => "Undo",
            Cmd::Redo => "Redo",
            Cmd::SelectTool => "Select",
            Cmd::ToggleSidebar => "Panels",
            Cmd::NewBlank => "New Blank",
            Cmd::Combine => "Combine Files",
            Cmd::PageInsert => "Insert Page",
            Cmd::PageExtract => "Extract",
            Cmd::PageDelete => "Delete",
            Cmd::Merge => "Merge PDFs",
            Cmd::Split => "Split File",
            Cmd::Imposition => "Imposition",
            Cmd::RotateLeft => "Rotate Left",
            Cmd::RotateRight => "Rotate Right",
            Cmd::MoveUp => "Move Up",
            Cmd::MoveDown => "Move Down",
            Cmd::Typewriter => "Typewriter",
            Cmd::Watermark => "Watermark",
            Cmd::Bates => "Bates Number",
            Cmd::ZoomIn => "Zoom In",
            Cmd::ZoomOut => "Zoom Out",
            Cmd::FitWidth => "Fit Width",
            Cmd::FitPage => "Fit Page",
            Cmd::ActualSize => "Actual Size",
            Cmd::FirstPage => "First Page",
            Cmd::PrevPage => "Previous",
            Cmd::NextPage => "Next",
            Cmd::LastPage => "Last Page",
            Cmd::Note => "Note",
            Cmd::Highlight => "Highlight",
            Cmd::HighlightArea => "Highlight Area",
            Cmd::Underline => "Underline",
            Cmd::Strikeout => "Crossout",
            Cmd::Reduce => "Reduce File",
            Cmd::Compare => "Compare",
            Cmd::Ocr => "OCR",
            Cmd::ExportImages => "To Image",
            Cmd::ExportText => "To Text",
            Cmd::SaveRevision => "Save Revision",
            Cmd::OpenRevision => "Open Revision",
            Cmd::MarkRedaction => "Mark Redaction",
            Cmd::ApplyRedactions => "Apply",
            Cmd::RemoveHidden => "Remove Hidden Info",
            Cmd::SignVisible => "Sign",
            Cmd::SignInvisible => "Sign Invisibly",
            Cmd::Certify => "Certify",
            Cmd::TimeStamp => "Time Stamp",
            Cmd::Verify => "Validate",
            Cmd::Protect => "Protect PDF",
            Cmd::TrustCertificate => "Trusted Identity",
            Cmd::AutoSaveSettings => "Auto Save...",
            Cmd::AutoRecovery => "Auto Recovery",
            Cmd::Quit => "Quit",
        }
    }

    pub fn tooltip(self) -> &'static str {
        match self {
            Cmd::PageInsert => "Insert a blank page after the current page",
            Cmd::PageExtract => "Save the current page as a new file",
            Cmd::Merge => "Append other PDF files to this document",
            Cmd::Split => "Split the document by page ranges",
            Cmd::Imposition => "Create a PDF with booklet, n-up or step-and-repeat sheets",
            Cmd::Typewriter => "Place text anywhere on the page",
            Cmd::Bates => "Add sequential identification numbers to every page",
            Cmd::Reduce => "Reduce the file size of this document or several files",
            Cmd::Compare => "Compare the text of this document with another PDF",
            Cmd::Ocr => "Make scanned pages searchable",
            Cmd::MarkRedaction => "Drag over content to mark it for permanent removal",
            Cmd::ApplyRedactions => "Permanently remove all marked content",
            Cmd::RemoveHidden => "Remove metadata, attachments, scripts, comments and other information the pages do not show",
            Cmd::SignVisible => "Drag a rectangle on the page to place a signature",
            Cmd::SignInvisible => "Sign without a visible appearance",
            Cmd::Certify => "Certify the document and limit later changes",
            Cmd::TimeStamp => "Add a time stamp from a time stamp authority",
            Cmd::Verify => "Check every signature in the document",
            Cmd::Protect => "Save a password-protected copy",
            Cmd::TrustCertificate => "Add a certificate to your list of trusted identities",
            Cmd::SaveRevision => "Save the document as a new named revision",
            Cmd::OpenRevision => "Open an earlier revision in a new window",
            Cmd::HighlightArea => "Highlight a rectangular area",
            _ => "",
        }
    }

    pub fn glyph(self) -> Glyph {
        match self {
            Cmd::Open => Glyph::Open,
            Cmd::Save | Cmd::SaveAs => Glyph::Save,
            Cmd::Undo => Glyph::Undo,
            Cmd::Redo => Glyph::Redo,
            Cmd::SelectTool => Glyph::Cursor,
            Cmd::ToggleSidebar => Glyph::Sidebar,
            Cmd::NewBlank => Glyph::NewDocument,
            Cmd::Combine | Cmd::Merge => Glyph::Merge,
            Cmd::PageInsert => Glyph::PageInsert,
            Cmd::PageExtract => Glyph::PageExtract,
            Cmd::PageDelete => Glyph::PageDelete,
            Cmd::Split => Glyph::Split,
            Cmd::Imposition => Glyph::Document,
            Cmd::RotateLeft => Glyph::RotateLeft,
            Cmd::RotateRight => Glyph::RotateRight,
            Cmd::MoveUp => Glyph::MovePageUp,
            Cmd::MoveDown => Glyph::MovePageDown,
            Cmd::Typewriter => Glyph::TextBox,
            Cmd::Watermark => Glyph::Stamp,
            Cmd::Bates => Glyph::Number,
            Cmd::ZoomIn => Glyph::ZoomIn,
            Cmd::ZoomOut => Glyph::ZoomOut,
            Cmd::FitWidth => Glyph::FitWidth,
            Cmd::FitPage => Glyph::FitPage,
            Cmd::ActualSize => Glyph::ActualSize,
            Cmd::FirstPage => Glyph::FirstPage,
            Cmd::PrevPage => Glyph::PrevPage,
            Cmd::NextPage => Glyph::NextPage,
            Cmd::LastPage => Glyph::LastPage,
            Cmd::Note => Glyph::Note,
            Cmd::Highlight => Glyph::Highlight,
            Cmd::HighlightArea => Glyph::HighlightArea,
            Cmd::Underline => Glyph::Underline,
            Cmd::Strikeout => Glyph::Strikeout,
            Cmd::Reduce => Glyph::Reduce,
            Cmd::Compare => Glyph::Compare,
            Cmd::Ocr => Glyph::Scan,
            Cmd::ExportImages => Glyph::ImageExport,
            Cmd::ExportText => Glyph::TextExport,
            Cmd::SaveRevision | Cmd::OpenRevision => Glyph::Revision,
            Cmd::MarkRedaction => Glyph::Redact,
            Cmd::ApplyRedactions => Glyph::ApplyRedact,
            Cmd::RemoveHidden => Glyph::Sanitize,
            Cmd::SignVisible | Cmd::SignInvisible => Glyph::Sign,
            Cmd::Certify => Glyph::Certify,
            Cmd::TimeStamp => Glyph::TimeStamp,
            Cmd::Verify => Glyph::Verify,
            Cmd::Protect => Glyph::Lock,
            Cmd::TrustCertificate => Glyph::Trust,
            Cmd::AutoSaveSettings => Glyph::Settings,
            Cmd::AutoRecovery => Glyph::Recovery,
            Cmd::Quit => Glyph::Close,
        }
    }
}

pub struct Group {
    pub name: &'static str,
    pub commands: &'static [Cmd],
}

const HOME: &[Group] = &[
    Group { name: "File", commands: &[Cmd::Open, Cmd::Save, Cmd::SaveAs, Cmd::NewBlank, Cmd::Combine] },
    Group { name: "Pages", commands: &[Cmd::PageInsert, Cmd::PageExtract, Cmd::PageDelete, Cmd::Merge, Cmd::Imposition] },
    Group { name: "Rotate", commands: &[Cmd::RotateRight, Cmd::RotateLeft, Cmd::MoveUp, Cmd::MoveDown] },
    Group { name: "Tools", commands: &[Cmd::Typewriter, Cmd::Reduce, Cmd::Split] },
];

const EDIT: &[Group] = &[
    Group { name: "Basic", commands: &[Cmd::Undo, Cmd::Redo, Cmd::SelectTool] },
    Group { name: "Content Editing", commands: &[Cmd::Typewriter, Cmd::Watermark, Cmd::Bates] },
];

const VIEW: &[Group] = &[
    Group { name: "Zoom", commands: &[Cmd::ZoomIn, Cmd::ZoomOut, Cmd::FitWidth, Cmd::FitPage, Cmd::ActualSize] },
    Group { name: "Page View", commands: &[Cmd::FirstPage, Cmd::PrevPage, Cmd::NextPage, Cmd::LastPage] },
    Group { name: "Panels", commands: &[Cmd::ToggleSidebar] },
];

const COMMENT: &[Group] = &[
    Group { name: "Comment", commands: &[Cmd::Note, Cmd::Typewriter] },
    Group {
        name: "Text Markup",
        commands: &[Cmd::Highlight, Cmd::HighlightArea, Cmd::Strikeout, Cmd::Underline],
    },
];

const ADVANCED: &[Group] = &[
    Group { name: "Document Processing", commands: &[Cmd::Reduce, Cmd::Split, Cmd::Compare] },
    Group { name: "Convert", commands: &[Cmd::Ocr, Cmd::ExportImages, Cmd::ExportText] },
    Group { name: "Revisions", commands: &[Cmd::SaveRevision, Cmd::OpenRevision] },
    Group { name: "Preferences", commands: &[Cmd::AutoSaveSettings, Cmd::AutoRecovery] },
];

const SECURITY: &[Group] = &[
    Group { name: "Redaction", commands: &[Cmd::MarkRedaction, Cmd::ApplyRedactions, Cmd::RemoveHidden] },
    Group {
        name: "Signature",
        commands: &[Cmd::SignVisible, Cmd::SignInvisible, Cmd::TimeStamp, Cmd::Certify, Cmd::Verify],
    },
    Group { name: "Secure", commands: &[Cmd::Protect] },
    Group { name: "Trusted", commands: &[Cmd::TrustCertificate] },
];

const FORMS: &[Group] = &[];

pub fn groups(tab: RibbonTab) -> &'static [Group] {
    match tab {
        RibbonTab::Home => HOME,
        RibbonTab::Edit => EDIT,
        RibbonTab::View => VIEW,
        RibbonTab::Comment => COMMENT,
        RibbonTab::Advanced => ADVANCED,
        RibbonTab::Security => SECURITY,
        RibbonTab::Forms => FORMS,
    }
}

/// Every command that appears on some ribbon tab, for "Find a Tool".
pub fn all_commands() -> Vec<Cmd> {
    let mut out: Vec<Cmd> = Vec::new();
    for tab in RibbonTab::ALL {
        for group in groups(tab) {
            for cmd in group.commands {
                if !out.contains(cmd) {
                    out.push(*cmd);
                }
            }
        }
    }
    out.push(Cmd::SaveAs);
    out
}
