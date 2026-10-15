# Contributing to Carbide PDF

Bug reports, fixes and new features are welcome. The Rust workspace is in `carbide-pdf/`.

## Build and test

```
cd carbide-pdf
cargo build --release
cargo test
```

Most tests need PDFium, and the OCR tests need the OCR models. Set `CARBIDE_PDFIUM_LIB` and `CARBIDE_OCR_MODELS` as described in `carbide-pdf/README.md`.

## Before you open a pull request

Run these from `carbide-pdf/`:

```
cargo fmt
cargo clippy --workspace --all-targets
```

Fix anything clippy reports, and make sure `cargo test` still passes.

## Guidelines

- Add tests for behaviour changes.
- Keep pull requests focused. One change per pull request is easier to review. Put refactors in a separate pull request from behaviour changes.
- Say in the pull request what changed, why, and how you tested it.
- Report security problems privately. See `SECURITY.md`.

## License

Contributions are released under the MIT License, the same as the rest of the project. See `LICENSE`.
