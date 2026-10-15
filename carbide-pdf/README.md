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

**PDFium (runtime, for rendering and text).** Needed by `images`, `text`, `compare`, `redact`, `ocr`, and the GUI page view. Other commands do not need it. Download a prebuilt library from https://github.com/bblanchon/pdfium-binaries and set `CARBIDE_PDFIUM_LIB` to the path of the library file (or to the folder containing it). The engine looks for `libpdfium.dylib` (`libpdfium.so`, `pdfium.dll`) in this order and loads the first it finds:

1. `CARBIDE_PDFIUM_LIB`, the file or the folder it names;
2. the folder holding the executable file (with symbolic links followed, then the folder of the link it was started through, if any);
3. inside an app bundle, its `Contents/Frameworks` and then `Contents/Resources` folders;
4. the system library folders, by absolute path (`/opt/homebrew/lib`, `/usr/local/lib`, `/usr/lib` on macOS).

The current directory is never searched, not even through the system loader's own search: loading a library runs its code, and a file with that name in the folder a command is run from (an unpacked download, say) could be anyone's.

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
| Remove hidden information | `sanitize.rs` (`remove_hidden_information`) | `sanitize` | Yes: Security > Redaction > Remove Hidden Info (also offered after Apply) |
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
Cmd+Shift+Z redo, Cmd+= and Cmd+- zoom. Closing the window, choosing Quit (or Cmd+Q), quitting from the Dock or with
AppleScript, and logging out, restarting or shutting down all ask about every document with unsaved changes first. A
quit that macOS asks for while changes are unsaved is cancelled and the question is shown instead (the app adds an
`applicationShouldTerminate:` handler to the app delegate winit installs); a logout, restart or shutdown then stops,
macOS names Carbide PDF as the app that stopped it, and it has to be started again once the question is answered. If
the app ends without asking (it is killed, say), documents with unsaved changes keep their Auto Recovery copies.
Forms is empty: form fields are not implemented.

A signed document shows a notice bar that sums up its signatures. Signatures are checked in the file on disk, so while
the document has unsaved changes the notice bar, the signatures panel and Validate say that what they report is about
the document as last saved or opened, and that the unsaved changes are not covered by any signature. Validate's
results say so as soon as the document is edited while they are shown, and close when its file is written.

Extract Page, Imposition, Split and Export Text do not write over a file that is open in a tab, the document's own
included: that tab would go on showing, and could save back, what is no longer on disk.

Add Location on the Start screen lists the cloud folders it finds on the computer (iCloud Drive, and the folders that the
Dropbox, Google Drive, OneDrive and similar desktop apps keep in `~/Library/CloudStorage` or the home folder) and lets
you pin any folder. These are ordinary folders to Carbide PDF: it does not sign in to a cloud service or use its API.

## CLI examples

Page numbers are 1-based. Rectangles are `PAGE:X,Y,W,H` in points, with the origin at the bottom left of the displayed page. The `note --at` anchor is `PAGE:X,Y`, measured from the top left. Most commands accept `--password` for a protected input.

New files made from a protected input (`extract`, `split`, `impose`, `merge`) keep its passwords and permissions; `--unprotected` writes them without. `images`, `text` and `compare` refuse a protected input unless `--unprotected` is given. `split` and `images` do not replace files already in the folder unless `--force` is given, and `split` refuses ranges that would write the same part twice. `reduce` writes `<name>-reduced.pdf` next to each original and replaces nothing; `--overwrite` replaces other existing files and `--in-place` replaces the originals.

```
carbide merge combined.pdf part1.pdf part2.pdf
carbide extract report.pdf "1-3,5" excerpt.pdf
carbide watermark draft.pdf stamped.pdf --text DRAFT --opacity 0.2 --angle 30 --color CC0000 --pages 1-5
carbide bates brief.pdf numbered.pdf --prefix ACME- --start 100 --digits 6 --position bottom-right
carbide redact filing.pdf filing-redacted.pdf --rect 2:72,500,300,24 --rect 4:72,180,200,16
carbide sanitize filing-redacted.pdf filing-clean.pdf --bookmarks
carbide reduce *.pdf --out-dir small
carbide encrypt contract.pdf contract-locked.pdf --owner "owner-secret" --user "open-secret" --no-print
carbide impose signature.pdf booklet.pdf --layout booklet4 --media A4 --perfect-bound --sheets-per-fold 2 --crop-marks
carbide sign contract.pdf contract-signed.pdf --p12 signer.p12 --p12-password "$P12_PASS" --certify 2 --reason "Approved" --rect 1:72,72,180,50
carbide verify contract-signed.pdf --trust ca.pem
carbide ocr scan.pdf scan-searchable.pdf --dpi 300 --pages 1-4
```

