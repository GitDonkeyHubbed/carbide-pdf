//! `carbide`: command-line front end for the Carbide PDF engine.
//!
//! Page numbers on the command line are 1-based; rectangles are `PAGE:X,Y,W,H` in points with the
//! origin at the bottom-left of the displayed page.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use carbide_engine::imposition::{ImpositionOptions, Margins, Media, RegistrationStyle};
use carbide_engine::imposition_plan::{Binding, LayoutKind, PlanOptions, PrintingOrder, Style};
use carbide_engine::reduce::{reduce_files, OutputOptions, ReduceOptions, TargetFolder};
use carbide_engine::signature::{
    notice_text, CertifyLevel, HttpTimestampSource, Identity, SignOptions, SubFilter, TrustStore, Validity,
};
use carbide_engine::{
    compare_documents, export_images, export_text, open_revision_bytes, pdf_date_now, revision_display_name,
    BatesOptions, ImageFormat, MarkupStyle, PageAnchor, Pdf, ProtectionOptions, Rect, Redaction, Renderer,
    SavePlan, SavedFile, StandardFont, WatermarkOptions,
};
use clap::{Args, Parser, Subcommand, ValueEnum};

type Failure = Box<dyn std::error::Error>;

#[derive(Parser)]
#[command(name = "carbide", version, about = "Carbide PDF: edit, protect, impose and reduce PDF files")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Clone)]
struct Password {
    /// Password to open the input file, if it is protected.
    #[arg(long, global = true)]
    password: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Show page count, page sizes and revisions.
    Info { input: PathBuf, #[command(flatten)] pw: Password },
    /// Join several PDFs into one.
    Merge { output: PathBuf, inputs: Vec<PathBuf> },
    /// Copy selected pages (e.g. "1-3,5") into a new file.
    Extract { input: PathBuf, pages: String, output: PathBuf, #[command(flatten)] pw: Password },
    /// Split into one file per range (e.g. "1-3,4-6") inside a folder.
    Split { input: PathBuf, ranges: String, folder: PathBuf, #[command(flatten)] pw: Password },
    /// Delete pages.
    Delete { input: PathBuf, pages: String, output: PathBuf, #[command(flatten)] pw: Password },
    /// Rotate pages by a multiple of 90 degrees.
    Rotate { input: PathBuf, pages: String, degrees: i32, output: PathBuf, #[command(flatten)] pw: Password },
    /// Insert a blank page so that it becomes page AT.
    Blank { input: PathBuf, at: usize, output: PathBuf, #[command(flatten)] pw: Password },
    /// Move a page to a new position.
    Move { input: PathBuf, from: usize, to: usize, output: PathBuf, #[command(flatten)] pw: Password },
    /// Add a text watermark.
    Watermark(WatermarkArgs),
    /// Add Bates numbers.
    Bates(BatesArgs),
    /// Permanently remove content under rectangles.
    Redact {
        input: PathBuf,
        output: PathBuf,
        /// `PAGE:X,Y,W,H`, repeatable.
        #[arg(long = "rect", required = true)]
        rects: Vec<String>,
        #[arg(long, default_value_t = 200.0)]
        dpi: f32,
        #[command(flatten)]
        pw: Password,
    },
    /// Highlight, underline or strike out a rectangle.
    Markup {
        input: PathBuf,
        output: PathBuf,
        #[arg(long)]
        rect: String,
        #[arg(long, value_enum, default_value = "highlight")]
        style: MarkupArg,
        #[command(flatten)]
        pw: Password,
    },
    /// Add a sticky note or a text box at a point (`PAGE:X,Y`, top-left anchored).
    Note {
        input: PathBuf,
        output: PathBuf,
        #[arg(long)]
        at: String,
        text: String,
        /// Draw the text on the page instead of a sticky note.
        #[arg(long)]
        on_page: bool,
        #[arg(long, default_value_t = 14.0)]
        size: f32,
        #[command(flatten)]
        pw: Password,
    },
    /// Compare the text of two documents.
    Compare { original: PathBuf, revised: PathBuf },
    /// Export every page as an image.
    Images {
        input: PathBuf,
        folder: PathBuf,
        #[arg(long, default_value_t = 150.0)]
        dpi: f32,
        #[arg(long)]
        jpeg: bool,
        #[command(flatten)]
        pw: Password,
    },
    /// Print the document's text.
    Text { input: PathBuf, #[command(flatten)] pw: Password },
    /// Save a password-protected copy (AES-256).
    Encrypt {
        input: PathBuf,
        output: PathBuf,
        #[arg(long, default_value = "")]
        user: String,
        #[arg(long)]
        owner: String,
        #[arg(long)]
        no_print: bool,
        #[arg(long)]
        no_copy: bool,
        #[command(flatten)]
        pw: Password,
    },
    /// Remove password protection (needs the password).
    Decrypt { input: PathBuf, output: PathBuf, #[arg(long)] password: String },
    /// Reduce file size (image slider 0 to 100, default 50).
    Reduce {
        inputs: Vec<PathBuf>,
        #[arg(long, default_value_t = 50)]
        slider: u8,
        /// Write results here instead of next to the originals.
        #[arg(long)]
        out_dir: Option<PathBuf>,
        #[arg(long, default_value = "", allow_hyphen_values = true)]
        prefix: String,
        #[arg(long, default_value = "", allow_hyphen_values = true)]
        suffix: String,
        /// Refuse to overwrite existing files.
        #[arg(long)]
        no_overwrite: bool,
    },
    /// Recognize text on scanned pages and add an invisible, searchable text layer.
    Ocr {
        input: PathBuf,
        output: PathBuf,
        #[arg(long, default_value_t = 300.0)]
        dpi: f32,
        /// Folder with text-detection.rten and text-recognition.rten (or set CARBIDE_OCR_MODELS).
        #[arg(long)]
        models: Option<PathBuf>,
        /// Pages to consider (default: all).
        #[arg(long)]
        pages: Option<String>,
        #[command(flatten)]
        pw: Password,
    },
    /// Impose pages onto sheets (booklets, n-up, step and repeat, cut stacks).
    Impose(ImposeArgs),
    /// Digitally sign a document (adds an incremental update; earlier signatures stay valid).
    Sign(SignArgs),
    /// Add a document time stamp from an RFC 3161 time stamp authority.
    Timestamp {
        input: PathBuf,
        output: PathBuf,
        #[arg(long)]
        tsa: String,
        #[arg(long)]
        tsa_user: Option<String>,
        /// Time-stamp authority password (or set CARBIDE_TSA_PASSWORD).
        #[arg(long)]
        tsa_password: Option<String>,
    },
    /// Validate every signature in a file.
    Verify {
        input: PathBuf,
        /// PEM certificate(s) to treat as trusted; repeatable.
        #[arg(long = "trust")]
        trust: Vec<PathBuf>,
    },
    /// Named revisions stored inside the file.
    Revision {
        #[command(subcommand)]
        action: RevisionAction,
    },
}

#[derive(Subcommand)]
enum RevisionAction {
    /// Append a new labelled revision to the file (the file is rewritten in place).
    Save { file: PathBuf, label: String },
    List { file: PathBuf },
    /// Write the document as it was at revision NUMBER (1-based) to OUTPUT.
    Open { file: PathBuf, number: usize, output: PathBuf },
}

#[derive(Args)]
struct SignArgs {
    input: PathBuf,
    output: PathBuf,
    /// PKCS#12 identity file (.p12 / .pfx).
    #[arg(long)]
    p12: Option<PathBuf>,
    /// PKCS#12 password (or set CARBIDE_P12_PASSWORD).
    #[arg(long)]
    p12_password: Option<String>,
    /// PEM certificate chain (signer first) and PEM private key, instead of --p12.
    #[arg(long)]
    cert: Option<PathBuf>,
    #[arg(long)]
    key: Option<PathBuf>,
    /// Use the CAdES-equivalent format instead of PKCS#7 detached.
    #[arg(long)]
    cades: bool,
    /// Certify with permission level 1 (no changes), 2 (form filling) or 3 (commenting).
    #[arg(long)]
    certify: Option<u8>,
    /// Lock the document after signing.
    #[arg(long)]
    lock: bool,
    #[arg(long)]
    reason: Option<String>,
    #[arg(long)]
    location: Option<String>,
    #[arg(long)]
    contact: Option<String>,
    /// Visible signature rectangle `PAGE:X,Y,W,H` (default: invisible).
    #[arg(long)]
    rect: Option<String>,
    /// Embed a time stamp from this RFC 3161 authority.
    #[arg(long)]
    tsa: Option<String>,
    #[arg(long)]
    tsa_user: Option<String>,
    /// Time-stamp authority password (or set CARBIDE_TSA_PASSWORD).
    #[arg(long)]
    tsa_password: Option<String>,
    #[command(flatten)]
    pw: Password,
}

#[derive(Args)]
struct WatermarkArgs {
    input: PathBuf,
    output: PathBuf,
    #[arg(long)]
    text: String,
    #[arg(long, default_value_t = 72.0)]
    size: f32,
    #[arg(long, default_value_t = 0.3)]
    opacity: f32,
    #[arg(long, default_value_t = 45.0)]
    angle: f32,
    /// Hex colour such as 999999.
    #[arg(long, default_value = "999999")]
    color: String,
    /// Pages to stamp (default: all).
    #[arg(long)]
    pages: Option<String>,
    #[command(flatten)]
    pw: Password,
}

#[derive(Args)]
struct BatesArgs {
    input: PathBuf,
    output: PathBuf,
    #[arg(long, default_value = "")]
    prefix: String,
    #[arg(long, default_value = "")]
    suffix: String,
    #[arg(long, default_value_t = 1)]
    start: u64,
    #[arg(long, default_value_t = 6)]
    digits: usize,
    #[arg(long, value_enum, default_value = "bottom-right")]
    position: AnchorArg,
    #[arg(long, default_value_t = 10.0)]
    size: f32,
    #[command(flatten)]
    pw: Password,
}

#[derive(Args)]
struct ImposeArgs {
    input: PathBuf,
    output: PathBuf,
    /// Load all options from a settings file written with --save-settings.
    #[arg(long)]
    settings: Option<PathBuf>,
    #[arg(long)]
    save_settings: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "booklet2")]
    layout: LayoutArg,
    #[arg(long, default_value_t = 2)]
    columns: usize,
    #[arg(long, default_value_t = 1)]
    rows: usize,
    #[arg(long)]
    perfect_bound: bool,
    #[arg(long, default_value_t = 1)]
    sheets_per_fold: usize,
    #[arg(long, value_enum, default_value = "sheetwise")]
    style: StyleArg,
    #[arg(long)]
    single_sided: bool,
    #[arg(long, value_enum, default_value = "interleaved")]
    printing_order: OrderArg,
    /// Media name (A4, Letter, ...) or WIDTHxHEIGHT in points, or "auto".
    #[arg(long, default_value = "Letter")]
    media: String,
    #[arg(long)]
    no_fit: bool,
    #[arg(long, default_value_t = 100.0)]
    scale_x: f32,
    #[arg(long, default_value_t = 100.0)]
    scale_y: f32,
    #[arg(long, default_value_t = 0.0)]
    margin: f32,
    #[arg(long, default_value_t = 0.0)]
    gutter: f32,
    #[arg(long, default_value_t = 0.0)]
    creep: f32,
    #[arg(long)]
    crop_marks: bool,
    #[arg(long)]
    registration_marks: bool,
    #[arg(long)]
    flip_backs: bool,
    #[command(flatten)]
    pw: Password,
}

#[derive(Clone, Copy, ValueEnum)]
enum MarkupArg {
    Highlight,
    Underline,
    Strikeout,
}

#[derive(Clone, Copy, ValueEnum)]
enum AnchorArg {
    TopLeft,
    TopCenter,
    TopRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

#[derive(Clone, Copy, ValueEnum)]
enum LayoutArg {
    Booklet2,
    Booklet4,
    Booklet8,
    Sequential,
    StepAndRepeat,
    CutStacks,
}

#[derive(Clone, Copy, ValueEnum)]
enum StyleArg {
    Sheetwise,
    WorkAndTurn,
    WorkAndTumble,
}

#[derive(Clone, Copy, ValueEnum)]
enum OrderArg {
    Interleaved,
    FrontsThenBacks,
    FrontsThenBacksReversed,
}

// ---- helpers -------------------------------------------------------------------------------

/// A secret from a command-line flag when given, else from `env_var`. Keeping secrets out of argv
/// (and shell history) is why the flags are optional.
fn secret(flag: Option<String>, env_var: &str) -> Option<String> {
    flag.or_else(|| std::env::var(env_var).ok())
}

fn open(path: &Path, pw: &Password) -> Result<Pdf, Failure> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(match &pw.password {
        Some(p) => Pdf::from_bytes_with_password(&bytes, p)?,
        None => Pdf::from_bytes(&bytes)?,
    })
}

fn renderer() -> Result<&'static Renderer, Failure> {
    Ok(Renderer::global()?)
}

fn pages_arg(text: &str, pdf: &Pdf) -> Result<Vec<usize>, Failure> {
    Ok(Pdf::parse_ranges(text, pdf.page_count())?.into_iter().flatten().collect())
}

fn one_based(n: usize, what: &str) -> Result<usize, Failure> {
    n.checked_sub(1).ok_or_else(|| format!("{what} must be 1 or more").into())
}

fn parse_numbers(text: &str, expected: usize) -> Result<Vec<f32>, Failure> {
    let values: Result<Vec<f32>, _> = text.split(',').map(|s| s.trim().parse::<f32>()).collect();
    let values = values.map_err(|_| format!("expected {expected} numbers, got {text:?}"))?;
    if values.len() != expected {
        return Err(format!("expected {expected} numbers, got {text:?}").into());
    }
    Ok(values)
}

/// `PAGE:X,Y,W,H` -> (zero-based page, rect)
fn parse_rect(text: &str) -> Result<(usize, Rect), Failure> {
    let (page, numbers) = text.split_once(':').ok_or("rectangle must look like PAGE:X,Y,W,H")?;
    let page = one_based(page.trim().parse::<usize>()?, "page")?;
    let v = parse_numbers(numbers, 4)?;
    Ok((page, Rect::new(v[0], v[1], v[2], v[3])))
}

fn parse_point(text: &str) -> Result<(usize, f32, f32), Failure> {
    let (page, numbers) = text.split_once(':').ok_or("point must look like PAGE:X,Y")?;
    let page = one_based(page.trim().parse::<usize>()?, "page")?;
    let v = parse_numbers(numbers, 2)?;
    Ok((page, v[0], v[1]))
}

fn parse_color(hex: &str) -> Result<[f32; 3], Failure> {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 {
        return Err("colour must be six hex digits, e.g. 999999".into());
    }
    let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map(|v| v as f32 / 255.0);
    Ok([channel(0)?, channel(2)?, channel(4)?])
}

fn save(pdf: &mut Pdf, output: &Path) -> Result<(), Failure> {
    pdf.save(output)?;
    println!("wrote {}", output.display());
    Ok(())
}

/// Opens an input that is edited and written out again, together with the file it came from.
fn open_for_edit(path: &Path, pw: &Password) -> Result<(Pdf, SavedFile), Failure> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(SavedFile::open(bytes, pw.password.clone())?)
}

/// Writes an edited input to `output` the way the desktop app saves it. A protected input stays
/// encrypted with its own key, passwords and permissions. A signed input, or one with saved
/// revisions, gets the edits appended so those stay valid, unless the edit removed content
/// (`content_removed`): then it is written in full, and a note says what that costs.
fn save_edit(pdf: &mut Pdf, saved: &SavedFile, content_removed: bool, input: &Path, output: &Path) -> Result<(), Failure> {
    let plan = pdf.save_plan(Some(saved), content_removed)?;
    if let SavePlan::FullRewrite { breaks_signatures, drops_revisions } = plan {
        if breaks_signatures {
            eprintln!("note: {} is written in full, so its signatures no longer validate", output.display());
        }
        if drops_revisions {
            eprintln!("note: {} is written in full, so its saved revisions are dropped", output.display());
        }
    }
    let bytes = pdf.save_bytes(plan, Some(saved))?;
    if !(plan == SavePlan::Unchanged && same_file(input, output)) {
        carbide_engine::fs::write_replacing(output, &bytes)?;
    }
    println!("wrote {}", output.display());
    Ok(())
}

/// True when `a` and `b` name the same existing file, links followed.
fn same_file(a: &Path, b: &Path) -> bool {
    matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

/// Refuses to write a new, unencrypted document over the password-protected `input` it came from.
fn refuse_over_protected(input: &Path, pdf: &Pdf, output: &Path) -> Result<(), Failure> {
    if pdf.document().was_encrypted() && same_file(input, output) {
        return Err(format!(
            "{} is password-protected, and this result would not be encrypted: choose another output file \
             (or remove the protection with `decrypt`)",
            input.display()
        )
        .into());
    }
    Ok(())
}

// ---- commands ------------------------------------------------------------------------------

fn run(cli: Cli) -> Result<(), Failure> {
    match cli.command {
        Command::Info { input, pw } => {
            let pdf = open(&input, &pw)?;
            println!("{}: {} page(s)", input.display(), pdf.page_count());
            for i in 0..pdf.page_count() {
                let g = pdf.page_geometry(i)?;
                let (w, h) = g.display_size();
                println!("  page {}: {:.0} x {:.0} pt, rotation {}", i + 1, w, h, g.rotation);
            }
            for (n, revision) in pdf.revisions().iter().enumerate() {
                println!("  revision {}: {:?} saved {}", n + 1, revision.label, revision.display_time());
            }
        }
        Command::Merge { output, inputs } => {
            let docs: Result<Vec<Pdf>, Failure> = inputs.iter().map(|p| open(p, &Password { password: None })).collect();
            let docs = docs?;
            for (input, doc) in inputs.iter().zip(&docs) {
                refuse_over_protected(input, doc, &output)?;
            }
            let refs: Vec<&Pdf> = docs.iter().collect();
            save(&mut Pdf::merge(&refs)?, &output)?;
        }
        Command::Extract { input, pages, output, pw } => {
            let pdf = open(&input, &pw)?;
            refuse_over_protected(&input, &pdf, &output)?;
            let selected = pages_arg(&pages, &pdf)?;
            save(&mut pdf.extract_pages(&selected)?, &output)?;
        }
        Command::Split { input, ranges, folder, pw } => {
            let pdf = open(&input, &pw)?;
            std::fs::create_dir_all(&folder)?;
            let stem = input.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            for range in Pdf::parse_ranges(&ranges, pdf.page_count())? {
                let name = format!("{stem}-pages-{}-{}.pdf", range.start() + 1, range.end() + 1);
                refuse_over_protected(&input, &pdf, &folder.join(&name))?;
                let mut part = pdf.extract_pages(&range.clone().collect::<Vec<_>>())?;
                save(&mut part, &folder.join(name))?;
            }
        }
        Command::Delete { input, pages, output, pw } => {
            let (mut pdf, saved) = open_for_edit(&input, &pw)?;
            let selected = pages_arg(&pages, &pdf)?;
            pdf.delete_pages(&selected)?;
            save_edit(&mut pdf, &saved, true, &input, &output)?;
        }
        Command::Rotate { input, pages, degrees, output, pw } => {
            let (mut pdf, saved) = open_for_edit(&input, &pw)?;
            let selected = pages_arg(&pages, &pdf)?;
            pdf.rotate_pages(&selected, degrees)?;
            save_edit(&mut pdf, &saved, false, &input, &output)?;
        }
        Command::Blank { input, at, output, pw } => {
            let (mut pdf, saved) = open_for_edit(&input, &pw)?;
            pdf.insert_blank_page(one_based(at, "position")?, None)?;
            save_edit(&mut pdf, &saved, false, &input, &output)?;
        }
        Command::Move { input, from, to, output, pw } => {
            let (mut pdf, saved) = open_for_edit(&input, &pw)?;
            pdf.move_page(one_based(from, "from")?, one_based(to, "to")?)?;
            save_edit(&mut pdf, &saved, false, &input, &output)?;
        }
        Command::Watermark(a) => {
            let (mut pdf, saved) = open_for_edit(&a.input, &a.pw)?;
            let mut options = WatermarkOptions::new(a.text);
            options.font = StandardFont::HelveticaBold;
            options.font_size = a.size;
            options.opacity = a.opacity;
            options.rotation_degrees = a.angle;
            options.color = parse_color(&a.color)?;
            if let Some(p) = &a.pages {
                options.pages = Some(pages_arg(p, &pdf)?);
            }
            pdf.apply_watermark(&options)?;
            save_edit(&mut pdf, &saved, false, &a.input, &a.output)?;
        }
        Command::Bates(a) => {
            let (mut pdf, saved) = open_for_edit(&a.input, &a.pw)?;
            let anchor = match a.position {
                AnchorArg::TopLeft => PageAnchor::TopLeft,
                AnchorArg::TopCenter => PageAnchor::TopCenter,
                AnchorArg::TopRight => PageAnchor::TopRight,
                AnchorArg::BottomLeft => PageAnchor::BottomLeft,
                AnchorArg::BottomCenter => PageAnchor::BottomCenter,
                AnchorArg::BottomRight => PageAnchor::BottomRight,
            };
            pdf.apply_bates(&BatesOptions {
                prefix: a.prefix,
                suffix: a.suffix,
                start_number: a.start,
                digits: a.digits,
                anchor,
                font_size: a.size,
                ..Default::default()
            })?;
            save_edit(&mut pdf, &saved, false, &a.input, &a.output)?;
        }
        Command::Redact { input, output, rects, dpi, pw } => {
            let (mut pdf, saved) = open_for_edit(&input, &pw)?;
            let parsed: Result<Vec<Redaction>, Failure> = rects
                .iter()
                .map(|r| parse_rect(r).map(|(page, rect)| Redaction { page, rect }))
                .collect();
            pdf.apply_redactions(renderer()?, &parsed?, dpi)?;
            save_edit(&mut pdf, &saved, true, &input, &output)?;
        }
        Command::Markup { input, output, rect, style, pw } => {
            let (mut pdf, saved) = open_for_edit(&input, &pw)?;
            let (page, rect) = parse_rect(&rect)?;
            let style = match style {
                MarkupArg::Highlight => MarkupStyle::Highlight,
                MarkupArg::Underline => MarkupStyle::Underline,
                MarkupArg::Strikeout => MarkupStyle::StrikeOut,
            };
            pdf.add_markup(page, &[rect], style, None)?;
            save_edit(&mut pdf, &saved, false, &input, &output)?;
        }
        Command::Note { input, output, at, text, on_page, size, pw } => {
            let (mut pdf, saved) = open_for_edit(&input, &pw)?;
            let (page, x, y) = parse_point(&at)?;
            if on_page {
                pdf.add_text_box(page, x, y, &text, size)?;
            } else {
                pdf.add_note(page, x, y, &text)?;
            }
            save_edit(&mut pdf, &saved, false, &input, &output)?;
        }
        Command::Compare { original, revised } => {
            let mut a = open(&original, &Password { password: None })?;
            let mut b = open(&revised, &Password { password: None })?;
            let diffs = compare_documents(renderer()?, &mut a, &mut b)?;
            let changed: Vec<_> = diffs.iter().filter(|d| !d.is_identical()).collect();
            if changed.is_empty() {
                println!("No text differences found.");
            }
            for diff in changed {
                println!("Page {}", diff.page_number);
                diff.removed.iter().for_each(|l| println!("  - {l}"));
                diff.added.iter().for_each(|l| println!("  + {l}"));
            }
        }
        Command::Images { input, folder, dpi, jpeg, pw } => {
            let mut pdf = open(&input, &pw)?;
            let stem = input.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let format = if jpeg { ImageFormat::Jpeg } else { ImageFormat::Png };
            for path in export_images(renderer()?, &mut pdf, &folder, &stem, format, dpi)? {
                println!("wrote {}", path.display());
            }
        }
        Command::Text { input, pw } => {
            let mut pdf = open(&input, &pw)?;
            println!("{}", export_text(renderer()?, &mut pdf)?);
        }
        Command::Encrypt { input, output, user, owner, no_print, no_copy, pw } => {
            let mut pdf = open(&input, &pw)?;
            let mut options = ProtectionOptions::new(user, owner);
            options.allow_printing = !no_print;
            options.allow_copying = !no_copy;
            pdf.save_encrypted(&output, &options)?;
            println!("wrote {}", output.display());
        }
        Command::Decrypt { input, output, password } => {
            let mut pdf = open(&input, &Password { password: Some(password) })?;
            save(&mut pdf, &output)?;
        }
        Command::Reduce { inputs, slider, out_dir, prefix, suffix, no_overwrite } => {
            let output = OutputOptions {
                folder: out_dir.map(TargetFolder::Specific).unwrap_or(TargetFolder::SameAsOriginal),
                prefix,
                suffix,
                overwrite: !no_overwrite,
            };
            let mut warnings = 0;
            for outcome in reduce_files(&inputs, &output, &ReduceOptions { image_factor: slider }) {
                match (&outcome.warning, &outcome.report, &outcome.output) {
                    (None, Some(report), Some(path)) => println!(
                        "{} -> {} ({} -> {} bytes, {} images re-encoded, {} streams merged)",
                        outcome.input.display(),
                        path.display(),
                        report.size_before,
                        report.size_after,
                        report.images_recompressed,
                        report.streams_merged
                    ),
                    (Some(message), _, _) => {
                        warnings += 1;
                        eprintln!("{}: {}", outcome.input.display(), message.replace('\n', " "));
                    }
                    _ => {}
                }
            }
            if warnings > 0 {
                return Err(format!("{warnings} file(s) could not be reduced").into());
            }
        }
        Command::Ocr { input, output, dpi, models, pages, pw } => {
            let (mut pdf, saved) = open_for_edit(&input, &pw)?;
            let options = carbide_engine::ocr::OcrOptions {
                dpi,
                models_dir: models,
                pages: match pages {
                    Some(p) => Some(pages_arg(&p, &pdf)?),
                    None => None,
                },
                ..Default::default()
            };
            let report = pdf.make_searchable(renderer()?, &options)?;
            println!(
                "{} page(s) recognized ({} text lines), {} already had text",
                report.recognized_pages.len(),
                report.lines,
                report.skipped_pages.len()
            );
            save_edit(&mut pdf, &saved, false, &input, &output)?;
        }
        Command::Impose(a) => {
            let pdf = open(&a.input, &a.pw)?;
            refuse_over_protected(&a.input, &pdf, &a.output)?;
            let mut options = match &a.settings {
                Some(path) => ImpositionOptions::from_ini(&std::fs::read_to_string(path)?)?,
                None => impose_options(&a)?,
            };
            if a.settings.is_none() {
                options.description = None;
            }
            if let Some(path) = &a.save_settings {
                std::fs::write(path, options.to_ini())?;
            }
            let mut last = 0;
            let mut progress = |percent: u32| {
                if percent / 10 != last / 10 {
                    eprint!("\r{percent}%");
                }
                last = percent;
                false
            };
            let mut imposed = pdf.impose(&options, Some(&mut progress))?;
            eprintln!("\r100%");
            save(&mut imposed, &a.output)?;
        }
        Command::Sign(a) => {
            let identity = match (&a.p12, &a.cert, &a.key) {
                (Some(p12), _, _) => {
                    let password = secret(a.p12_password.clone(), "CARBIDE_P12_PASSWORD").unwrap_or_default();
                    Identity::from_pkcs12(&std::fs::read(p12)?, &password)?
                }
                (None, Some(cert), Some(key)) => {
                    Identity::from_pem(&std::fs::read_to_string(cert)?, &std::fs::read_to_string(key)?)?
                }
                _ => return Err("give --p12, or --cert and --key".into()),
            };
            let base = std::fs::read(&a.input)?;
            let pdf = open(&a.input, &a.pw)?;
            let authority = a.tsa.as_ref().map(|url| HttpTimestampSource {
                url: url.clone(),
                username: a.tsa_user.clone(),
                password: secret(a.tsa_password.clone(), "CARBIDE_TSA_PASSWORD"),
            });
            let appearance = match &a.rect {
                Some(text) => Some(parse_rect(text)?),
                None => None,
            };
            let options = SignOptions {
                identity: Some(&identity),
                sub_filter: if a.cades { SubFilter::CadesDetached } else { SubFilter::Pkcs7Detached },
                reason: a.reason.clone(),
                location: a.location.clone(),
                contact: a.contact.clone(),
                appearance,
                certify: match a.certify {
                    None => None,
                    Some(1) => Some(CertifyLevel::NoChanges),
                    Some(2) => Some(CertifyLevel::FormFilling),
                    Some(3) => Some(CertifyLevel::Commenting),
                    Some(_) => return Err("--certify must be 1, 2 or 3".into()),
                },
                lock_document: a.lock,
                timestamp: authority.as_ref().map(|t| t as &dyn carbide_engine::signature::TimestampSource),
                signing_time: None,
            };
            carbide_engine::fs::write_replacing(&a.output, &pdf.sign(&base, &options)?)?;
            println!("signed {} as {}", a.output.display(), identity.common_name());
        }
        Command::Timestamp { input, output, tsa, tsa_user, tsa_password } => {
            let base = std::fs::read(&input)?;
            let pdf = Pdf::from_bytes(&base)?;
            let authority =
                HttpTimestampSource { url: tsa, username: tsa_user, password: secret(tsa_password, "CARBIDE_TSA_PASSWORD") };
            let options = SignOptions {
                sub_filter: SubFilter::DocumentTimeStamp,
                timestamp: Some(&authority),
                ..Default::default()
            };
            carbide_engine::fs::write_replacing(&output, &pdf.sign(&base, &options)?)?;
            println!("wrote {}", output.display());
        }
        Command::Verify { input, trust } => {
            let bytes = std::fs::read(&input)?;
            let mut store = TrustStore::default();
            for path in trust {
                let bytes = std::fs::read(&path)?;
                let certificates = carbide_engine::signature::read_certificates(&bytes)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                store.certificates.extend(certificates);
            }
            let reports = Pdf::verify_signatures(&bytes, &store, chrono::Utc::now())?;
            if reports.is_empty() {
                println!("No signatures found.");
            }
            for r in &reports {
                println!("{}: {}", r.field_name, r.status_line());
                if let Some(who) = &r.signer {
                    println!("  signer: {who}");
                }
                if let Some(claimed) = &r.claimed_name {
                    println!("  name in signature (unverified): {claimed}");
                }
                for line in r.description_lines() {
                    println!("  {line}");
                }
                if r.certifying {
                    println!("  certifying signature");
                }
                if r.validity == Validity::Invalid {
                    println!("  detail: {:?}", r.detail);
                }
            }
            if let Some(text) = notice_text(&reports) {
                println!("{text}");
            }
            if reports.iter().any(|r| r.validity == Validity::Invalid) {
                return Err("at least one signature is invalid".into());
            }
        }
        Command::Revision { action } => match action {
            RevisionAction::Save { file, label } => {
                let base = std::fs::read(&file)?;
                let pdf = Pdf::from_bytes(&base)?;
                let updated = pdf.save_revision(&base, &label, &pdf_date_now())?;
                carbide_engine::fs::write_replacing(&file, &updated)?;
                println!("saved revision {:?} in {}", label, file.display());
            }
            RevisionAction::List { file } => {
                let pdf = Pdf::open(&file)?;
                for (n, r) in pdf.revisions().iter().enumerate() {
                    println!("{}\t{}\t{}\t{} bytes", n + 1, r.display_time(), r.label, r.file_offset);
                }
                if !pdf.can_open_revision() {
                    println!("(Open Revision becomes available once two revisions exist.)");
                }
            }
            RevisionAction::Open { file, number, output } => {
                let bytes = std::fs::read(&file)?;
                let pdf = Pdf::from_bytes(&bytes)?;
                let revisions = pdf.revisions();
                let revision = revisions
                    .get(one_based(number, "revision")?)
                    .ok_or_else(|| format!("{} has {} revision(s)", file.display(), revisions.len()))?;
                carbide_engine::fs::write_replacing(&output, &open_revision_bytes(&bytes, revision)?)?;
                let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                println!("wrote {} ({})", output.display(), revision_display_name(&name, revision));
            }
        },
    }
    Ok(())
}

fn impose_options(a: &ImposeArgs) -> Result<ImpositionOptions, Failure> {
    let layout = match a.layout {
        LayoutArg::Booklet2 => LayoutKind::Booklet2Up,
        LayoutArg::Booklet4 => LayoutKind::Booklet4Up,
        LayoutArg::Booklet8 => LayoutKind::Booklet8Up,
        LayoutArg::Sequential => LayoutKind::Sequential,
        LayoutArg::StepAndRepeat => LayoutKind::StepAndRepeat,
        LayoutArg::CutStacks => LayoutKind::CutStacks,
    };
    let (columns, rows) = match layout {
        LayoutKind::Booklet2Up => (2, 1),
        LayoutKind::Booklet4Up => (2, 2),
        LayoutKind::Booklet8Up => (4, 2),
        _ => (a.columns, a.rows),
    };
    let media = if a.media.eq_ignore_ascii_case("auto") {
        Media::Autosize
    } else if let Some((w, h)) = a.media.split_once('x') {
        Media::Custom { width: w.trim().parse()?, height: h.trim().parse()? }
    } else {
        Media::Named(a.media.clone())
    };
    Ok(ImpositionOptions {
        plan: PlanOptions {
            layout,
            columns,
            rows,
            binding: if a.perfect_bound { Binding::Perfect } else { Binding::Saddle },
            sheets_per_fold: a.sheets_per_fold,
            style: match a.style {
                StyleArg::Sheetwise => Style::Sheetwise,
                StyleArg::WorkAndTurn => Style::WorkAndTurn,
                StyleArg::WorkAndTumble => Style::WorkAndTumble,
            },
            double_sided: !a.single_sided,
            printing_order: match a.printing_order {
                OrderArg::Interleaved => PrintingOrder::Interleaved,
                OrderArg::FrontsThenBacks => PrintingOrder::FrontsThenBacks,
                OrderArg::FrontsThenBacksReversed => PrintingOrder::FrontsThenBacksReversed,
            },
        },
        media,
        fit_to_media: !a.no_fit,
        scale_x_percent: a.scale_x,
        scale_y_percent: a.scale_y,
        margins: Margins { left: a.margin, right: a.margin, top: a.margin, bottom: a.margin },
        gutter_horizontal: a.gutter,
        gutter_vertical: a.gutter,
        creep: a.creep,
        crop_marks: a.crop_marks,
        registration_marks: a.registration_marks,
        registration_style: RegistrationStyle::Cross,
        flip_backs: a.flip_backs,
        ..Default::default()
    })
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
