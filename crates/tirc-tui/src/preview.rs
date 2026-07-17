//! Open Graph link previews. Mirrors the inline-image pipeline
//! ([`super::renderer`] + the `image_decode_worker` in `crate::main`): the
//! renderer sends a [`PreviewRequest`] for each URL it draws, a background worker
//! fetches the page, parses its Open Graph metadata (and downloads the preview
//! image so the existing image pipeline can render the thumbnail), and hands a
//! [`PreviewResult`] back to the main loop which caches it in the renderer.
//!
//! Everything here runs off the main loop; nothing touches `mlua` or the UI
//! state, so parsing (which uses the non-`Send` `scraper::Html`) is confined to
//! synchronous helpers that return owned data before any `.await`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::Duration;

use linkify::{LinkFinder, LinkKind};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// Absolute cap on how much of an HTML document is read while looking for the
/// `</head>` that ends the metadata. Generous because some pages (e.g. YouTube)
/// inline hundreds of KB of JSON ahead of their Open Graph tags; the read stops
/// as soon as `</head>` is seen, so the body is normally never downloaded.
const MAX_HTML_BYTES: usize = 4 * 1024 * 1024;

/// The tag that ends the document head; once seen, all OG metadata has been read.
const HEAD_END: &[u8] = b"</head>";

/// Cap on a downloaded preview image, mirroring the media cap in the Matrix
/// backend so a hostile `og:image` cannot fill the cache.
const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;

/// How long a single page (or image) fetch may take before it is abandoned.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound on any single extracted text field (title/description/site name),
/// a guard against a pathologically long tag. The theme handles finer display
/// truncation.
const MAX_TEXT_CHARS: usize = 2000;

/// The parsed Open Graph metadata for one URL. Any field may be absent; a preview
/// with no title and no image is treated as "nothing to show" (see
/// [`LinkPreview::is_empty`]).
#[derive(Debug, Clone, Default)]
pub struct LinkPreview {
    pub title: Option<String>,
    pub description: Option<String>,
    pub site_name: Option<String>,
    /// Local cache path of the downloaded `og:image`, when one was fetched. Feeds
    /// the existing inline-image pipeline (decoded/drawn like any other image).
    pub image_path: Option<PathBuf>,
    /// Pixel dimensions of the `og:image`, read from its header when downloaded.
    /// Lets the renderer reserve the thumbnail's exact cell height before the
    /// image is decoded, so a cached preview does not shift rows when its image
    /// lands (see `Renderer::predicted_thumb_height`).
    pub image_dims: Option<(u32, u32)>,
}

impl LinkPreview {
    /// A preview worth rendering has at least a title or a thumbnail.
    fn is_empty(&self) -> bool {
        self.title.is_none() && self.image_path.is_none()
    }
}

/// A request to fetch and parse one URL's preview, sent by the renderer.
#[derive(Debug, Clone)]
pub struct PreviewRequest {
    pub url: String,
}

/// The result of a background fetch, handed back to the main loop and cached in
/// the renderer. `preview` is `None` when nothing usable was found (or the fetch
/// failed), so the renderer records the URL as failed and stops re-requesting it.
#[derive(Debug, Clone)]
pub struct PreviewResult {
    pub url: String,
    pub preview: Option<LinkPreview>,
}

/// Extracts the distinct http(s) URLs from a message body, in order of first
/// appearance. Non-http schemes are ignored so we never fetch, e.g., `file://`.
pub fn extract_urls(text: &str) -> Vec<String> {
    let mut finder = LinkFinder::new();
    finder.kinds(&[LinkKind::Url]);

    let mut urls = Vec::new();
    for link in finder.links(text) {
        let url = link.as_str();
        if url.starts_with("http://") || url.starts_with("https://") {
            let url = url.to_string();
            if !urls.contains(&url) {
                urls.push(url);
            }
        }
    }
    urls
}

/// Builds the shared HTTP client used for all preview fetches: a real
/// `User-Agent` (many sites omit OG tags for unknown agents), a request timeout,
/// and a capped redirect chain.
pub fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!(
            "tirc/",
            env!("CARGO_PKG_VERSION"),
            " (link-preview)"
        ))
        .timeout(FETCH_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .unwrap_or_default()
}

/// Background preview worker. Mirrors `image_decode_worker`: receives requests,
/// fetches/parses each on its own task so several previews in a freshly opened
/// buffer resolve concurrently, and sends the result back for the main loop to
/// cache.
pub async fn link_preview_worker(
    client: reqwest::Client,
    cache_dir: PathBuf,
    mut requests: UnboundedReceiver<PreviewRequest>,
    results: UnboundedSender<PreviewResult>,
) {
    while let Some(request) = requests.recv().await {
        let client = client.clone();
        let cache_dir = cache_dir.clone();
        let results = results.clone();
        tokio::spawn(async move {
            let preview = fetch_preview(&client, &cache_dir, &request.url).await;
            let _ = results.send(PreviewResult {
                url: request.url,
                preview,
            });
        });
    }
}