`verify` prints a report for each signature, and its exit status says what it found, so a script can rely on it:

| Status | Meaning |
|---|---|
| 0 | The file is signed, and every signature and document time stamp in it is valid: intact, from a certificate that is trusted (given with `--trust`, or issued through a chain that leads to one), valid and allowed to sign. Nothing was added after the last signature. |
| 1 | The file could not be checked: it cannot be read, or it needs `--password`. |
| 2 | The command line is wrong. |
| 3 | At least one signature is invalid: it is damaged, or what it signed was changed. |
| 4 | No signature is invalid, but not all are proven valid: a signer or the authority of a document time stamp is not trusted, a certificate has expired or may not sign, an algorithm cannot be checked, or the file was changed after the last signature. |
| 5 | The file has no signatures. |

A time stamp carried inside a signature only decides when that signature's certificates are checked: one that does not verify, or whose authority is not trusted, is reported and not used, and the certificates are then checked as of now.

## Known limits and deviations

- Redaction rasterizes the affected pages (those pages lose selectable text).
- Watermark and Bates text uses standard Latin fonts (WinAnsi); other scripts become `?`.
- Reduce file size: photos become JPEG; 1-bit and JPEG 2000 sources are left unchanged, CMYK is left unchanged, an image is only replaced when the new data is smaller, and the output is not linearized.
- Imposition: page-to-cell assignment is covered by tests. Cell origin, fit and creep application are this project's own design.
- Revisions are stored in the file as a catalog entry plus incremental updates.
- Signatures: signing uses RSA identities (PKCS#12 or PEM) with SHA-256. Verification also accepts ECDSA (P-256 and P-384), RSA-PSS and SHA-1/384/512 digests (SHA-1 is flagged as weak), subjectKeyIdentifier signer references, and RFC 3161 document and signature time stamps; an algorithm it cannot check is reported as validity unknown rather than invalid. Certificates are checked at the time a time stamp from a trusted authority gives, or else now: the signing time a signature claims for itself is shown as the signer's unverified claim and never used for that, and a time stamp's time is shown only from a token that verifies. As there is no long-term validation, a time stamp's authority must be valid and trusted now as well as at the time it gives: once its certificate has expired, nothing shows its key was not used to give an earlier time. A signer's certificate that names purposes (extended key usage) must name one for signing documents (any purpose, e-mail protection or document signing). Credentials for a time stamp authority, given as options or written into its URL (`https://user:password@host/`), are only sent over https. Revocation checking and LTV are not implemented. DocMDP change-detection is not implemented: the certification is recorded, but later changes are not classified.
- OCR writes Latin text only and does not auto-detect rotated scans.
- Files are replaced in one step: the new file is written under a random name next to the old one and renamed over it, so it is never left half written, and a link planted at a name is never written through. It keeps the old file's group, permission bits and exactly its access control list (without entries the folder gives new files), and on macOS also its extended attributes and its hidden and no-backup flags. When the access control list cannot be given to the new file, or the group cannot and it has other access than everyone else, the old file is left as it is and saving fails with a message. A file of another user is not replaced, as the new file would belong to whoever saves it (root gives it back to its owner). Limits: on Linux the access control list comes along but other extended attributes do not; on Windows only the read-only attribute does, not the access control list; other hard links to a replaced file keep the old contents. Auto Recovery keeps its copies in a folder only the user can open, each readable by the user alone.
- Desktop Save writes the whole file again, so removed or replaced content does not stay in it. Only a signed file or one with saved revisions gets an appended update, to keep those valid; after redacting, deleting pages or other removals Save asks before rewriting it, which invalidates its signatures and drops its revisions. An encrypted file keeps its passwords and permissions. The CLI's edit commands save the same way; where a rewrite costs signatures or revisions they say so instead of asking.
- Redacting or deleting a page also removes its form fields (a field that still has a widget on another page keeps its value), its tags, and resources only it used, and replies and pop-ups on other pages stop pointing at its annotations, which would otherwise stay in the file. Deleting a page also removes the bookmarks, named destinations, open action, link destinations and article beads that lead only to it, and the pages that stay keep their page labels. Extracted and split files carry nothing of the other pages (no article thread that runs only through them, and none of their annotations through a reply or pop-up on a page that is kept) and have no tags.
- Redaction does not touch document-level text such as the document information, bookmarks or attachments; Remove Hidden Information does, item by item. The file is then written in full, which also leaves out the earlier versions appended updates kept in it and objects nothing refers to; these are reported too.
- Extracted pages, split parts, imposed sheets and merged documents made from a protected document keep its passwords and permissions. Documents protected in different ways are only merged without protection, which the GUI asks about and the CLI needs `--unprotected` for. Exporting images or text of a protected document asks first in the GUI.
- Signing and saving revisions of encrypted files are not supported. Reduce File Size refuses encrypted files, and Auto Recovery keeps no copy of them.
- Reduce File Size writes its results under new names next to the originals by default; replacing the originals is a separate choice. A batch in which two files would be written to the same file is refused before anything is reduced.
- Auto Recovery: each window keeps its own recovery records, and only those of windows that are no longer running are offered for recovery. Records an earlier version of the app kept (it had no per-window lock) are offered once they have not changed for longer than the longest auto-save interval. A recovered document opens as an unsaved document; Save asks before replacing the file it was recovered for, and refuses while that file is open in another tab with unsaved changes. Deleting recovered copies asks first. Each document counts its own auto-save interval from when it was opened.
- Damaged page trees (pages or nodes without a type, loops through `/Kids` or `/Parent`, wrong page counts, a page listed twice, kids that name their page through another reference or with another generation number) are repaired when a file is opened, so that editing and rendering agree on every page number; the repairs are written with the next save. Copies made by a repair share large values instead of duplicating them. A page tree nested more than 128 levels deep, one that lists the same group of pages twice, one that lists pages so often that the copies would hold more than a million values, and one with a page or node that the file holds but that cannot be read (left out, it would be missing from the file after the next save) are refused. A `/Rotate` that is not a plain quarter turn is read the way PDFium reads it; page boxes with coordinates past 1,000,000 units are brought within that, a media box without an area is a US Letter page (as PDFium shows it), and numbers too large to be written out and read back are brought within range when a file is read and again before it is written.
- Page-label trees that list a node more than once or are nested more than 32 levels deep are rebuilt as one list of labels, and a form field listed more than once in the field tree is kept only where it is first listed, when a file is opened. PDFium reads both whenever it opens a page and follows them without limit, so such a file would otherwise crash or freeze the app.
- Limits against hostile files: content the engine decompresses itself may come to 256 MB for one page or object, and to 1 GB in all for imposition (pages drawn by several content streams) and Reduce; past that the command fails with a message. The clean-up after deleting, redacting or extracting pages reads each page's content within the 256 MB limit, and fails with a message rather than keep resources that removed content alone may have used. Content is read token by token, never parsed into a list of operations. Object streams are unpacked by the engine, each within 16 MB and all of them within 256 times the size of the file (at least 256 MB, at most 2 GB, counting the memory of the objects parsed from them); a file past that is refused. Reduce decodes images within 1 GB plus 32 times what they take in the file, and leaves the rest, and any image of more than 128 MB of samples, as they are. To find the size images are drawn at, it reads each form once for each set of resources it is drawn with, however often it is drawn, within 10 million steps, 64 MB of form content in all, 100,000 different image draws for one form or page, and forms nested 8 deep. In a file past that, or with a form that draws itself, the sizes some images are drawn at are not known, so its images are recompressed but none is down-sampled.
- PDFium has no such limits of its own, so before a page is rendered or its text read the engine checks it: its content, with the forms it and its annotations draw counted each time they are drawn, may decode to 256 MB and hold 4 million operators, and no form may draw itself. Redaction, OCR, Compare and the image and text exports refuse a page past that with a message; the GUI shows a notice in its place. Pages are rendered at a lower resolution when the image would exceed 50 million pixels or 20000 pixels on a side; the GUI's page images stay within what the GPU takes (8192 pixels on a side unless egui reports otherwise) and 16 million pixels, thumbnails kept for redrawing within 16 million pixels in all, and page sizes no file could mean are laid out as the largest a page box can have. Imposition takes at most 32 columns and rows, 64 sheets per fold, and sheets, margins and gaps up to 14400 points; Bates numbers have 1 to 20 digits.
- Page lists that name a page more than once (`1-3,2`) act on it once. `rotate` takes multiples of 90 degrees, negative ones included. Rectangles given with a negative width or height count from the other corner. A watermark needs a size larger than 0 (and no larger than a page can be), an angle, and an opacity and colour between 0 and 1. Redacted pages are rasterized at 72 dpi or more. Merging nothing is refused.

Interface differences, as found in the code:

- Auto Save and Auto Recovery are available in the GUI only. There is no CLI command for them.
- Removing protection (`decrypt`) is CLI only. The GUI can only write a protected copy. Written over the open document's own file, the protected copy (or a reduced one) becomes the document: it is read back with the new password, so later saves, Save As and Auto Recovery keep the protection. Like Save, these copies are not written over a file open in another tab with unsaved changes, and close other tabs showing the file they replace.
- `verify` takes a file on the command line. The GUI checks the open document.
- Most GUI page actions act on the current page. The CLI takes page lists.
- `blank` puts the new page at position AT. The GUI inserts after the current page.
- `move` takes FROM and TO positions. The GUI moves one page per action.
