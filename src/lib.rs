//! Google Lens OCR and translation through the Chromium `crupload` endpoint.
//!
//! The protos are generated from Chromium's own `.proto` files (see
//! [`proto`]), and the request this builds mirrors what Chromium's lens overlay
//! actually sends - by way of chrome-lens-py's `create_ocr_translate_request`,
//! which is kept honest against the live endpoint.
//!
//! ```no_run
//! # async fn demo() -> anyhow::Result<()> {
//! use chrome_lens_ocr::{LensClient, ScanOptions};
//!
//! let client = LensClient::new(None);
//!
//! // OCR only.
//! let page = client.scan_path("screenshot.png", &ScanOptions::default()).await?;
//! println!("{}", page.full_text);
//!
//! // OCR plus translation into Russian.
//! let page = client
//!     .scan_path("screenshot.png", &ScanOptions::default().translate_to("ru"))
//!     .await?;
//! println!("{:?}", page.translation);
//! # Ok(())
//! # }
//! ```

pub mod constants;
pub mod image_processor;
pub mod proto;

pub use crate::constants::{DEFAULT_API_KEY, LENS_CRUPLOAD_ENDPOINT};
// Exposed on GeometryData, so it should not need a `proto::` path.
pub use crate::proto::CoordinateType;

use std::f32::consts::PI;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use prost::Message;

#[cfg(feature = "client")]
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
#[cfg(feature = "client")]
use std::{path::Path, time::Duration};

use crate::constants::*;
use crate::image_processor::{AlphaMode, ProcessedImage};
use crate::proto::*;

/// What to ask for. `Default` is plain OCR with no language stated, which is
/// the request Chromium sends when the user has chosen nothing.
#[derive(Clone, Debug, Default)]
pub struct ScanOptions {
    /// `locale_context.language`. Empty means "not stated", which is not the
    /// same request as `"en"`.
    pub ocr_language: Option<String>,
    /// Target language. Setting it is what turns the request into a translate
    /// request; without it Lens returns no translation at all, whatever else
    /// is set.
    pub target_language: Option<String>,
    /// Source language. Left unset, Lens detects it.
    pub source_language: Option<String>,
    pub client_region: Option<String>,
    pub client_time_zone: Option<String>,
    /// What to do with an image that really uses transparency.
    pub alpha_mode: AlphaMode,
}

impl ScanOptions {
    #[must_use]
    pub fn ocr_language(mut self, language: impl Into<String>) -> Self {
        self.ocr_language = Some(language.into());
        self
    }

    /// Ask for a translation into `language`, which is what adds the TRANSLATE
    /// filter to the request.
    #[must_use]
    pub fn translate_to(mut self, language: impl Into<String>) -> Self {
        self.target_language = Some(language.into());
        self
    }

    #[must_use]
    pub fn translate_from(mut self, language: impl Into<String>) -> Self {
        self.source_language = Some(language.into());
        self
    }

    #[must_use]
    pub fn alpha_mode(mut self, mode: AlphaMode) -> Self {
        self.alpha_mode = mode;
        self
    }
}

/// A box around recognised text.
///
/// The coordinates are always normalised: `center_x`/`width` are fractions of
/// the image's width, `center_y`/`height` of its height.
///
/// So multiply by whichever copy you are drawing on - the original or the
/// uploaded one. A downscale preserves the aspect ratio, and a fraction is
/// invariant under that, so both give the right answer. (The uploaded size is
/// on [`Page::image_width`]/[`Page::image_height`] if you need it.)
///
/// That holds even though the server may send either kind of box: a
/// `CoordinateType::Image` box arrives in pixels of the *uploaded* copy, and
/// this crate divides it back down so callers have one rule rather than two.
/// [`GeometryData::coordinate_type`] still says which kind it was.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GeometryData {
    pub center_x: f32,
    pub center_y: f32,
    pub width: f32,
    pub height: f32,
    /// Clockwise rotation in radians, as the server states it.
    pub rotation_z: f32,
    /// The same rotation in degrees, for renderers that want it that way.
    pub angle_deg: f32,
    /// What the server said these coordinates were, before this crate
    /// normalised them. Informational: the four fields above are normalised
    /// either way.
    pub coordinate_type: CoordinateType,
}

#[derive(Clone, Debug)]
pub struct Word {
    pub text: String,
    /// What separates this word from the next - usually a space, empty at the
    /// end of a line, and empty for scripts that do not separate words.
    pub separator: String,
    pub geometry: Option<GeometryData>,
}

#[derive(Clone, Debug)]
pub struct Line {
    pub text: String,
    pub words: Vec<Word>,
    pub geometry: Option<GeometryData>,
    /// This line's share of the paragraph's translation, when there is one.
    ///
    /// Lens does not translate line by line: it returns one string per
    /// paragraph plus ranges saying which part of it belongs where. This is
    /// that text already assembled, so an overlay can draw it at
    /// [`Line::geometry`] - which is where the *source* line was, and where
    /// Chromium puts the translation too, since the server sends no geometry
    /// of its own for translated text.
    ///
    /// It is built from the word ranges the way Chromium's
    /// `CreateTranslatedLineMojomFromProto` does, which means the separator
    /// after the last word is included and a line can therefore **end in a
    /// space**. That is deliberate: it is what makes the lines concatenate
    /// back into [`Translation::text`]. Trim it before measuring or centring.
    pub translation: Option<String>,
}

