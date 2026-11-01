# chrome-lens-rs

Google Lens OCR and translation from Rust, through the same `crupload` endpoint
Chromium's lens overlay uses. No Google account, no API key of your own, no
headless browser.

```rust
use chrome_lens_ocr::{LensClient, ScanOptions};

let client = LensClient::new(None);

// OCR.
let page = client.scan_path("screenshot.png", &ScanOptions::default()).await?;
println!("{}", page.full_text);

// OCR plus translation into Russian.
let page = client
    .scan_path("screenshot.png", &ScanOptions::default().translate_to("ru"))
    .await?;
for paragraph in &page.paragraphs {
    println!("{:?} -> {:?}", paragraph.text, paragraph.translation);
}
```

Each paragraph, line and word carries a `GeometryData` box so you can place
text back over the picture. The coordinates are always fractions of the image,
so multiply by whichever copy you are drawing on — a downscale preserves the
aspect ratio, and a fraction is invariant under that. (The server may send a
box in pixels instead; those are divided back down on the way out, so callers
have one rule rather than two.)

There is a CLI for looking at a response:

```
cargo run -- image.png --to ru
```

## Using your own HTTP client

`LensClient` is a convenience, not the interface. If you already have an HTTP
client with your own proxy settings, timeouts, cancellation and a test double,
use the four pieces directly and skip it:

```rust
use chrome_lens_ocr::{
    image_processor::{self, AlphaMode},
    parse_response_bytes, request_body, request_headers,
    DEFAULT_API_KEY, LENS_CRUPLOAD_ENDPOINT,
};

let image = image_processor::process_image_from_bytes(png, AlphaMode::Contrast)?;
let body = request_body(&image, &options)?;
let headers = request_headers(DEFAULT_API_KEY);   // (name, value) pairs

let reply = your_client.post(LENS_CRUPLOAD_ENDPOINT, headers, body)?;
let page = parse_response_bytes(&reply, image.width, image.height)?;
```

`objects_request` gives you the `LensOverlayServerRequest` itself if you want to
inspect or amend it before encoding.

Two things the crate deliberately leaves to you: `request_headers` returns no
transport headers, so `Accept-Encoding` and `Content-Length` are your client's
business — and `parse_response_bytes` wants the **decompressed** body, because
a client that advertised gzip and handed back the compressed bytes fails there
as a protobuf error, which is a confusing way to learn about a
`Content-Encoding`.

## Where this came from

A fork of [KolbyML/chrome-lens-ocr](https://github.com/KolbyML/chrome-lens-ocr),
which is itself a Rust reimplementation of a subset of
[chrome-lens-py](https://github.com/bropines/chrome-lens-py). This fork brings
it up to what chrome-lens-py 4.0.0 actually sends and reads.

### The protos are generated, not transcribed

The fork's `src/proto.rs` was about two hundred lines of prost structs with
field numbers typed in by hand. Those are now generated at build time from
Chromium's own `.proto` files, vendored under `third_party/lens_server_proto`
and pinned to one commit in `CHROMIUM_COMMIT` beside them.

Re-pin them with:

```
cargo run --bin update-protos -- --ref main
```

which resolves the ref to a commit, downloads the directory, checks that every
import of every file resolves, drops files upstream has removed, and exits
non-zero on any problem. `aim_communication.proto` is excluded, with the reason
printed on every run: it imports protos from `third_party/omnibox_proto` and
`components/optimization_guide` that do not live in this directory, and nothing
here uses it.

prost cannot read protobuf editions at all — it maps `syntax` onto a two-value
enum and panics on anything else ([tokio-rs/prost#1031], open) — and 28 of the
45 files are editions. So `build.rs` has protoc produce a descriptor set and
rewrites each editions file's `syntax` to the pre-editions one with the same
field-presence semantics: `proto3` for the six files that declare
`features.field_presence = IMPLICIT`, `proto2` for the rest. That distinction
matters on the wire — see `build.rs`, which spells out why, and the two
per-field overrides neither syntax can express.

protoc comes from `protoc-bin-vendored` rather than the host, so there is
nothing to install and the version is pinned: an older protoc *skips* an
editions file rather than failing, which is a silent way to ship stale code.

### What the request was missing

Measured against `create_ocr_translate_request` in chrome-lens-py. The fork did
not send:

- **`client_filters`.** Chromium always sends exactly one filter: `AUTO_FILTER`,
  or `TRANSLATE` which *replaces* it and carries the target language. The fork
  sent none — a state real Chrome never produces. It also meant translation was
  unreachable: the fork read `deep_gleams[].translation`, but Lens only fills
  that in when asked through the `TRANSLATE` filter, so `LensResult.translation`
  could never be anything but `None`.
- `request_id.time_usec`
- `rendering_context.rendering_environment = RENDERING_ENV_LENS_OVERLAY`, which
  is what Chromium pairs with `SURFACE_CHROMIUM`/`PLATFORM_WEB`.

An unset OCR language is now the empty string rather than `"en"`, because those
are different requests and the empty one is what Chromium sends by default.

### What the image handling was missing

| | before | now |
|---|---|---|
| when to downscale | either side over 1500 px | area over 1.5 MP **and** a side over 1600 px, which is `lens::ShouldDownscaleSize` |
| upload format | always re-encoded PNG | JPEG at quality 40, `kLensOverlayImageCompressionQuality` |
| transparency | flattened by the PNG encoder | kept, then flattened onto white or black by content luminance |

The resize rule matters in both directions: a 1600x900 screenshot is under the
area limit, so Chromium sends it untouched where the old rule shrank it and lost
OCR detail. And PNG of a screenshot is several times the size of the equivalent
JPEG, so the old encoding paid for every upload and got nothing back.

The transparency choice is measured, not guessed: the server composites a
transparent upload onto black, so black text on a transparent PNG reads back
empty if the alpha survives, and white text vanishes if it is flattened onto
white. Picking the background by luminance reads both. Pass
`AlphaMode::Flatten` for the plain white behaviour.

## Not implemented

- Region OCR — OCR of a selection rather than the whole image
  (`create_region_request` in chrome-lens-py).
- Session continuity. `sequence_id`/`image_sequence_id` are 1 with a fresh uuid
  per request; there is no follow-up interaction on the same image, and no
  cookies.
- Retries and rate limiting. chrome-lens-py holds a semaphore and retries on
  429/5xx; a tight capture loop here will not.

## Related

- [chrome-lens-py](https://github.com/bropines/chrome-lens-py) — the Python
  library and `lens_scan` CLI, with a warm daemon, ShareX integration and the
  text renderer. The reference for what goes on the wire.
- [chrome-lens-ocr](https://github.com/dimdenGD/chrome-lens-ocr) — dimdenGD's
  JavaScript original, where the endpoint was first worked out.

## Licence

MIT, as the upstream fork is. Upstream's copyright notice is kept in
[LICENSE](LICENSE) alongside this fork's.

## Disclaimer

This uses an undocumented Google endpoint and is not affiliated with or endorsed
by Google. It can break without notice, Google's terms of service apply to how
you use it, and images you scan are uploaded to Google's servers.

[tokio-rs/prost#1031]: https://github.com/tokio-rs/prost/issues/1031
