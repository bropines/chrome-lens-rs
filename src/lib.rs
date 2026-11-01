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

use std::f32::consts::PI;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use prost::Message;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};

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
/// The coordinates are normalised to the image: `center_x`/`width` are
/// fractions of its width, `center_y`/`height` of its height. Multiply by the
/// dimensions of the image **as uploaded** ([`ProcessedImage::width`]) to get
/// pixels - which equal the original's only when no downscale happened.
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
}

#[derive(Clone, Debug)]
pub struct Paragraph {
    pub text: String,
    pub lines: Vec<Line>,
    pub geometry: Option<GeometryData>,
    /// The language Lens detected for this paragraph, when it said.
    pub language: Option<String>,
    /// This paragraph's translation, when one was asked for and returned.
    ///
    /// Lens returns translations in a list parallel to the paragraphs, so this
    /// is the entry at the same index. That correspondence is how the endpoint
    /// behaves rather than something the protos promise, so treat a mismatch in
    /// length as "no translation for this paragraph" - which is what happens
    /// here - rather than as an error.
    pub translation: Option<String>,
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

pub struct LensClient {
    client: reqwest::Client,
    api_key: String,
}

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

    /// The request Chromium's lens overlay would send for this image.
    ///
    /// Public so a caller can inspect or diff the bytes; nothing else needs it.
    #[must_use]
    pub fn objects_request(
        &self,
        image: &ProcessedImage,
        options: &ScanOptions,
    ) -> LensOverlayServerRequest {
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
        let request = self.objects_request(image, options);
        let mut payload = Vec::with_capacity(image.bytes.len() + 512);
        request
            .encode(&mut payload)
            .context("encoding the request")?;

        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/x-protobuf"),
        );
        headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
        headers.insert(USER_AGENT, HeaderValue::from_static(DEFAULT_USER_AGENT));
        headers.insert(
            "X-Goog-Api-Key",
            HeaderValue::from_str(&self.api_key).context("the API key is not a valid header")?,
        );

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