/// A paragraph's translation.
#[derive(Clone, Debug)]
pub struct Translation {
    /// The whole paragraph, translated. The per-line pieces are on
    /// [`Line::translation`].
    pub text: String,
    /// The language Lens translated from, when it said - useful when it was
    /// asked to detect.
    pub source_language: Option<String>,
    pub target_language: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Paragraph {
    pub text: String,
    pub lines: Vec<Line>,
    pub geometry: Option<GeometryData>,
    /// The language Lens detected for this paragraph, when it said.
    pub language: Option<String>,
    /// This paragraph's translation, when one was asked for and succeeded.
    ///
    /// Lens pairs these with the paragraphs by position, so a paragraph it
    /// could not translate leaves a `None` here rather than shifting the rest
    /// along.
    pub translation: Option<Translation>,
}

#[derive(Clone, Debug)]
pub struct Page {
    /// Every paragraph's text, joined with newlines.
    pub full_text: String,
    pub paragraphs: Vec<Paragraph>,
    /// The language Lens detected for the page, when it said.
    pub language: Option<String>,
    /// Every successful translation, joined with newlines, or `None` when none
    /// came back. Per-paragraph translations are on [`Paragraph::translation`].
    pub translation: Option<String>,
    /// The size of the image as uploaded, which the normalised geometry above
    /// is relative to.
    pub image_width: i32,
    pub image_height: i32,
}

/// Kept as an alias so code written against the previous name still compiles.
pub type LensResult = Page;

/// An HTTP client that sends the request for you.
///
/// Behind the default-on `client` feature. Without it the crate still builds
/// requests and parses replies, which is what a caller with its own HTTP stack
/// wants - and saves linking reqwest, hyper, tokio and a TLS stack beside it.
#[cfg(feature = "client")]
pub struct LensClient {
    client: reqwest::Client,
    api_key: String,
}

#[cfg(feature = "client")]
impl LensClient {
    /// A client with the default API key, the one Chromium ships.
    #[must_use]
    pub fn new(api_key: Option<String>) -> Self {
        Self::build(api_key, None).expect("a client with no proxy cannot fail to build")
    }

    pub fn new_with_proxy(api_key: Option<String>, proxy_url: Option<&str>) -> Result<Self> {
        Self::build(api_key, proxy_url)
    }

    fn build(api_key: Option<String>, proxy_url: Option<&str>) -> Result<Self> {
        let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(60));
        if let Some(proxy) = proxy_url {
            let proxy = reqwest::Proxy::all(proxy)
                .with_context(|| format!("building a proxy from {proxy:?}"))?;
            builder = builder.proxy(proxy);
        }
        Ok(Self {
            client: builder.build().context("building the HTTP client")?,
            api_key: api_key.unwrap_or_else(|| DEFAULT_API_KEY.to_owned()),
        })
    }

    /// The headers this client's requests carry, as name/value pairs.
    ///
    /// See the free [`request_headers`] for what is and is not in here.
    #[must_use]
    pub fn headers(&self) -> Vec<(&'static str, String)> {
        request_headers(&self.api_key)
    }

    /// The API key these requests are sent with.
    #[must_use]
    pub fn api_key(&self) -> &str {
        &self.api_key
    }
}

