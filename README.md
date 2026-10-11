# Carbide PDF

A free, open-source PDF editor written in Rust, built for macOS. Its window layout, tab and command names, and feature set
follow Tungsten Power PDF for Mac so it feels familiar; the code, icons and colours are original.

| Path | What it is |
|---|---|
| `carbide-pdf/` | The Rust workspace: engine library, `carbide` CLI and the `carbide-gui` desktop app. Start with its `README.md`. |

`carbide-pdf/README.md` lists what to download to build and run the app (PDFium and the OCR models).

The icons are vector drawings in `carbide-pdf/crates/app/src/icons.rs`.

## Building

```
cd carbide-pdf
cargo build --release          # writes target/release/carbide and target/release/carbide-gui
CARBIDE_PDFIUM_LIB=/path/to/libpdfium.dylib target/release/carbide-gui [file.pdf]
```

All of the application's own code is Rust. Two things are not: PDFium (Google's C++ renderer), which is loaded at run
time for page rendering and text extraction and is not bundled, and the `ring` cryptography crate, which carries a small
amount of C and assembly. See `carbide-pdf/README.md` for the PDFium and OCR model downloads.

## Legal

Carbide PDF is an independent project. It is not affiliated with, endorsed by, or sponsored by Tungsten Automation
(formerly Kofax) or any of its products. "Tungsten", "Power PDF" and "Kofax" belong to their owners and appear here only
to say what the project's behaviour was modelled on. No code, artwork or binaries from the original product are included.

Released under the MIT License (see `LICENSE`).

