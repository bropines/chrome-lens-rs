# Changelog

## [0.4.0] - 2026-11-01

First release of the fork, bringing the crate up to what chrome-lens-py 4.0.0
sends and reads. Everything below is measured against
`create_ocr_translate_request` and `image_processor.py` in that release.

### Breaking

- `LensResult` is now called `Page` and has gained `language`, `image_width`
  and `image_height`. The old name stays as a type alias, so existing code
  compiles.
- `Paragraph` has gained `language` and `translation`.
- `Word::separator` is a `String` rather than an `Option<String>`; an absent
  separator and an empty one were already treated the same everywhere.
- `process_image_path`/`process_image_bytes` still take an OCR language and do
  OCR only. Translation needs the new `scan_path`/`scan_bytes` with
  `ScanOptions`.
- The binaries are behind a default-on `bin` feature, so a library consumer can
  turn off `env_logger` and `serde_json` with `default-features = false`.
- `bytes`, `url` and `thiserror` are no longer dependencies; none of them was
  used.

### Added

- **Translation actually works.** The crate read `deep_gleams[].translation`
  but never asked for a translation, so that field could only ever be `None`.
  Lens fills it in only when the request carries a `TRANSLATE` filter, which is
  now sent when `ScanOptions::target_language` is set.
- Per-paragraph translations on `Paragraph::translation`, paired with the
  paragraphs by index. A translation whose status is anything but `SUCCESS` is
  dropped rather than shown: an unsupported language pair or a same-language
  no-op carries an empty or echoed string that is worse than nothing.
- `ScanOptions` for the OCR language, translation target and source, client
  region and time zone, and the alpha mode.
- `objects_request` is public, so the request can be inspected or diffed
  against another implementation's.
- `cargo run --bin update-protos` re-pins the vendored protos to a Chromium ref.
- 25 tests, over request construction, the response parse and the image rules.

### Changed

- **The protos are generated from Chromium's `.proto` files** rather than
  hand-transcribed prost structs. `build.rs` compiles the vendored tree with a
  pinned protoc, working around prost's lack of editions support by rewriting
  each editions file's `syntax` in the descriptor set to the pre-editions one
  with matching field presence (tokio-rs/prost#1031). It also warns about the
  per-field presence overrides that no syntax can express.
- **The request now carries what Chromium's does.** It was missing
  `client_filters` entirely — Chromium always sends exactly one filter, and
  sending none is a state real Chrome never produces — along with
  `request_id.time_usec` and
  `rendering_context.rendering_environment = RENDERING_ENV_LENS_OVERLAY`.
- **Downscaling follows `lens::ShouldDownscaleSize`**: only when the area is
  over 1.5 MP *and* a side is over 1600 px. The old rule shrank anything over
  1500 px on a side, which cost OCR detail on images Chromium keeps whole — a
  1600x900 screenshot among them.
- **Uploads are JPEG at quality 40**, not PNG. PNG of a screenshot is several
  times the size for no benefit, and every request paid that on the upload.
- Transparency is flattened onto white or black by the content's luminance
  rather than left to the PNG encoder. The server composites a transparent
  upload onto black, so preserving alpha loses black text and flattening onto
  white loses white text. `AlphaMode::Flatten` keeps the plain white behaviour.
- An unset OCR language is the empty string, not `"en"`. Those are different
  requests, and the empty one is what Chromium sends when nothing was chosen.
- A `LensOverlayServerError` in the response is reported with its error type
  rather than parsed as an empty page.

## [0.3.0] and earlier

Upstream releases of [KolbyML/chrome-lens-ocr](https://github.com/KolbyML/chrome-lens-ocr).
