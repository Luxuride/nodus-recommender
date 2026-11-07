//! The shared-post page: meta tags for crawlers, readable HTML for people.
//!
//! Everything here is escaped on the way out, without exception. The title, the
//! description and the image come from someone else's server, and this page is
//! served from our origin with no authentication — a single unescaped field would be
//! stored cross-site scripting on a domain that also hosts the API.
//!
//! There is no script on the page at all, which is what makes the strict CSP on the
//! app unnecessary here and keeps a preview cheap to render and safe to cache.

use super::Unfurl;

/// Render a found post.
#[must_use]
pub fn page(unfurl: &Unfurl, share_url: &str, app_url: &str) -> String {
    let title = escape(&unfurl.title);
    let description = escape(&unfurl.description);
    let canonical = escape(&unfurl.canonical);
    let author = escape(&unfurl.author_acct);
    let published = escape(&unfurl.published_at);
    let share_url = escape(share_url);
    let app_url = escape(app_url);

    // `summary_large_image` only makes sense with an image; without one, X and
    // friends render a big empty card.
    let card = if unfurl.image.is_some() {
        "summary_large_image"
    } else {
        "summary"
    };
    let image_tags = unfurl
        .image
        .as_deref()
        .map(|url| {
            let url = escape(url);
            format!(
                "    <meta property=\"og:image\" content=\"{url}\">\n\
                 \x20   <meta name=\"twitter:image\" content=\"{url}\">\n"
            )
        })
        .unwrap_or_default();
    let image = unfurl.image.as_deref().map_or_else(String::new, |url| {
        format!(
            "      <img class=\"media\" src=\"{}\" alt=\"\" referrerpolicy=\"no-referrer\">\n",
            escape(url)
        )
    });

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>{title}</title>
    <link rel="canonical" href="{canonical}">
    <meta name="description" content="{description}">
    <meta property="og:type" content="article">
    <meta property="og:site_name" content="Nodus">
    <meta property="og:title" content="{title}">
    <meta property="og:description" content="{description}">
    <meta property="og:url" content="{share_url}">
    <meta property="article:published_time" content="{published}">
    <meta property="article:author" content="{author}">
{image_tags}    <meta name="twitter:card" content="{card}">
    <meta name="twitter:title" content="{title}">
    <meta name="twitter:description" content="{description}">
    <style>
      :root {{ color-scheme: light dark; }}
      body {{ font: 16px/1.5 system-ui, sans-serif; margin: 0; padding: 2rem 1rem;
              display: flex; justify-content: center; }}
      main {{ max-width: 40rem; width: 100%; }}
      h1 {{ font-size: 1.05rem; margin: 0 0 0.25rem; }}
      .byline {{ opacity: 0.65; font-size: 0.85rem; margin: 0 0 1rem; }}
      .body {{ white-space: pre-wrap; margin: 0 0 1rem; }}
      .media {{ max-width: 100%; border-radius: 0.5rem; margin: 0 0 1rem; }}
      .links {{ display: flex; flex-wrap: wrap; gap: 1rem; font-size: 0.9rem; }}
      footer {{ margin-top: 2rem; font-size: 0.75rem; opacity: 0.55; }}
    </style>
  </head>
  <body>
    <main>
      <article>
        <h1>{title}</h1>
        <p class="byline"><time datetime="{published}">{published}</time></p>
        <p class="body">{description}</p>
{image}      </article>
      <nav class="links">
        <a href="{canonical}" rel="noopener noreferrer" target="_blank">Open on the original instance</a>
        <a href="{app_url}">Open in Nodus</a>
      </nav>
      <footer>
        Nodus is not affiliated with, endorsed by, or sponsored by Mastodon GmbH.
        &ldquo;Mastodon&rdquo; is a trademark of Mastodon GmbH and is used here only to
        describe compatibility.
      </footer>
    </main>
  </body>
</html>
"#
    )
}

/// Render a page for a post that cannot be shown.
///
/// Deliberately says nothing about whether the post exists: a link to a
/// followers-only post and a link to a deleted one must look identical to a stranger.
#[must_use]
pub fn unavailable(app_url: &str) -> String {
    let app_url = escape(app_url);
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <meta name="robots" content="noindex">
    <meta property="og:type" content="website">
    <meta property="og:site_name" content="Nodus">
    <meta property="og:title" content="Post unavailable">
    <meta property="og:description" content="This post is not publicly available.">
    <title>Post unavailable</title>
    <style>
      :root {{ color-scheme: light dark; }}
      body {{ font: 16px/1.5 system-ui, sans-serif; margin: 0; padding: 3rem 1rem;
              display: flex; justify-content: center; }}
      main {{ max-width: 32rem; }}
    </style>
  </head>
  <body>
    <main>
      <h1>Post unavailable</h1>
      <p>This post is not publicly available. It may be followers-only, deleted, or
         on an instance that is not reachable right now.</p>
      <p><a href="{app_url}">Open Nodus</a></p>
    </main>
  </body>
</html>
"#
    )
}

/// HTML-escape the five characters that matter outside an attribute value or in
/// text, plus both quote characters so the same function is safe inside `content="…"`.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Unfurl {
        Unfurl {
            title: "Ada (@ada@example.social)".to_owned(),
            description: "hello world".to_owned(),
            image: Some("https://files.example/a.png".to_owned()),
            canonical: "https://example.social/@ada/1".to_owned(),
            author_acct: "ada@example.social".to_owned(),
            published_at: "2026-11-07T00:00:00.000Z".to_owned(),
        }
    }

    #[test]
    fn a_hostile_display_name_cannot_break_out_of_the_page() {
        let mut post = sample();
        post.title = "</title><script>alert(1)</script>".to_owned();
        let html = page(&post, "https://nodus.example/s/example.social/1", "/");
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn quotes_in_an_image_url_stay_inside_the_attribute() {
        let mut post = sample();
        post.image = Some("https://files.example/a.png\" onload=\"alert(1)".to_owned());
        let html = page(&post, "https://nodus.example/s/example.social/1", "/");
        assert!(!html.contains("onload=\"alert"));
        assert!(html.contains("&quot; onload=&quot;alert(1)"));
    }

    #[test]
    fn a_post_without_an_image_uses_a_small_card() {
        let mut post = sample();
        post.image = None;
        let html = page(&post, "https://nodus.example/s/example.social/1", "/");
        assert!(html.contains("name=\"twitter:card\" content=\"summary\""));
        assert!(!html.contains("og:image"));
    }

    #[test]
    fn the_unavailable_page_does_not_reveal_why() {
        let html = unavailable("/");
        for word in ["private", "deleted", "blocked", "followers-only"] {
            assert!(
                !html.contains(word) || html.contains("may be"),
                "the page must not assert {word}"
            );
        }
        assert!(html.contains("noindex"));
    }
}