/// Fetches one URL and returns its preview, or `None` when the page is not HTML,
/// the fetch fails, or no usable metadata is present.
async fn fetch_preview(
    client: &reqwest::Client,
    cache_dir: &Path,
    url: &str,
) -> Option<LinkPreview> {
    let response = client.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    if !is_html(&response) {
        return None;
    }

    let html = read_html_head(response, MAX_HTML_BYTES).await?;
    let html = String::from_utf8_lossy(&html);

    // `parse_og` fully consumes the non-`Send` `scraper::Html` and returns owned
    // strings, so nothing un-`Send` is held across the image download below.
    let og = parse_og(&html);
    if og.title.is_none() && og.description.is_none() && og.image_url.is_none() {
        return None;
    }

    let image_path = match &og.image_url {
        Some(image_url) => download_image(client, cache_dir, image_url).await,
        None => None,
    };

    // Read the image's pixel dimensions from its header (cheap, no full decode);
    // works whether it was just downloaded or already on disk from a prior run.
    let image_dims = image_path
        .as_ref()
        .and_then(|path| image::image_dimensions(path).ok());

    let preview = LinkPreview {
        title: og.title,
        description: og.description,
        site_name: og.site_name,
        image_path,
        image_dims,
    };

    (!preview.is_empty()).then_some(preview)
}

/// Whether a response advertises an HTML content type (so parsing it for OG tags
/// is worthwhile). Absence of the header is treated as non-HTML to avoid parsing
/// binary bodies.
fn is_html(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.contains("text/html") || value.contains("application/xhtml"))
        .unwrap_or(false)
}

/// Reads an HTML response only as far as its `</head>`, or `cap` bytes, whichever
/// comes first. Open Graph tags live in `<head>`, so this avoids downloading the
/// (often multi-MB) body while still tolerating pages that inline a lot of markup
/// ahead of their metadata.
async fn read_html_head(mut response: reqwest::Response, cap: usize) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    let mut scanned = 0usize;
    while let Ok(Some(chunk)) = response.chunk().await {
        buf.extend_from_slice(&chunk);
        // Search from just before where the previous scan ended so a `</head>`
        // straddling a chunk boundary is still found, without rescanning all of
        // `buf` each time.
        let from = scanned.saturating_sub(HEAD_END.len());
        if let Some(pos) = find_ignore_case(&buf[from..], HEAD_END) {
            buf.truncate(from + pos + HEAD_END.len());
            return Some(buf);
        }
        scanned = buf.len();
        if buf.len() >= cap {
            buf.truncate(cap);
            break;
        }
    }
    (!buf.is_empty()).then_some(buf)
}

/// Case-insensitive (ASCII) substring search over bytes, returning the start
/// offset of the first match.
fn find_ignore_case(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
}

/// Reads a response body up to `cap` bytes, stopping early once the cap is hit so
/// an oversized page is never fully buffered.
async fn read_capped(mut response: reqwest::Response, cap: usize) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        buf.extend_from_slice(&chunk);
        if buf.len() >= cap {
            buf.truncate(cap);
            break;
        }
    }
    (!buf.is_empty()).then_some(buf)
}

/// Downloads an `og:image` into the cache directory and returns its path, capped
/// at [`MAX_IMAGE_BYTES`]. The file name is a hash of the image URL; the byte
/// content (not the extension) determines decoding later, so no extension is
/// needed. Best-effort: any failure yields `None` and the preview stays text-only.
async fn download_image(
    client: &reqwest::Client,
    cache_dir: &Path,
    image_url: &str,
) -> Option<PathBuf> {
    let path = cache_dir.join(cache_key(image_url));
    if path.exists() {
        return Some(path);
    }

    let response = client.get(image_url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }

    let bytes = read_capped(response, MAX_IMAGE_BYTES).await?;
    match tokio::fs::write(&path, &bytes).await {
        Ok(()) => Some(path),
        Err(err) => {
            log::warn!("failed to cache preview image at {path:?}: {err}");
            None
        }
    }
}