/// The request Chromium's lens overlay would send for this image.
///
/// Free rather than a method on [`LensClient`], because sending it is the part
/// a caller is most likely to want to do itself - through its own HTTP client,
/// with its own proxy settings, timeouts and cancellation. Pair it with
/// [`request_body`], [`request_headers`] and [`parse_response_bytes`] and
/// [`LensClient`] is not needed at all.
#[must_use]
pub fn objects_request(image: &ProcessedImage, options: &ScanOptions) -> LensOverlayServerRequest {
    let request_id = LensOverlayRequestId {
        uuid: Some(rand::random::<u64>() >> 1),
        sequence_id: Some(1),
        image_sequence_id: Some(1),
        time_usec: Some(now_usec()),
        // `mime_type` stays unset: Chromium states it only for a
        // WebP-with-alpha upload, and ours is always JPEG.
        ..Default::default()
    };

    // Chromium always sends exactly one filter, and TRANSLATE *replaces*
    // AUTO_FILTER rather than joining it. Sending none - which this crate
    // used to do, leaving translation unreachable - is a state real Chrome
    // never produces.
    let filter = match &options.target_language {
        Some(target) => AppliedFilter {
            filter_type: Some(LensOverlayFilterType::Translate as i32),
            filter_payload: Some(applied_filter::FilterPayload::Translate(
                applied_filter::Translate {
                    target_language: Some(target.clone()),
                    source_language: options.source_language.clone(),
                },
            )),
        },
        None => AppliedFilter {
            filter_type: Some(LensOverlayFilterType::AutoFilter as i32),
            filter_payload: None,
        },
    };

    let client_context = LensOverlayClientContext {
        platform: Platform::Web as i32,
        surface: Surface::Chromium as i32,
        locale_context: Some(LocaleContext {
            language: options
                .ocr_language
                .clone()
                .unwrap_or_else(|| DEFAULT_OCR_LANG.to_owned()),
            region: options
                .client_region
                .clone()
                .unwrap_or_else(|| DEFAULT_CLIENT_REGION.to_owned()),
            time_zone: options
                .client_time_zone
                .clone()
                .unwrap_or_else(|| DEFAULT_CLIENT_TIME_ZONE.to_owned()),
        }),
        client_filters: Some(AppliedFilters {
            filter: vec![filter],
        }),
        // Chromium's shipping path pairs SURFACE_CHROMIUM/PLATFORM_WEB with
        // this rendering environment (LensOverlayQueryController::
        // CreateClientContext). Note it never sets app_id, despite the
        // proto marking it required.
        rendering_context: Some(RenderingContext {
            rendering_environment: LensRenderingEnvironment::RenderingEnvLensOverlay as i32,
        }),
        ..Default::default()
    };

    LensOverlayServerRequest {
        objects_request: Some(LensOverlayObjectsRequest {
            request_context: Some(LensOverlayRequestContext {
                request_id: Some(request_id),
                client_context: Some(client_context),
            }),
            image_data: Some(ImageData {
                payload: Some(ImagePayload {
                    image_bytes: image.bytes.clone(),
                }),
                image_metadata: Some(ImageMetadata {
                    width: image.width,
                    height: image.height,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The encoded request body, ready to POST to [`LENS_CRUPLOAD_ENDPOINT`].
pub fn request_body(image: &ProcessedImage, options: &ScanOptions) -> Result<Vec<u8>> {
    let request = objects_request(image, options);
    let mut body = Vec::with_capacity(image.bytes.len() + 512);
    request.encode(&mut body).context("encoding the request")?;
    Ok(body)
}

/// The headers Chromium sends with a `crupload` request, as name/value pairs.
///
/// Transport headers are deliberately absent: `Accept-Encoding`,
/// `Content-Length` and anything to do with connection reuse belong to
/// whichever HTTP client sends this, and a client that is told to accept gzip
/// but does not decompress it will hand back bytes that fail to parse.
#[must_use]
pub fn request_headers(api_key: &str) -> Vec<(&'static str, String)> {
    vec![
        ("content-type", "application/x-protobuf".to_owned()),
        ("accept", "*/*".to_owned()),
        ("user-agent", DEFAULT_USER_AGENT.to_owned()),
        ("x-goog-api-key", api_key.to_owned()),
    ]
}

#[cfg(feature = "client")]
impl LensClient {
    pub async fn scan_path<P: AsRef<Path>>(&self, path: P, options: &ScanOptions) -> Result<Page> {
        let image = image_processor::process_image_from_path(path, options.alpha_mode)?;
        self.scan_processed(&image, options).await
    }

    pub async fn scan_bytes(&self, data: &[u8], options: &ScanOptions) -> Result<Page> {
        let image = image_processor::process_image_from_bytes(data, options.alpha_mode)?;
        self.scan_processed(&image, options).await
    }

    pub async fn scan_processed(
        &self,
        image: &ProcessedImage,
        options: &ScanOptions,
    ) -> Result<Page> {
        let payload = request_body(image, options)?;

        let mut headers = HeaderMap::new();
        for (name, value) in request_headers(&self.api_key) {
            headers.insert(
                HeaderName::from_static(name),
                HeaderValue::from_str(&value)
                    .with_context(|| format!("{name} is not a valid header value"))?,
            );
        }

        log::debug!(
            "POST {} ({} bytes, image {}x{})",
            LENS_CRUPLOAD_ENDPOINT,
            payload.len(),
            image.width,
            image.height
        );

        let response = self
            .client
            .post(LENS_CRUPLOAD_ENDPOINT)
            .headers(headers)
            .body(payload)
            .send()
            .await
            .context("the request to Lens failed")?;

        let status = response.status();
        let body = response.bytes().await.context("reading Lens's reply")?;
        if !status.is_success() {
            bail!(
                "Lens returned {status}: {}",
                String::from_utf8_lossy(&body)
                    .chars()
                    .take(500)
                    .collect::<String>()
            );
        }

        parse_response_bytes(&body, image.width, image.height)
    }

    /// OCR only, by path. Kept for callers written against the older API;
    /// `lang` is the OCR language, not a translation target.
    pub async fn process_image_path(&self, path: &str, lang: Option<&str>) -> Result<Page> {
        let options = ScanOptions {
            ocr_language: lang.map(str::to_owned),
            ..Default::default()
        };
        self.scan_path(path, &options).await
    }

    /// OCR only, from bytes. Kept for callers written against the older API.
    pub async fn process_image_bytes(&self, bytes: &[u8], lang: Option<&str>) -> Result<Page> {
        let options = ScanOptions {
            ocr_language: lang.map(str::to_owned),
            ..Default::default()
        };
        self.scan_bytes(bytes, &options).await
    }
}

fn now_usec() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// The dimensions a response's boxes are read against: the image as uploaded.
#[derive(Clone, Copy, Debug)]
struct Uploaded {
    width: f32,
    height: f32,
}

fn geometry_of(geometry: &Geometry, uploaded: Uploaded) -> Option<GeometryData> {
    let box_ = geometry.bounding_box.as_ref()?;
    let rotation_z = box_.rotation_z.unwrap_or(0.0);
    let coordinate_type = box_
        .coordinate_type
        .and_then(|value| CoordinateType::try_from(value).ok())
        .unwrap_or(CoordinateType::Unspecified);

    // A box can arrive either way. IMAGE means pixels of the image *as
    // uploaded*, so it is divided back down here and everything this crate
    // hands out is normalised - which is what makes the one scaling rule in
    // `GeometryData`'s docs true for every box. NORMALIZED and UNSPECIFIED are
    // both already fractions; the endpoint leaves the type unset for most of
    // what it returns.
    let (sx, sy) = match coordinate_type {
        CoordinateType::Image if uploaded.width > 0.0 && uploaded.height > 0.0 => {
            (1.0 / uploaded.width, 1.0 / uploaded.height)
        }
        _ => (1.0, 1.0),
    };

    Some(GeometryData {
        center_x: box_.center_x.unwrap_or(0.0) * sx,
        center_y: box_.center_y.unwrap_or(0.0) * sy,
        width: box_.width.unwrap_or(0.0) * sx,
        height: box_.height.unwrap_or(0.0) * sy,
        rotation_z,
        angle_deg: rotation_z * (180.0 / PI),
        coordinate_type,
    })
}

fn parse_word(word: &text_layout::Word, uploaded: Uploaded) -> Word {
    Word {
        text: word.plain_text.clone(),
        separator: word.text_separator.clone(),
        geometry: word
            .geometry
            .as_ref()
            .and_then(|g| geometry_of(g, uploaded)),
    }
}

fn parse_line(line: &text_layout::Line, uploaded: Uploaded) -> Line {
    let words: Vec<Word> = line.words.iter().map(|w| parse_word(w, uploaded)).collect();
    let mut text = String::new();
    for word in &words {
        text.push_str(&word.text);
        text.push_str(&word.separator);
    }
    Line {
        text: text.trim_end().to_owned(),
        words,
        geometry: line
            .geometry
            .as_ref()
            .and_then(|g| geometry_of(g, uploaded)),
        translation: None,
    }
}

fn parse_paragraph(paragraph: &text_layout::Paragraph, uploaded: Uploaded) -> Paragraph {
    let lines: Vec<Line> = paragraph
        .lines
        .iter()
        .map(|l| parse_line(l, uploaded))
        .collect();
    let text = lines
        .iter()
        .map(|line| line.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    Paragraph {
        text,
        lines,
        geometry: paragraph
            .geometry
            .as_ref()
            .and_then(|g| geometry_of(g, uploaded)),
        language: non_empty(&paragraph.content_language),
        translation: None,
    }
}

fn non_empty(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

/// The successful translation for each paragraph, by paragraph index.
///
/// **A gleam's position in `deep_gleams` is the paragraph index**, so a gleam
/// that did not succeed has to be skipped *in place*, leaving a `None`. This
/// crate used to filter the failures out and then zip the survivors against
/// the paragraphs, which put every translation after a mid-list failure on the
/// wrong paragraph - silently, and only for images where something failed.
/// chrome-lens-py's `_extract_translation_render_data` enumerates all of them
/// for the same reason.
///
/// Anything other than SUCCESS is a `None`: an unsupported language pair or a
/// same-language no-op carries an empty or echoed string that is worse than
/// nothing in an overlay.
fn translations_by_index(objects: &LensOverlayObjectsResponse) -> Vec<Option<&TranslationData>> {
    objects
        .deep_gleams
        .iter()
        .map(|gleam| {
            let deep_gleam_data::RenderingOneof::Translation(data) =
                gleam.rendering_oneof.as_ref()?;
            if data.status.as_ref()?.code? != translation_data::status::Code::Success as i32 {
                return None;
            }
            if data.translation.as_deref().is_none_or(str::is_empty) {
                return None;
            }
            Some(data)
        })
        .collect()
}

/// The `start..end` slice of a translation, in UTF-16 code units.
///
/// Lens states these ranges in UTF-16 code units, matching
/// `icu::UnicodeString` - the proto's comment says "Unicode characters", which
/// is not the same thing and would be wrong. Rust strings are UTF-8, so the
/// range cannot index one directly: an emoji is one character, two UTF-16 code
/// units and four bytes, and getting that wrong splits text mid-glyph.
///
/// `units` is the translation pre-encoded, so a paragraph of many lines does
/// not re-encode per line.
/// An empty slice is a legitimate answer, so `None` means only that the range
/// was unset or impossible.
fn utf16_slice(units: &[u16], start: Option<i32>, end: Option<i32>) -> Option<String> {
    // Unset means this line has no translation, which the proto states
    // explicitly; a negative or reversed range is the server being odd and is
    // better dropped than clamped into something that looks deliberate.
    let start = usize::try_from(start?).ok()?;
    let end = usize::try_from(end?).ok()?;
    if start > end || end > units.len() {
        return None;
    }
    Some(String::from_utf16_lossy(&units[start..end]))
}

/// One line of a translation, assembled the way Chromium assembles it.
///
/// Not the line's own `start..end` range, which is what you would reach for
/// first: Chromium's `CreateTranslatedLineMojomFromProto` builds the line from
/// the *word* ranges, taking each word plus the gap up to the next one - and
/// for the last word, the gap up to the first word of the following line.
/// chrome-lens-py's `build_line_text` mirrors it. Doing that rather than
/// slicing the line range keeps the pieces concatenating back into the
/// paragraph, and keeps whatever padding the server put in the line range out
/// of the drawn text.
///
/// The trailing gap is kept, so a line can end in a space. That is Chromium's
/// behaviour and it is what makes the concatenation work; trim it if you are
/// measuring or centring the text.
///
/// Falls back to the line's own range when a line carries no word ranges.
fn line_translation(
    units: &[u16],
    line: &translation_data::Line,
    next: Option<&translation_data::Line>,
) -> Option<String> {
    if line.word.is_empty() {
        return utf16_slice(units, line.start, line.end).filter(|text| !text.trim().is_empty());
    }

    let mut text = String::new();
    for (index, word) in line.word.iter().enumerate() {
        if let Some(part) = utf16_slice(units, word.start, word.end) {
            text.push_str(&part);
        }
        let gap_end = match line.word.get(index + 1) {
            Some(following) => following.start,
            // The separator after the last word lives between this line and
            // the next, so it is read across the boundary.
            None => next
                .and_then(|line| line.word.first())
                .and_then(|w| w.start),
        };
        if let Some(part) = utf16_slice(units, word.end, gap_end) {
            text.push_str(&part);
        }
    }

    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Attach a paragraph's translation, and each line's share of it.
fn attach_translation(paragraph: &mut Paragraph, data: &TranslationData) {
    let Some(text) = data.translation.as_deref() else {
        return;
    };
    let units: Vec<u16> = text.encode_utf16().collect();

    // Lens sends one entry per line of the *source* paragraph, in order, so
    // they pair by position. A shorter list leaves the remaining lines without
    // a translation rather than being an error.
    for (index, (line, ranges)) in paragraph.lines.iter_mut().zip(data.line.iter()).enumerate() {
        line.translation = line_translation(&units, ranges, data.line.get(index + 1));
    }

    paragraph.translation = Some(Translation {
        text: text.to_owned(),
        source_language: data.source_language.clone().filter(|s| !s.is_empty()),
        target_language: data.target_language.clone().filter(|s| !s.is_empty()),
    });
}

/// Decode a `crupload` response body and turn it into a [`Page`].
///
/// The counterpart to [`request_body`], for a caller that does its own HTTP.
/// Pass the raw, *decompressed* body: a client that advertised gzip and handed
/// back the compressed bytes fails here with a protobuf error, which is a
/// confusing way to learn about a `Content-Encoding`.
pub fn parse_response_bytes(body: &[u8], image_width: i32, image_height: i32) -> Result<Page> {
    let decoded = LensOverlayServerResponse::decode(body)
        .context("decoding Lens's reply as a LensOverlayServerResponse")?;
    parse_response(&decoded, image_width, image_height)
}

/// Turn a decoded response into a [`Page`].
///
/// `image_width`/`image_height` are the dimensions of the image as uploaded;
/// they are carried through so a caller can un-normalise the geometry without
/// having to remember whether a downscale happened.
pub fn parse_response(
    response: &LensOverlayServerResponse,
    image_width: i32,
    image_height: i32,
) -> Result<Page> {
    // Only a non-zero error_type is a failure. The field has implicit
    // presence, so a response can carry an `error` submessage that says
    // UNKNOWN_TYPE(0) and still have a perfectly good objects_response beside
    // it; chrome-lens-py's request_handler checks `error_type != 0` for the
    // same reason.
    if let Some(error) = &response.error
        && error.error_type != 0
    {
        let kind = lens_overlay_server_error::ErrorType::try_from(error.error_type)
            .map(|t| t.as_str_name().to_owned())
            .unwrap_or_else(|_| format!("error_type {}", error.error_type));
        bail!("Lens reported an error: {kind}");
    }

    let objects = response
        .objects_response
        .as_ref()
        .ok_or_else(|| anyhow!("Lens returned neither an objects response nor an error"))?;

    let uploaded = Uploaded {
        width: image_width as f32,
        height: image_height as f32,
    };

    let mut paragraphs: Vec<Paragraph> = objects
        .text
        .as_ref()
        .and_then(|text| text.text_layout.as_ref())
        .map(|layout| {
            layout
                .paragraphs
                .iter()
                .map(|p| parse_paragraph(p, uploaded))
                .collect()
        })
        .unwrap_or_default();

    // By index, not by position among the successes - see
    // `translations_by_index`. A gleam list longer than the paragraphs is the
    // server translating something we did not parse; ignore the tail rather
    // than failing.
    for (paragraph, data) in paragraphs
        .iter_mut()
        .zip(translations_by_index(objects))
        .filter_map(|(paragraph, data)| data.map(|data| (paragraph, data)))
    {
        attach_translation(paragraph, data);
    }

    let full_text = paragraphs
        .iter()
        .map(|p| p.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let translated: Vec<&str> = paragraphs
        .iter()
        .filter_map(|p| p.translation.as_ref())
        .map(|t| t.text.as_str())
        .collect();
    let translation = if translated.is_empty() {
        None
    } else {
        Some(translated.join("\n"))
    };

    Ok(Page {
        full_text: full_text.trim().to_owned(),
        paragraphs,
        language: objects
            .text
            .as_ref()
            .and_then(|text| non_empty(&text.content_language)),
        translation,
        image_width,
        image_height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> ProcessedImage {
        ProcessedImage {
            bytes: vec![0xFF, 0xD8, 0xFF],
            width: 800,
            height: 600,
        }
    }

    fn context_of(request: &LensOverlayServerRequest) -> &LensOverlayClientContext {
        request
            .objects_request
            .as_ref()
            .unwrap()
            .request_context
            .as_ref()
            .unwrap()
            .client_context
            .as_ref()
            .unwrap()
    }

    #[test]
    fn a_plain_scan_sends_exactly_one_auto_filter() {
        let request = objects_request(&image(), &ScanOptions::default());
        let filters = &context_of(&request).client_filters.as_ref().unwrap().filter;

        assert_eq!(filters.len(), 1, "Chromium sends exactly one filter");
        assert_eq!(
            filters[0].filter_type,
            Some(LensOverlayFilterType::AutoFilter as i32)
        );
        assert!(filters[0].filter_payload.is_none());
    }

    #[test]
    fn a_translate_scan_replaces_auto_filter_rather_than_joining_it() {
        let request = objects_request(
            &image(),
            &ScanOptions::default()
                .translate_to("ru")
                .translate_from("ja"),
        );
        let filters = &context_of(&request).client_filters.as_ref().unwrap().filter;

        assert_eq!(filters.len(), 1);
        assert_eq!(
            filters[0].filter_type,
            Some(LensOverlayFilterType::Translate as i32)
        );
        match filters[0].filter_payload.as_ref().unwrap() {
            applied_filter::FilterPayload::Translate(t) => {
                assert_eq!(t.target_language.as_deref(), Some("ru"));
                assert_eq!(t.source_language.as_deref(), Some("ja"));
            }
        }
    }

    #[test]
    fn the_rendering_environment_is_the_one_chromium_pairs_with_this_surface() {
        let request = objects_request(&image(), &ScanOptions::default());
        let context = context_of(&request);

        assert_eq!(context.platform, Platform::Web as i32);
        assert_eq!(context.surface, Surface::Chromium as i32);
        assert_eq!(
            context
                .rendering_context
                .as_ref()
                .unwrap()
                .rendering_environment,
            LensRenderingEnvironment::RenderingEnvLensOverlay as i32
        );
        // Chromium never sets it, despite the proto marking it required.
        assert!(context.app_id.is_empty());
    }

    #[test]
    fn no_ocr_language_means_an_empty_string_not_en() {
        let request = objects_request(&image(), &ScanOptions::default());
        let locale = context_of(&request).locale_context.as_ref().unwrap();

        assert_eq!(locale.language, "");
        assert_eq!(locale.region, "US");
        assert_eq!(locale.time_zone, "America/New_York");
    }

    #[test]
    fn an_empty_ocr_language_is_not_on_the_wire_at_all() {
        // The field has implicit presence, so the default-valued case must
        // serialize to nothing. This is what the proto3/proto2 split in
        // build.rs buys, and what an all-proto2 mapping would silently break.
        let with_nothing = LocaleContext {
            language: String::new(),
            region: String::new(),
            time_zone: String::new(),
        };
        assert_eq!(with_nothing.encoded_len(), 0);

        let request = objects_request(&image(), &ScanOptions::default());
        let locale = context_of(&request).locale_context.as_ref().unwrap();
        let mut bytes = Vec::new();
        locale.encode(&mut bytes).unwrap();
        // region and time_zone are there, language is not.
        assert!(!bytes.is_empty());
        assert!(
            !bytes.windows(2).any(|w| w == [0x0A, 0x00]),
            "empty language was serialized"
        );
    }

    #[test]
    fn the_request_id_carries_a_timestamp_and_a_positive_uuid() {
        let request = objects_request(&image(), &ScanOptions::default());
        let id = request
            .objects_request
            .as_ref()
            .unwrap()
            .request_context
            .as_ref()
            .unwrap()
            .request_id
            .as_ref()
            .unwrap();

        assert!(id.time_usec.unwrap() > 1_700_000_000_000_000);
        assert_eq!(id.sequence_id, Some(1));
        assert_eq!(id.image_sequence_id, Some(1));
        // JPEG uploads state no mime type, the way Chromium does.
        assert!(id.mime_type.is_none());
    }

    #[test]
    fn the_image_goes_in_with_its_uploaded_dimensions() {
        let request = objects_request(&image(), &ScanOptions::default());
        let data = request
            .objects_request
            .as_ref()
            .unwrap()
            .image_data
            .as_ref()
            .unwrap();

        assert_eq!(
            data.payload.as_ref().unwrap().image_bytes,
            vec![0xFF, 0xD8, 0xFF]
        );
        let metadata = data.image_metadata.as_ref().unwrap();
        assert_eq!((metadata.width, metadata.height), (800, 600));
    }

    #[test]
    fn the_request_round_trips_through_protobuf() {
        let request = objects_request(&image(), &ScanOptions::default().translate_to("de"));
        let mut bytes = Vec::new();
        request.encode(&mut bytes).unwrap();

        let parsed = LensOverlayServerRequest::decode(&*bytes).unwrap();
        assert_eq!(parsed, request);
    }

    #[test]
    fn request_body_encodes_the_same_request_the_builder_returns() {
        let options = ScanOptions::default().translate_to("de");
        let body = request_body(&image(), &options).unwrap();

        // Not byte-compared against a second build: the uuid and timestamp
        // differ per call by design. Decoding is what proves the body is the
        // request rather than something re-encoded on the way out.
        let parsed = LensOverlayServerRequest::decode(&*body).unwrap();
        let filters = &context_of(&parsed).client_filters.as_ref().unwrap().filter;
        assert_eq!(
            filters[0].filter_type,
            Some(LensOverlayFilterType::Translate as i32)
        );
    }

    #[test]
    fn the_headers_are_what_chromium_sends_and_nothing_about_transport() {
        let headers = request_headers("test-key");
        let names: Vec<&str> = headers.iter().map(|(name, _)| *name).collect();

        assert_eq!(
            names,
            ["content-type", "accept", "user-agent", "x-goog-api-key"]
        );
        assert_eq!(headers[0].1, "application/x-protobuf");
        assert_eq!(headers[3].1, "test-key");
        // Content-Encoding and friends belong to whoever sends this; promising
        // gzip here and not decompressing would surface as a protobuf error.
        assert!(!names.iter().any(|n| n.contains("encoding")));
        assert!(!names.iter().any(|n| n.contains("length")));
    }

    #[test]
    fn a_client_carries_its_own_key_into_its_headers() {
        let client = LensClient::new(Some("mine".to_owned()));
        assert_eq!(client.api_key(), "mine");
        assert!(
            client
                .headers()
                .contains(&("x-goog-api-key", "mine".to_owned()))
        );

        let default = LensClient::new(None);
        assert_eq!(default.api_key(), DEFAULT_API_KEY);
    }

    #[test]
    fn parse_response_bytes_reads_what_the_endpoint_would_return() {
        let mut encoded = Vec::new();
        response_with(&["hello world"])
            .encode(&mut encoded)
            .unwrap();

        let page = parse_response_bytes(&encoded, 800, 600).unwrap();
        assert_eq!(page.full_text, "hello world");
        assert_eq!((page.image_width, page.image_height), (800, 600));
    }

    #[test]
    fn parse_response_bytes_says_it_is_not_protobuf_rather_than_guessing() {
        // What a gzipped body looks like to the parser.
        let message = parse_response_bytes(&[0x1F, 0x8B, 0x08, 0x00], 1, 1)
            .unwrap_err()
            .to_string();
        assert!(message.contains("decoding"), "{message}");
    }

    fn response_with(paragraph_texts: &[&str]) -> LensOverlayServerResponse {
        let paragraphs = paragraph_texts
            .iter()
            .map(|text| text_layout::Paragraph {
                lines: vec![text_layout::Line {
                    words: text
                        .split(' ')
                        .map(|w| text_layout::Word {
                            plain_text: w.to_owned(),
                            text_separator: " ".to_owned(),
                            ..Default::default()
                        })
                        .collect(),
                    geometry: None,
                }],
                ..Default::default()
            })
            .collect();

        LensOverlayServerResponse {
            objects_response: Some(LensOverlayObjectsResponse {
                text: Some(Text {
                    text_layout: Some(TextLayout { paragraphs }),
                    content_language: "ja".to_owned(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn gleam(text: &str, code: translation_data::status::Code) -> DeepGleamData {
        DeepGleamData {
            rendering_oneof: Some(deep_gleam_data::RenderingOneof::Translation(
                TranslationData {
                    status: Some(translation_data::Status {
                        code: Some(code as i32),
                    }),
                    translation: Some(text.to_owned()),
                    ..Default::default()
                },
            )),
            ..Default::default()
        }
    }

    /// A successful translation carrying one range per source line.
    fn translated_gleam(text: &str, ranges: &[(Option<i32>, Option<i32>)]) -> DeepGleamData {
        DeepGleamData {
            rendering_oneof: Some(deep_gleam_data::RenderingOneof::Translation(
                TranslationData {
                    status: Some(translation_data::Status {
                        code: Some(translation_data::status::Code::Success as i32),
                    }),
                    translation: Some(text.to_owned()),
                    line: ranges
                        .iter()
                        .map(|(start, end)| translation_data::Line {
                            start: *start,
                            end: *end,
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                },
            )),
            ..Default::default()
        }
    }

    fn translation_of(page: &Page, index: usize) -> Option<&str> {
        page.paragraphs[index]
            .translation
            .as_ref()
            .map(|t| t.text.as_str())
    }

    #[test]
    fn words_are_joined_by_their_separators_and_lines_by_newlines() {
        let page =
            parse_response(&response_with(&["hello world", "second one"]), 800, 600).unwrap();

        assert_eq!(page.full_text, "hello world\nsecond one");
        assert_eq!(page.paragraphs.len(), 2);
        assert_eq!(page.paragraphs[0].lines[0].text, "hello world");
        assert_eq!(page.language.as_deref(), Some("ja"));
        assert_eq!((page.image_width, page.image_height), (800, 600));
    }

    #[test]
    fn translations_line_up_with_paragraphs_by_index() {
        let mut response = response_with(&["one", "two"]);
        let objects = response.objects_response.as_mut().unwrap();
        objects.deep_gleams = vec![
            gleam("один", translation_data::status::Code::Success),
            gleam("два", translation_data::status::Code::Success),
        ];

        let page = parse_response(&response, 1, 1).unwrap();
        assert_eq!(translation_of(&page, 0), Some("один"));
        assert_eq!(translation_of(&page, 1), Some("два"));
        assert_eq!(page.translation.as_deref(), Some("один\nдва"));
    }

    #[test]
    fn a_failed_translation_leaves_a_hole_and_does_not_shift_the_rest() {
        // The gleam's index IS the paragraph index. Filtering the failures out
        // first and then zipping - which this crate used to do - moved "три"
        // onto the second paragraph and left the third untranslated: silently,
        // and only for images where something failed.
        let mut response = response_with(&["one", "two", "three"]);
        let objects = response.objects_response.as_mut().unwrap();
        objects.deep_gleams = vec![
            gleam("один", translation_data::status::Code::Success),
            gleam("", translation_data::status::Code::UnsupportedLanguagePair),
            gleam("три", translation_data::status::Code::Success),
        ];

        let page = parse_response(&response, 1, 1).unwrap();
        assert_eq!(translation_of(&page, 0), Some("один"));
        assert_eq!(
            translation_of(&page, 1),
            None,
            "the failure must leave a hole"
        );
        assert_eq!(translation_of(&page, 2), Some("три"));
        assert_eq!(page.translation.as_deref(), Some("один\nтри"));
    }

    #[test]
    fn a_same_language_no_op_counts_as_a_failure() {
        // It echoes the source back, which is worse than nothing in an overlay.
        let mut response = response_with(&["one"]);
        response.objects_response.as_mut().unwrap().deep_gleams =
            vec![gleam("one", translation_data::status::Code::SameLanguage)];

        let page = parse_response(&response, 1, 1).unwrap();
        assert_eq!(translation_of(&page, 0), None);
        assert!(page.translation.is_none());
    }

    #[test]
    fn fewer_translations_than_paragraphs_is_not_an_error() {
        let mut response = response_with(&["one", "two", "three"]);
        response.objects_response.as_mut().unwrap().deep_gleams =
            vec![gleam("один", translation_data::status::Code::Success)];

        let page = parse_response(&response, 1, 1).unwrap();
        assert_eq!(translation_of(&page, 0), Some("один"));
        assert!(page.paragraphs[1].translation.is_none());
        assert!(page.paragraphs[2].translation.is_none());
    }

    #[test]
    fn each_line_gets_its_slice_of_the_paragraph_translation() {
        let mut response = response_with(&["one two"]);
        // A second source line, so there are two ranges into "привет мир".
        let layout = response
            .objects_response
            .as_mut()
            .unwrap()
            .text
            .as_mut()
            .unwrap()
            .text_layout
            .as_mut()
            .unwrap();
        layout.paragraphs[0]
            .lines
            .push(text_layout::Line::default());

        response.objects_response.as_mut().unwrap().deep_gleams = vec![translated_gleam(
            "привет мир",
            &[(Some(0), Some(6)), (Some(7), Some(10))],
        )];

        let page = parse_response(&response, 1, 1).unwrap();
        assert_eq!(translation_of(&page, 0), Some("привет мир"));
        assert_eq!(
            page.paragraphs[0].lines[0].translation.as_deref(),
            Some("привет")
        );
        assert_eq!(
            page.paragraphs[0].lines[1].translation.as_deref(),
            Some("мир")
        );
    }

    #[test]
    fn a_line_is_assembled_from_word_ranges_with_the_gap_after_each_word() {
        // "привет мир" as two words on one line. Chromium takes each word plus
        // the gap up to the next, so the space between them comes along.
        let units: Vec<u16> = "привет мир".encode_utf16().collect();
        let line = translation_data::Line {
            word: vec![
                translation_data::line::Word {
                    start: Some(0),
                    end: Some(6),
                },
                translation_data::line::Word {
                    start: Some(7),
                    end: Some(10),
                },
            ],
            ..Default::default()
        };

        assert_eq!(
            line_translation(&units, &line, None).as_deref(),
            Some("привет мир")
        );
    }

    #[test]
    fn the_separator_after_the_last_word_is_read_from_the_next_line() {
        // Chromium's CreateTranslatedLineMojomFromProto crosses the boundary
        // for the last word, which is why a line can end in a space - and why
        // the lines concatenate back into the paragraph.
        let units: Vec<u16> = "привет мир".encode_utf16().collect();
        let first = translation_data::Line {
            word: vec![translation_data::line::Word {
                start: Some(0),
                end: Some(6),
            }],
            ..Default::default()
        };
        let second = translation_data::Line {
            word: vec![translation_data::line::Word {
                start: Some(7),
                end: Some(10),
            }],
            ..Default::default()
        };

        let one = line_translation(&units, &first, Some(&second)).unwrap();
        let two = line_translation(&units, &second, None).unwrap();

        assert_eq!(one, "привет ", "the trailing space is deliberate");
        assert_eq!(two, "мир");
        assert_eq!(one + &two, "привет мир", "the lines must concatenate back");
    }

    #[test]
    fn a_line_with_no_word_ranges_falls_back_to_its_own_range() {
        let units: Vec<u16> = "привет мир".encode_utf16().collect();
        let line = translation_data::Line {
            start: Some(0),
            end: Some(6),
            ..Default::default()
        };

        assert_eq!(
            line_translation(&units, &line, None).as_deref(),
            Some("привет")
        );
    }

    #[test]
    fn a_line_whose_assembly_is_only_whitespace_has_no_translation() {
        let units: Vec<u16> = "a  b".encode_utf16().collect();
        let line = translation_data::Line {
            start: Some(1),
            end: Some(3),
            ..Default::default()
        };

        assert!(line_translation(&units, &line, None).is_none());
    }

    #[test]
    fn line_ranges_are_utf16_code_units_not_characters_or_bytes() {
        // "ab" is two units; the emoji is one character, two UTF-16 units and
        // four UTF-8 bytes. So 2..4 is the emoji under UTF-16, nothing under
        // characters, and half of it under bytes.
        let text = "ab\u{1F600}cd";
        let units: Vec<u16> = text.encode_utf16().collect();

        assert_eq!(
            utf16_slice(&units, Some(2), Some(4)).as_deref(),
            Some("\u{1F600}")
        );
        assert_eq!(utf16_slice(&units, Some(0), Some(2)).as_deref(), Some("ab"));
        assert_eq!(utf16_slice(&units, Some(4), Some(6)).as_deref(), Some("cd"));
    }

    #[test]
    fn an_absent_or_impossible_line_range_is_no_translation_rather_than_a_panic() {
        let units: Vec<u16> = "hello".encode_utf16().collect();

        assert!(utf16_slice(&units, None, Some(3)).is_none(), "unset start");
        assert!(utf16_slice(&units, Some(0), None).is_none(), "unset end");
        assert!(utf16_slice(&units, Some(3), Some(1)).is_none(), "reversed");
        assert!(
            utf16_slice(&units, Some(0), Some(99)).is_none(),
            "past the end"
        );
        assert!(utf16_slice(&units, Some(-1), Some(3)).is_none(), "negative");
        // An empty range is a legitimate answer, not a missing one: it is what
        // a zero-width gap between two adjacent words looks like.
        assert_eq!(utf16_slice(&units, Some(2), Some(2)).as_deref(), Some(""));
    }

    #[test]
    fn an_error_submessage_of_type_zero_is_not_an_error() {
        // The field has implicit presence, so a good response can carry an
        // `error` that says UNKNOWN_TYPE(0). chrome-lens-py checks
        // `error_type != 0` for the same reason.
        let mut response = response_with(&["hello world"]);
        response.error = Some(LensOverlayServerError::default());

        let page = parse_response(&response, 1, 1).expect("not an error");
        assert_eq!(page.full_text, "hello world");
    }

    #[test]
    fn a_response_with_no_text_is_an_empty_page_not_a_failure() {
        let response = LensOverlayServerResponse {
            objects_response: Some(LensOverlayObjectsResponse::default()),
            ..Default::default()
        };
        let page = parse_response(&response, 1, 1).unwrap();
        assert!(page.full_text.is_empty());
        assert!(page.paragraphs.is_empty());
        assert!(page.translation.is_none());
    }

    #[test]
    fn a_server_error_is_reported_with_its_name() {
        let response = LensOverlayServerResponse {
            error: Some(LensOverlayServerError {
                error_type: lens_overlay_server_error::ErrorType::MissingRequest as i32,
                ..Default::default()
            }),
            ..Default::default()
        };
        let message = parse_response(&response, 1, 1).unwrap_err().to_string();
        assert!(message.contains("MISSING_REQUEST"), "{message}");
    }

    #[test]
    fn an_empty_response_says_so_rather_than_pretending_to_have_read_nothing() {
        let message = parse_response(&LensOverlayServerResponse::default(), 1, 1)
            .unwrap_err()
            .to_string();
        assert!(message.contains("neither"), "{message}");
    }

    #[test]
    fn geometry_is_carried_through_with_degrees_alongside_radians() {
        let mut response = response_with(&["x"]);
        let objects = response.objects_response.as_mut().unwrap();
        let layout = objects.text.as_mut().unwrap().text_layout.as_mut().unwrap();
        layout.paragraphs[0].geometry = Some(Geometry {
            bounding_box: Some(CenterRotatedBox {
                center_x: Some(0.5),
                center_y: Some(0.25),
                width: Some(0.4),
                height: Some(0.1),
                rotation_z: Some(PI / 2.0),
                coordinate_type: Some(CoordinateType::Normalized as i32),
            }),
            ..Default::default()
        });

        let page = parse_response(&response, 1000, 500).unwrap();
        let geometry = page.paragraphs[0].geometry.unwrap();
        assert_eq!(geometry.center_x, 0.5);
        assert!((geometry.angle_deg - 90.0).abs() < 0.001);
        // Normalised, so pixels are the caller's multiplication to do.
        assert_eq!(geometry.center_x * page.image_width as f32, 500.0);
    }

    #[test]
    fn a_pixel_typed_box_is_divided_back_down_to_a_fraction() {
        // coordinate_type IMAGE means pixels of the image as uploaded. Left
        // alone, a caller applying the one documented rule would multiply
        // pixels by a size and land far off the picture.
        let mut response = response_with(&["x"]);
        let objects = response.objects_response.as_mut().unwrap();
        let layout = objects.text.as_mut().unwrap().text_layout.as_mut().unwrap();
        layout.paragraphs[0].geometry = Some(Geometry {
            bounding_box: Some(CenterRotatedBox {
                center_x: Some(800.0),
                center_y: Some(300.0),
                width: Some(400.0),
                height: Some(100.0),
                rotation_z: Some(0.0),
                coordinate_type: Some(CoordinateType::Image as i32),
            }),
            ..Default::default()
        });

        // Uploaded at 1600x600, so 800px across is half the width.
        let page = parse_response(&response, 1600, 600).unwrap();
        let geometry = page.paragraphs[0].geometry.unwrap();
        assert_eq!(geometry.center_x, 0.5);
        assert_eq!(geometry.center_y, 0.5);
        assert_eq!(geometry.width, 0.25);
        assert_eq!(geometry.coordinate_type, CoordinateType::Image);

        // And the one rule now works against the original, 3200x1200.
        assert_eq!(geometry.center_x * 3200.0, 1600.0);
    }

    #[test]
    fn a_box_with_no_stated_type_is_taken_as_already_normalised() {
        let mut response = response_with(&["x"]);
        let objects = response.objects_response.as_mut().unwrap();
        let layout = objects.text.as_mut().unwrap().text_layout.as_mut().unwrap();
        layout.paragraphs[0].geometry = Some(Geometry {
            bounding_box: Some(CenterRotatedBox {
                center_x: Some(0.5),
                width: Some(0.25),
                coordinate_type: None,
                ..Default::default()
            }),
            ..Default::default()
        });

        let page = parse_response(&response, 1600, 600).unwrap();
        let geometry = page.paragraphs[0].geometry.unwrap();
        assert_eq!(geometry.center_x, 0.5);
        assert_eq!(geometry.width, 0.25);
        assert_eq!(geometry.coordinate_type, CoordinateType::Unspecified);
    }
}
