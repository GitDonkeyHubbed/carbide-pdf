# Carbide PDF

Carbide PDF is a free, open-source PDF editor written in Rust. Its window layout and feature set follow Tungsten Power PDF for Mac so it feels familiar; the code and icons are its own, and no code, artwork or binaries from that product are included. It edits page structure, adds annotations and watermarks, redacts content, imposes pages for print, reduces file size, protects files with AES-256, keeps named revisions, auto-saves, signs and verifies digital signatures, and adds a searchable text layer to scanned pages. The engine is a library with no UI dependencies. A command-line tool and a desktop app are built on it.

## Layout

| Crate | Package | Output | What it is |
|---|---|---|---|
| `crates/engine` | `carbide-engine` | library | PDF operations with no UI dependencies. Structure edits go through `lopdf`; PDFium is used only to rasterize pages and read text. |
| `crates/cli` | `carbide-cli` | `carbide` | Command-line front end for the engine. |
| `crates/app` | `carbide-app` | `carbide-gui` | Desktop app (eframe) with a ribbon, document tabs, a Start screen and dialogs over the engine. |

## Requirements and setup

**Rust toolchain.** Cargo workspace, edition 2021. The repository does not pin a minimum Rust version.

**PDFium (runtime, for rendering and text).** Needed by `images`, `text`, `compare`, `redact`, `ocr`, and the GUI page view. Other commands do not need it. Download a prebuilt library from https://github.com/bblanchon/pdfium-binaries and set `CARBIDE_PDFIUM_LIB` to the path of the library file (or to the folder containing it). If the variable is not set, the engine also looks in the current directory and then in the executable's directory.

```
export CARBIDE_PDFIUM_LIB=/path/to/lib/libpdfium.dylib
```

**OCR models (runtime, for OCR only).** Two files, `text-detection.rten` and `text-recognition.rten`, go in one folder. Download them from https://ocrs-models.s3-accelerate.amazonaws.com/ and set `CARBIDE_OCR_MODELS` to that folder. The `ocr` command also accepts `--models DIR`. If the models are missing, OCR fails with a message that names both files.

## Build, test, run

```
cargo build --release
cargo test
target/release/carbide --help
target/release/carbide-gui [file.pdf]
```

`cargo build --release` writes `target/release/carbide` and `target/release/carbide-gui`. `carbide-gui` opens the PDF named on the command line, if one is given.

## Features

| Feature | Engine module | CLI command | In GUI? |
|---|---|---|---|
| Merge | `pdf.rs` (`Pdf::merge`) | `merge` | Yes: Home > Pages > Merge PDFs |
| Split | `pdf.rs` (`split`, `split_every_page`) | `split` | Yes: Home > Tools > Split File |
| Extract | `pdf.rs` (`extract_pages`) | `extract` | Yes: Home > Pages > Extract (current page only) |
| Delete | `pdf.rs` (`delete_pages`) | `delete` | Yes: Home > Pages > Delete (current page only) |
| Rotate | `pdf.rs` (`rotate_pages`) | `rotate` | Yes: Home > Rotate > Rotate Left / Rotate Right (current page only) |
| Insert blank page | `pdf.rs` (`insert_blank_page`) | `blank` | Yes: Home > Pages > Insert Page (after the current page) |
| Move page | `pdf.rs` (`move_page`) | `move` | Yes: Home > Rotate > Move Up / Move Down (one step per action) |
| Watermark | `stamp.rs` (`apply_watermark`) | `watermark` | Yes: Edit > Content Editing > Watermark |
| Bates numbering | `stamp.rs` (`apply_bates`) | `bates` | Yes: Edit > Content Editing > Bates Number |
| Redaction | `redact.rs` (`apply_redactions`) | `redact` | Yes: Security > Redaction > Mark Redaction, then Apply |
| Markup (highlight, underline, strikeout) | `annotate.rs` (`add_markup`) | `markup` | Yes: Comment > Text Markup > Highlight, Highlight Area, Crossout, Underline |
| Sticky note | `annotate.rs` (`add_note`) | `note` | Yes: Comment > Comment > Note |
| Text box | `annotate.rs` (`add_text_box`) | `note --on-page` | Yes: Comment > Comment > Typewriter |
| Compare | `compare.rs` (`compare_documents`) | `compare` | Yes: Advanced Processing > Document Processing > Compare |
| Export images | `export.rs` (`export_images`) | `images` | Yes: Advanced Processing > Convert > To Image |
| Export text | `export.rs` (`export_text`) | `text` (prints to stdout) | Yes: Advanced Processing > Convert > To Text |
| Password protection (AES-256) | `security.rs` (`ProtectionOptions`) | `encrypt`, `decrypt` | Partly: Security > Secure > Protect PDF writes a protected copy. Removing protection is not in the GUI. |
| Revisions | `revisions.rs`, `incremental.rs` | `revision save`, `revision list`, `revision open` | Yes: Advanced Processing > Revisions > Save Revision, Open Revision |
| Auto Save / Auto Recovery | `autosave.rs` | none | Yes: Advanced Processing > Preferences > Auto Save..., Auto Recovery |
| Imposition (six layouts) | `imposition.rs`, `imposition_plan.rs` | `impose --layout` (booklet2, booklet4, booklet8, sequential, step-and-repeat, cut-stacks) | Yes: Home > Pages > Imposition |
| Reduce file size | `reduce.rs` | `reduce` | Yes: Home > Tools > Reduce File, or Reduce File Size on the Start screen (current file or a list of files) |
| Digital signature (sign) | `signature.rs` | `sign` | Yes: Security > Signature > Sign (drag a rectangle for a visible signature) or Sign Invisibly |
| Certification | `signature.rs` | `sign --certify N` (N = 1, 2 or 3) | Yes: Security > Signature > Certify |
| Time stamp | `signature.rs` | `timestamp` (or `sign --tsa`) | Yes: Security > Signature > Time Stamp |
| Verify signatures | `signature.rs` | `verify` | Yes: Security > Signature > Validate (open document) |
| OCR | `ocr.rs` | `ocr` | Yes: Advanced Processing > Convert > OCR |

