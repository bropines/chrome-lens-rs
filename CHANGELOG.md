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
- `Paragraph::translation` is a `Translation` rather than a `String`; the
  string is on its `text`.
- **`LensClient` is behind a default-on `client` feature**, along with
  `reqwest` and `tokio`. A consumer that sends through its own HTTP stack no
  longer links a second one: `default-features = false` takes the dependency
  tree from **358 crates to 50**, with no reqwest, hyper or tokio in it. The
  binaries are behind `bin`, which implies `client`.
- `image` is taken with `default-features = false` and only `jpeg` (every
  upload is encoded as one) and `png`. The default-on `extra-formats` adds
  `webp`, `gif` and `bmp`; the rest of image's default set (tiff, exr, avif,
  ...) no longer unifies into a consumer's build. `rand` stays mandatory -
  it is used for the request uuid, not only by the client.
- `bytes`, `url` and `thiserror` are no longer dependencies; none of them was
  used.

### Added

- **Translation actually works.** The crate read `deep_gleams[].translation`
  but never asked for a translation, so that field could only ever be `None`.
  Lens fills it in only when the request carries a `TRANSLATE` filter, which is
  now sent when `ScanOptions::target_language` is set.
- Per-paragraph translations on `Paragraph::translation`, as a `Translation`
  with the text and the languages, paired with the paragraphs by index. A
  translation whose status is anything but `SUCCESS` is dropped rather than
  shown: an unsupported language pair or a same-language no-op carries an empty
  or echoed string that is worse than nothing.
- **Per-line translations on `Line::translation`.** Lens does not translate
  line by line - it returns one string per paragraph plus ranges saying which
  part belongs where - so this is that text assembled, ready to draw at
  `Line::geometry`, which is where the source line was and where Chromium puts
  the translation too.

  It is built from the *word* ranges, the way Chromium's
  `CreateTranslatedLineMojomFromProto` and chrome-lens-py's `build_line_text`
  do, rather than by slicing the line's own `start..end`: each word plus the
  gap up to the next, and for the last word the gap up to the first word of
  the following line. That keeps the lines concatenating back into
  `Translation::text`, and keeps any padding in the line range out of the
  drawn text. A line can therefore end in a space - deliberate, and what makes
  the concatenation work; trim before measuring or centring. A line carrying
  no word ranges falls back to its own range.

  The ranges are UTF-16 code units, matching `icu::UnicodeString` and *not*
  the "Unicode characters" the proto comment claims, so they are sliced as
  such: an emoji is one character, two UTF-16 code units and four UTF-8 bytes,
  and guessing wrong splits text mid-glyph.
- `ScanOptions` for the OCR language, translation target and source, client
  region and time zone, and the alpha mode.
- **The request can be built without being sent**, for callers that already
  have an HTTP client they want to keep using - their own proxy settings,
  timeouts, cancellation and test doubles. `objects_request`, `request_body`,
  `request_headers` and `parse_response_bytes` are free functions; `LensClient`
  is a convenience built on them rather than the way in.
  `request_headers` returns name/value pairs and deliberately no transport
  headers, and `parse_response_bytes` wants a decompressed body.
- `cargo run --bin update-protos` re-pins the vendored protos to a Chromium ref.
- 25 tests, over request construction, the response parse and the image rules.

### Fixed

- **A failed translation no longer shifts every later one onto the wrong
  paragraph.** A gleam's position in `deep_gleams` *is* the paragraph index, so
  a gleam that did not succeed has to be skipped in place. This crate filtered
  the failures out and then zipped the survivors against the paragraphs, so one
  failure in the middle moved every translation after it up by one — silently,
  and only for images where something failed. Found by the Glossa team.
- A response carrying an `error` submessage of type `UNKNOWN_TYPE` (0) is no
  longer treated as a failure. The field has implicit presence, so a perfectly
  good response can have one; chrome-lens-py checks `error_type != 0` for the
  same reason.

- **A `coordinate_type: IMAGE` box is no longer read as a fraction.** The
  server may send a box either normalised or in pixels of the uploaded image,
  and the crate ignored the field and treated every box as normalised. A pixel
  box then came out three orders of magnitude too large. They are divided back
  down now, so every `GeometryData` is normalised and the one scaling rule in
  its docs holds for all of them; `GeometryData::coordinate_type` still reports
  what the server sent. Found by the Glossa team hitting the same hole.

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
