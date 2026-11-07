//! Fetch a public status and reduce it to what a preview needs.
//!
//! The instance is the only source: we do not store other people's post bodies for
//! a whole retention window just to be able to unfurl a link. The request is
//! unauthenticated, so the instance applies its own visibility rules and we cannot
//! accidentally preview something the reader is not allowed to see.

use std::time::Duration;

use serde::Deserialize;

/// The longest description a preview carries.
///
/// Every platform truncates somewhere and each one differently, so the limit here
/// is the smallest that still reads like a sentence — 240 characters fits a chat
/// client's two-line preview without being cut mid-word by us.
const DESCRIPTION_LIMIT: usize = 240;

/// One status, ready to be rendered as a page and cached.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Unfurl {
    /// `Display name (@user@host)`, or the acct alone when the display name is empty.
    pub title: String,
    /// Post text with markup removed, trimmed to [`DESCRIPTION_LIMIT`].
    pub description: String,
    /// Preview image: the first attachment, else the author's avatar.
    pub image: Option<String>,
    /// The post on its own instance — the canonical destination, and what a reader
    /// who is already on that instance should open.
    pub canonical: String,
    /// `@user@host`, for the byline.
    pub author_acct: String,
    /// Publish time as the instance reported it. Not parsed: it is displayed, not
    /// compared, and re-formatting someone else's ISO timestamp is a way to be wrong.
    pub published_at: String,
}

/// Why a status could not be unfurled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnfurlError {
    /// The instance does not know the post, or will not show it to an anonymous
    /// reader. Deliberately one variant: distinguishing them would tell a stranger
    /// whether a private post exists.
    NotFound,
    /// The instance answered, but with something this code cannot read.
    Decode,
    /// The instance did not answer in time or refused the connection.
    Unreachable,
}

impl UnfurlError {
    /// The wire name, for logs and the cache.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::Decode => "decode",
            Self::Unreachable => "unreachable",
        }
    }

    /// The inverse of [`Self::reason`]. An unknown name is treated as a miss that
    /// should be retried, not as a permanent answer.
    #[must_use]
    pub fn from_reason(reason: &str) -> Self {
        match reason {
            "decode" => Self::Decode,
            "unreachable" => Self::Unreachable,
            _ => Self::NotFound,
        }
    }
}

/// Fetch `https://<host>/api/v1/statuses/<id>` and reduce it.
///
/// # Errors
///
/// Returns [`UnfurlError`] for a missing, non-public, unreadable or unreachable
/// status.
pub async fn fetch(
    http: &reqwest::Client,
    host: &str,
    status_id: &str,
) -> Result<Unfurl, UnfurlError> {
    let url = format!("https://{host}/api/v1/statuses/{status_id}");
    let response = http
        .get(&url)
        .timeout(Duration::from_secs(5))
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|_| UnfurlError::Unreachable)?;

    let status = response.status();
    if !status.is_success() {
        // 401/403/404 all mean "not for you"; 5xx means "ask again later".
        return Err(if status.is_server_error() {
            UnfurlError::Unreachable
        } else {
            UnfurlError::NotFound
        });
    }

    let wire: Wire = response.json().await.map_err(|_| UnfurlError::Decode)?;
    reduce(&wire, host, status_id)
}

/// Narrow a parsed status into an [`Unfurl`].
///
/// Separated from the request so the decisions below are testable without a server.
fn reduce(wire: &Wire, host: &str, status_id: &str) -> Result<Unfurl, UnfurlError> {
    // An unauthenticated GET should only ever return public or unlisted posts, but
    // the value is checked rather than trusted: a misconfigured instance must not be
    // able to make this page publish a followers-only post.
    if !matches!(wire.visibility.as_str(), "public" | "unlisted") {
        return Err(UnfurlError::NotFound);
    }

    let account = &wire.account;
    let display = account.display_name.trim();
    let title = if display.is_empty() {
        account.acct.clone()
    } else {
        format!("{display} (@{})", account.acct)
    };

    let canonical = wire
        .url
        .clone()
        .filter(|url| url.starts_with("https://"))
        .unwrap_or_else(|| format!("https://{host}/@{}/{status_id}", account.acct));

    let image = wire
        .media_attachments
        .first()
        .map(|media| media.preview_url.as_str())
        .or(Some(account.avatar_static.as_str()))
        .filter(|url| url.starts_with("https://"))
        .map(ToOwned::to_owned);

    Ok(Unfurl {
        title,
        description: plain_text(&wire.content, DESCRIPTION_LIMIT),
        image,
        canonical,
        author_acct: account.acct.clone(),
        published_at: wire.created_at.clone(),
    })
}

/// Reduce instance HTML to a single line of text.
///
/// The instance's `content` is HTML it has already sanitized, but it is HTML we do
/// not want in a preview: tags would show up as literal text in every client that
/// escapes them, and links would eat the character budget. Removing tags here is
/// not a safety measure — [`super::render`] escapes everything it emits — it is a
/// readability measure, which is why it is allowed to be approximate.
fn plain_text(html: &str, limit: usize) -> String {
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut pending_space = false;

    for ch in html.chars() {
        match ch {
            '<' => {
                in_tag = true;
                // A tag is a word boundary: "<p>one</p><p>two</p>" must not become
                // "onetwo".
                pending_space = true;
            }
            '>' if in_tag => in_tag = false,
            _ if in_tag => {}
            _ => {
                if ch.is_whitespace() {
                    pending_space = true;
                } else {
                    if pending_space && !text.is_empty() {
                        text.push(' ');
                    }
                    pending_space = false;
                    text.push(ch);
                }
            }
        }
    }

    let decoded = decode_entities(&text);
    truncate(&decoded, limit)
}