        let decoded = LensOverlayServerResponse::decode(body)
            .context("decoding Lens's reply as a LensOverlayServerResponse")?;
        parse_response(&decoded, image.width, image.height)
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

fn geometry_of(geometry: &Geometry) -> Option<GeometryData> {
    let box_ = geometry.bounding_box.as_ref()?;
    let rotation_z = box_.rotation_z.unwrap_or(0.0);
    Some(GeometryData {
        center_x: box_.center_x.unwrap_or(0.0),
        center_y: box_.center_y.unwrap_or(0.0),
        width: box_.width.unwrap_or(0.0),
        height: box_.height.unwrap_or(0.0),
        rotation_z,
        angle_deg: rotation_z * (180.0 / PI),
    })
}

fn parse_word(word: &text_layout::Word) -> Word {
    Word {
        text: word.plain_text.clone(),
        separator: word.text_separator.clone(),
        geometry: word.geometry.as_ref().and_then(geometry_of),
    }
}

fn parse_line(line: &text_layout::Line) -> Line {
    let words: Vec<Word> = line.words.iter().map(parse_word).collect();
    let mut text = String::new();
    for word in &words {
        text.push_str(&word.text);
        text.push_str(&word.separator);
    }
    Line {
        text: text.trim_end().to_owned(),
        words,
        geometry: line.geometry.as_ref().and_then(geometry_of),
    }
}

fn parse_paragraph(paragraph: &text_layout::Paragraph) -> Paragraph {
    let lines: Vec<Line> = paragraph.lines.iter().map(parse_line).collect();
    let text = lines
        .iter()
        .map(|line| line.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    Paragraph {
        text,
        lines,
        geometry: paragraph.geometry.as_ref().and_then(geometry_of),
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

/// Every translation Lens actually produced, in the order it listed them.
///
/// Anything other than SUCCESS is dropped: an unsupported language pair or a
/// same-language no-op carries an empty or echoed string that is worse than
/// nothing for a caller rendering an overlay.
fn successful_translations(objects: &LensOverlayObjectsResponse) -> Vec<String> {
    objects
        .deep_gleams
        .iter()
        .filter_map(|gleam| match gleam.rendering_oneof.as_ref()? {
            deep_gleam_data::RenderingOneof::Translation(data) => {
                let code = data.status.as_ref()?.code?;
                if code != translation_data::status::Code::Success as i32 {
                    return None;
                }
                let text = data.translation.as_deref()?;
                if text.is_empty() {
                    None
                } else {
                    Some(text.to_owned())
                }
            }
        })
        .collect()
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
    if let Some(error) = &response.error {
        let kind = lens_overlay_server_error::ErrorType::try_from(error.error_type)
            .map(|t| t.as_str_name().to_owned())
            .unwrap_or_else(|_| format!("error_type {}", error.error_type));
        bail!("Lens reported an error: {kind}");
    }

    let objects = response
        .objects_response
        .as_ref()
        .ok_or_else(|| anyhow!("Lens returned neither an objects response nor an error"))?;

    let mut paragraphs: Vec<Paragraph> = objects
        .text
        .as_ref()
        .and_then(|text| text.text_layout.as_ref())
        .map(|layout| layout.paragraphs.iter().map(parse_paragraph).collect())
        .unwrap_or_default();

    // Lens lists translations in a list parallel to the paragraphs. Pair them
    // up only as far as both go: a shorter list means some paragraphs have no
    // translation, which is not an error.
    let translations = successful_translations(objects);
    for (paragraph, translation) in paragraphs.iter_mut().zip(translations.iter()) {
        paragraph.translation = Some(translation.clone());
    }

    let full_text = paragraphs
        .iter()
        .map(|p| p.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let translation = if translations.is_empty() {
        None
    } else {
        Some(translations.join("\n"))
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
        let client = LensClient::new(None);
        let request = client.objects_request(&image(), &ScanOptions::default());
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
        let client = LensClient::new(None);
        let request = client.objects_request(
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
        let client = LensClient::new(None);
        let request = client.objects_request(&image(), &ScanOptions::default());
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
        let client = LensClient::new(None);
        let request = client.objects_request(&image(), &ScanOptions::default());
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
        let client = LensClient::new(None);
        let with_nothing = LocaleContext {
            language: String::new(),
            region: String::new(),
            time_zone: String::new(),
        };
        assert_eq!(with_nothing.encoded_len(), 0);

        let request = client.objects_request(&image(), &ScanOptions::default());
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
        let client = LensClient::new(None);
        let request = client.objects_request(&image(), &ScanOptions::default());
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
        let client = LensClient::new(None);
        let request = client.objects_request(&image(), &ScanOptions::default());
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
        let client = LensClient::new(None);
        let request = client.objects_request(&image(), &ScanOptions::default().translate_to("de"));
        let mut bytes = Vec::new();
        request.encode(&mut bytes).unwrap();

        let parsed = LensOverlayServerRequest::decode(&*bytes).unwrap();
        assert_eq!(parsed, request);
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
        assert_eq!(page.paragraphs[0].translation.as_deref(), Some("один"));
        assert_eq!(page.paragraphs[1].translation.as_deref(), Some("два"));
        assert_eq!(page.translation.as_deref(), Some("один\nдва"));
    }

    #[test]
    fn a_failed_translation_is_dropped_rather_than_shown() {
        let mut response = response_with(&["one", "two"]);
        let objects = response.objects_response.as_mut().unwrap();
        objects.deep_gleams = vec![
            gleam("", translation_data::status::Code::UnsupportedLanguagePair),
            gleam("два", translation_data::status::Code::Success),
        ];

        let page = parse_response(&response, 1, 1).unwrap();
        // The only surviving translation lands on the first paragraph, because
        // the failed one left no entry at all.
        assert_eq!(page.paragraphs[0].translation.as_deref(), Some("два"));
        assert_eq!(page.translation.as_deref(), Some("два"));
    }

    #[test]
    fn fewer_translations_than_paragraphs_is_not_an_error() {
        let mut response = response_with(&["one", "two", "three"]);
        response.objects_response.as_mut().unwrap().deep_gleams =
            vec![gleam("один", translation_data::status::Code::Success)];

        let page = parse_response(&response, 1, 1).unwrap();
        assert_eq!(page.paragraphs[0].translation.as_deref(), Some("один"));
        assert!(page.paragraphs[1].translation.is_none());
        assert!(page.paragraphs[2].translation.is_none());
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
}