## The window

The layout follows the original product: a quick-access strip (panels, open, save, select, undo, redo), a centred
tab menu (Home, Edit, View, Comment, Advanced Processing, Security, Forms) and a Find a Tool box on top, then a strip
of document tabs (a permanent Start tab, one tab per open file, and a + button), and below it the ribbon of command
groups for the chosen tab. The Start screen has Quick Launch (Open, Create, Reduce File Size, Combine All, Protect
PDF), a Recent File / Recent Folder list that is remembered between runs, and pinned locations. In a document, the
left panel switches between page thumbnails, comments and signatures, and the bottom bar holds page navigation and
zoom (fit width, fit page, actual size; pinch or Ctrl-scroll also zooms). All icons are drawn with vector paths in
`crates/app/src/icons.rs`; no artwork from the original product is used.

Shortcuts: Cmd+N new document, Cmd+O open, Cmd+S save, Cmd+W close tab (asks about unsaved changes), Cmd+Z undo,
Cmd+Shift+Z redo, Cmd+= and Cmd+- zoom. Closing the window, or choosing Quit, asks about every document with unsaved
changes first (on macOS the application menu's Quit is rewired to close the window so it cannot skip that question).
Forms is empty: form fields are not implemented.

Add Location on the Start screen lists the cloud folders it finds on the computer (iCloud Drive, and the folders that the
Dropbox, Google Drive, OneDrive and similar desktop apps keep in `~/Library/CloudStorage` or the home folder) and lets
you pin any folder. These are ordinary folders to Carbide PDF: it does not sign in to a cloud service or use its API.

## CLI examples

Page numbers are 1-based. Rectangles are `PAGE:X,Y,W,H` in points, with the origin at the bottom left of the displayed page. The `note --at` anchor is `PAGE:X,Y`, measured from the top left. Most commands accept `--password` for a protected input.

```
carbide merge combined.pdf part1.pdf part2.pdf
carbide extract report.pdf "1-3,5" excerpt.pdf
carbide watermark draft.pdf stamped.pdf --text DRAFT --opacity 0.2 --angle 30 --color CC0000 --pages 1-5
carbide bates brief.pdf numbered.pdf --prefix ACME- --start 100 --digits 6 --position bottom-right
carbide redact filing.pdf filing-redacted.pdf --rect 2:72,500,300,24 --rect 4:72,180,200,16
carbide encrypt contract.pdf contract-locked.pdf --owner "owner-secret" --user "open-secret" --no-print
carbide impose signature.pdf booklet.pdf --layout booklet4 --media A4 --perfect-bound --sheets-per-fold 2 --crop-marks
carbide sign contract.pdf contract-signed.pdf --p12 signer.p12 --p12-password "$P12_PASS" --certify 2 --reason "Approved" --rect 1:72,72,180,50
carbide verify contract-signed.pdf --trust ca.pem
carbide ocr scan.pdf scan-searchable.pdf --dpi 300 --pages 1-4
```

## Known limits and deviations

- Redaction rasterizes the affected pages (those pages lose selectable text).
- Watermark and Bates text uses standard Latin fonts (WinAnsi); other scripts become `?`.
- Reduce file size: photos become JPEG; 1-bit and JPEG 2000 sources are left unchanged, CMYK is left unchanged, an image is only replaced when the new data is smaller, and the output is not linearized.
- Imposition: page-to-cell assignment is covered by tests. Cell origin, fit and creep application are this project's own design.
- Revisions are stored in the file as a catalog entry plus incremental updates.
- Signatures: signing uses RSA identities (PKCS#12 or PEM) with SHA-256. Verification also accepts ECDSA (P-256 and P-384), RSA-PSS and SHA-1/384/512 digests (SHA-1 is flagged as weak), subjectKeyIdentifier signer references, and RFC 3161 document and signature time stamps; an algorithm it cannot check is reported as validity unknown rather than invalid. Revocation checking and LTV are not implemented. DocMDP change-detection is not implemented: the certification is recorded, but later changes are not classified.
- OCR writes Latin text only and does not auto-detect rotated scans.
- Desktop Save writes the whole file again, so removed or replaced content does not stay in it. Only a signed file or one with saved revisions gets an appended update, to keep those valid; after redacting, deleting pages or other removals Save asks before rewriting it, which invalidates its signatures and drops its revisions. An encrypted file keeps its passwords and permissions. The CLI's edit commands save the same way; where a rewrite costs signatures or revisions they say so instead of asking.
- Redacting or deleting a page also removes its form fields (a field that still has a widget on another page keeps its value), its tags, and resources only it used. Extracted and split files carry nothing of the other pages and have no tags.
- Signing and saving revisions of encrypted files are not supported. Reduce File Size refuses encrypted files, and Auto Recovery keeps no copy of them.

Interface differences, as found in the code:

- Auto Save and Auto Recovery are available in the GUI only. There is no CLI command for them.
- Removing protection (`decrypt`) is CLI only. The GUI can only write a protected copy.
- `verify` takes a file on the command line. The GUI checks the open document.
- Most GUI page actions act on the current page. The CLI takes page lists.
- `blank` puts the new page at position AT. The GUI inserts after the current page.
- `move` takes FROM and TO positions. The GUI moves one page per action.