/// Undo the entity references Mastodon emits in post bodies.
///
/// A deliberately short list: these are the ones that appear in ordinary text and
/// that would otherwise be visible as `&amp;`. Entity decoding is not a sanitizer,
/// and treating it as one is how double-decoding bugs start — the output is escaped
/// again at render time regardless.
fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find('&') {
        out.push_str(&rest[..index]);
        let tail = &rest[index..];
        let (decoded, consumed) = if tail.starts_with("&amp;") {
            ("&", 5)
        } else if tail.starts_with("&lt;") {
            ("<", 4)
        } else if tail.starts_with("&gt;") {
            (">", 4)
        } else if tail.starts_with("&quot;") {
            ("\"", 6)
        } else if tail.starts_with("&#39;") || tail.starts_with("&apos;") {
            ("'", 6)
        } else if tail.starts_with("&nbsp;") {
            (" ", 6)
        } else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        out.push_str(decoded);
        rest = &tail[consumed..];
    }
    out.push_str(rest);
    out
}

/// Cut at a character boundary, appending an ellipsis only when something was cut.
fn truncate(text: &str, limit: usize) -> String {
    // `char_indices` gives byte offsets, so the slice below cannot split a character
    // even though the limit counts characters.
    let Some((index, _)) = text.char_indices().nth(limit) else {
        return text.to_owned();
    };
    let mut out = String::with_capacity(index + '…'.len_utf8());
    out.push_str(&text[..index]);
    out.push('…');
    out
}

/// The subset of the status payload a preview needs.
///
/// Unknown fields are ignored, which is what keeps this working when the instance is
/// on a newer version than us.
#[derive(Debug, Deserialize)]
struct Wire {
    content: String,
    created_at: String,
    visibility: String,
    #[serde(default)]
    url: Option<String>,
    account: WireAccount,
    #[serde(default)]
    media_attachments: Vec<WireMedia>,
}

#[derive(Debug, Deserialize)]
struct WireAccount {
    acct: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    avatar_static: String,
}

#[derive(Debug, Deserialize)]
struct WireMedia {
    preview_url: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(visibility: &str, content: &str) -> Wire {
        Wire {
            content: content.to_owned(),
            created_at: "2026-11-07T00:00:00.000Z".to_owned(),
            visibility: visibility.to_owned(),
            url: Some("https://mastodon.social/@ada/1".to_owned()),
            account: WireAccount {
                acct: "ada@mastodon.social".to_owned(),
                display_name: "Ada".to_owned(),
                avatar_static: "https://files.example/ada.png".to_owned(),
            },
            media_attachments: Vec::new(),
        }
    }

    #[test]
    fn a_followers_only_post_is_never_previewed() {
        let result = reduce(&wire("private", "<p>secret</p>"), "mastodon.social", "1");
        assert_eq!(result, Err(UnfurlError::NotFound));
    }

    #[test]
    fn markup_becomes_readable_text() {
        assert_eq!(
            plain_text("<p>one</p><p>two &amp; three</p>", 240),
            "one two & three"
        );
        assert_eq!(plain_text("  spaced\n\tout  ", 240), "spaced out");
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        let text = "é".repeat(10);
        let cut = truncate(&text, 3);
        assert_eq!(cut, "ééé…");
        assert_eq!(truncate("abc", 3), "abc");
    }

    #[test]
    fn a_missing_display_name_falls_back_to_the_acct() {
        let mut post = wire("public", "<p>hi</p>");
        post.account.display_name = "   ".to_owned();
        let unfurl = reduce(&post, "mastodon.social", "1").expect("reduces");
        assert_eq!(unfurl.title, "ada@mastodon.social");
    }

    #[test]
    fn the_avatar_stands_in_when_a_post_has_no_media() {
        let mut post = wire("public", "<p>hi</p>");
        post.account.avatar_static = "http://insecure.example/ada.png".to_owned();
        let unfurl = reduce(&post, "mastodon.social", "1").expect("reduces");
        assert_eq!(unfurl.image, None, "an http image would be blocked anyway");

        post.account.avatar_static = "https://files.example/ada.png".to_owned();
        let unfurl = reduce(&post, "mastodon.social", "1").expect("reduces");
        assert_eq!(
            unfurl.image.as_deref(),
            Some("https://files.example/ada.png")
        );
    }

    #[test]
    fn a_post_without_a_canonical_url_gets_one_built() {
        let mut post = wire("public", "<p>hi</p>");
        post.url = None;
        let unfurl = reduce(&post, "mastodon.social", "42").expect("reduces");
        assert_eq!(
            unfurl.canonical,
            "https://mastodon.social/@ada@mastodon.social/42"
        );
    }
}