/// A stable cache filename for a URL: its hash rendered as hex.
fn cache_key(url: &str) -> String {
    let mut hasher = DefaultHasher::new();
    url.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Owned Open Graph fields extracted from a document, kept separate from
/// [`LinkPreview`] because the image is still a URL here (not yet downloaded).
#[derive(Debug, Default)]
struct OgData {
    title: Option<String>,
    description: Option<String>,
    site_name: Option<String>,
    image_url: Option<String>,
}

/// Parses Open Graph `<meta property="og:*">` tags (plus a `<title>` fallback)
/// out of an HTML document. Confined to a synchronous scope so the non-`Send`
/// `scraper::Html` never escapes.
fn parse_og(html: &str) -> OgData {
    use scraper::{Html, Selector};

    let document = Html::parse_document(html);

    let mut data = OgData::default();

    if let Ok(selector) = Selector::parse("meta[property][content], meta[name][content]") {
        for element in document.select(&selector) {
            let key = element
                .value()
                .attr("property")
                .or_else(|| element.value().attr("name"));
            let Some(content) = element.value().attr("content") else {
                continue;
            };
            let content = clean(content);
            if content.is_empty() {
                continue;
            }
            match key {
                Some("og:title") => set_if_unset(&mut data.title, &content),
                Some("og:description") | Some("description") => {
                    set_if_unset(&mut data.description, &content)
                }
                Some("og:site_name") => set_if_unset(&mut data.site_name, &content),
                Some("og:image") | Some("og:image:url") | Some("og:image:secure_url") => {
                    set_if_unset(&mut data.image_url, &content)
                }
                _ => {}
            }
        }
    }

    // Fall back to the document <title> when no og:title was present.
    if data.title.is_none() {
        if let Ok(selector) = Selector::parse("title") {
            if let Some(title) = document.select(&selector).next() {
                let text = clean(&title.text().collect::<String>());
                if !text.is_empty() {
                    data.title = Some(text);
                }
            }
        }
    }

    data
}

fn set_if_unset(slot: &mut Option<String>, value: &str) {
    if slot.is_none() {
        *slot = Some(value.to_string());
    }
}

/// Collapses all runs of whitespace (including newlines from multi-line
/// descriptions) into single spaces and trims the ends, so display text stays on
/// one row. Also caps the length (char-safe) as a guard against a pathological
/// `og:description`; the theme owns any finer display truncation.
fn clean(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= MAX_TEXT_CHARS {
        return collapsed;
    }
    collapsed.chars().take(MAX_TEXT_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_http_urls_in_order_and_dedupes() {
        let text = "see https://example.com/a and http://example.com/b \
                    plus https://example.com/a again, not ftp://x or plain example.com";
        assert_eq!(
            extract_urls(text),
            vec![
                "https://example.com/a".to_string(),
                "http://example.com/b".to_string(),
            ]
        );
    }

    #[test]
    fn ignores_non_http_schemes() {
        assert!(extract_urls("file:///etc/passwd mailto:a@b.com").is_empty());
    }

    #[test]
    fn parses_og_meta_tags() {
        let html = r#"
            <html><head>
              <meta property="og:site_name" content="YouTube">
              <meta property="og:title" content="Never Gonna Give You Up">
              <meta property="og:description" content="The official video">
              <meta property="og:image" content="https://i.ytimg.com/vi/x/hqdefault.jpg">
              <title>fallback title</title>
            </head><body></body></html>
        "#;
        let og = parse_og(html);
        assert_eq!(og.title.as_deref(), Some("Never Gonna Give You Up"));
        assert_eq!(og.description.as_deref(), Some("The official video"));
        assert_eq!(og.site_name.as_deref(), Some("YouTube"));
        assert_eq!(
            og.image_url.as_deref(),
            Some("https://i.ytimg.com/vi/x/hqdefault.jpg")
        );
    }

    #[test]
    fn falls_back_to_document_title() {
        let html = "<html><head><title>Just A Page</title></head><body></body></html>";
        let og = parse_og(html);
        assert_eq!(og.title.as_deref(), Some("Just A Page"));
        assert!(og.image_url.is_none());
    }

    /// End-to-end against a real page: exercises the full fetch -> parse ->
    /// image-download path. Ignored by default (needs network); run with
    /// `cargo test -- --ignored fetches_real_youtube_preview`.
    #[tokio::test]
    #[ignore]
    async fn fetches_real_youtube_preview() {
        let dir = std::env::temp_dir().join("tirc-preview-test");
        let _ = std::fs::create_dir_all(&dir);
        let client = build_client();
        let url = "https://www.youtube.com/watch?v=dQw4w9WgXcQ";
        let preview = fetch_preview(&client, &dir, url)
            .await
            .expect("expected a preview");
        assert!(preview.title.is_some(), "title: {:?}", preview.title);
        assert_eq!(preview.site_name.as_deref(), Some("YouTube"));
        let image = preview.image_path.expect("expected a downloaded thumbnail");
        assert!(image.exists(), "thumbnail file missing: {image:?}");
        assert!(std::fs::metadata(&image).unwrap().len() > 0);
    }
}
