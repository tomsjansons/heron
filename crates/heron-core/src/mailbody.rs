//! Turning a fetched message into a [`crate::MessageBody`]: shared by the
//! Proton and Gmail providers.
//!
//! [`from_mime`] parses a whole RFC 822 message; [`from_html`] and
//! [`from_text`] handle a single decoded body (Proton hands those over
//! directly) and leave the headers for the caller to fill in.

use std::panic::{AssertUnwindSafe, catch_unwind};

use anyhow::{Context, Result};
use html2text::render::RichAnnotation;
use mail_parser::{Addr, Address, MessageParser, PartType};

use crate::{Link, MessageBody};

/// Lines are not wrapped (the shell wraps them); this only bounds pathological
/// layouts.
const RENDER_WIDTH: usize = 1000;

/// Parses a full message: headers, the readable text and its links. `id` is
/// left empty for the caller.
pub fn from_mime(raw: &[u8]) -> Result<MessageBody> {
    let message = MessageParser::default().parse(raw).context("not a MIME message")?;

    let plain = join_parts(message.text_bodies().filter_map(|p| match &p.body {
        PartType::Text(text) => Some(text.as_ref()),
        _ => None,
    }));
    let html = join_parts(message.html_bodies().filter_map(|p| match &p.body {
        PartType::Html(html) => Some(html.as_ref()),
        _ => None,
    }));
    let mut body = choose(&plain, &html);

    let first = |address: Option<&Address<'_>>| address.and_then(|a| a.iter().find_map(format_addr));
    let all = |address: Option<&Address<'_>>| -> Vec<String> {
        address.map(|a| a.iter().filter_map(format_addr).collect()).unwrap_or_default()
    };
    body.from = first(message.from()).or_else(|| first(message.sender())).unwrap_or_default();
    body.to = all(message.to());
    body.cc = all(message.cc());
    body.date = message.date().map_or(0, |d| d.to_timestamp());
    body.subject = message.subject().map(clean_spaces).unwrap_or_default();
    Ok(body)
}

/// Text and links of an HTML body; headers left empty.
pub fn from_html(html: &str) -> MessageBody {
    let (text, mut links) = match render_html(html) {
        Some(rendered) => rendered,
        None => (tidy(&mail_parser::decoders::html::html_to_text(html)), Vec::new()),
    };
    for link in bare_links(&text) {
        push_link(&mut links, link);
    }
    MessageBody { text, links, ..Default::default() }
}

/// Text and bare links of a plain-text body; headers left empty.
pub fn from_text(text: &str) -> MessageBody {
    let text = tidy(text);
    let links = bare_links(&text);
    MessageBody { text, links, ..Default::default() }
}

/// The plain part when it says about as much as the HTML one (not a "view
/// this in a browser" stub), else the HTML converted to text.
fn choose(plain: &str, html: &str) -> MessageBody {
    let plain = from_text(plain);
    if html.trim().is_empty() {
        return plain;
    }
    let html = from_html(html);
    let size = |s: &str| s.chars().filter(|c| !c.is_whitespace()).count();
    if plain.text.is_empty() || size(&plain.text) * 3 < size(&html.text) {
        return html;
    }
    // Keep the plain text's links, borrowing anchor text from the HTML.
    let links = if plain.links.is_empty() {
        html.links
    } else {
        plain.links.into_iter()
            .map(|link| html.links.iter().find(|l| l.url == link.url).cloned().unwrap_or(link))
            .collect()
    };
    MessageBody { links, ..plain }
}

fn join_parts<'a>(parts: impl Iterator<Item = &'a str>) -> String {
    parts.filter(|p| !p.trim().is_empty()).collect::<Vec<_>>().join("\n\n")
}

fn format_addr(addr: &Addr<'_>) -> Option<String> {
    format_address(addr.name(), addr.address())
}

/// "Name <address>", or whichever of the two there is.
pub fn format_address(name: Option<&str>, address: Option<&str>) -> Option<String> {
    let name = name.map(|n| n.trim().trim_matches('"').trim()).filter(|n| !n.is_empty());
    let address = address.map(str::trim).filter(|a| !a.is_empty());
    match (name, address) {
        (Some(name), Some(address)) if name != address => Some(format!("{name} <{address}>")),
        (_, Some(address)) => Some(address.to_owned()),
        (Some(name), None) => Some(name.to_owned()),
        (None, None) => None,
    }
}

/// Renders HTML with html2text, collecting `<a href>` targets with their
/// anchor text. `None` if the renderer fails (or panics on odd input).
fn render_html(html: &str) -> Option<(String, Vec<Link>)> {
    let lines = catch_unwind(AssertUnwindSafe(|| {
        html2text::config::rich()
            .use_doc_css()
            .raw_mode(true)
            .allow_width_overflow()
            .no_link_wrapping()
            .lines_from_read(html.as_bytes(), RENDER_WIDTH)
    }))
    .ok()?
    .ok()?;

    let mut text = String::new();
    // (url, anchor text) of the link being read; links can span lines.
    let mut current: Option<(String, String)> = None;
    let mut links = Vec::new();
    for line in &lines {
        for piece in line.tagged_strings() {
            text.push_str(&piece.s);
            let url = piece.tag.iter().rev().find_map(|a| match a {
                RichAnnotation::Link(url) => Some(url.as_str()),
                _ => None,
            });
            match (url, current.as_mut()) {
                (Some(url), Some((open, anchor))) if open == url => anchor.push_str(&piece.s),
                (Some(url), _) => {
                    if let Some((url, anchor)) = current.take() {
                        push_link(&mut links, Link { text: anchor, url });
                    }
                    current = Some((url.to_owned(), piece.s.clone()));
                }
                (None, _) => {
                    if piece.s.trim().is_empty() {
                        continue;
                    }
                    if let Some((url, anchor)) = current.take() {
                        push_link(&mut links, Link { text: anchor, url });
                    }
                }
            }
        }
        text.push('\n');
        if let Some((_, anchor)) = current.as_mut() {
            anchor.push(' ');
        }
    }
    if let Some((url, anchor)) = current {
        push_link(&mut links, Link { text: anchor, url });
    }
    Some((tidy(&text), links))
}

/// Adds a link unless it is not http(s)/mailto or already listed.
fn push_link(links: &mut Vec<Link>, link: Link) {
    let url = link.url.trim();
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    if !(lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("mailto:")) {
        return;
    }
    // A scheme with nothing after it is junk.
    if lower.len() <= "mailto:".len() || url.ends_with("://") || links.iter().any(|l| l.url == url) {
        return;
    }
    let text = clean_spaces(&link.text);
    let text = if text.is_empty() { url.to_owned() } else { text };
    links.push(Link { text, url: url.to_owned() });
}

/// http(s) and mailto URLs written out in text, in order, without duplicates.
/// Each is named by its URL.
fn bare_links(text: &str) -> Vec<Link> {
    let mut links = Vec::new();
    let mut rest = text;
    while let Some(start) = find_scheme(rest) {
        let tail = &rest[start..];
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '`' | '{' | '}' | '|' | '\\' | '^'))
            .unwrap_or(tail.len());
        let url = trim_url(&tail[..end]);
        push_link(&mut links, Link { text: String::new(), url: url.to_owned() });
        rest = &tail[end.max(1)..];
    }
    links
}

/// Drops sentence punctuation and unbalanced closing brackets from the end.
fn trim_url(mut url: &str) -> &str {
    loop {
        let mut trimmed = url.trim_end_matches(['.', ',', ';', ':', '!', '?', '\'', '*']);
        if let Some(close @ (')' | ']')) = trimmed.chars().last() {
            let open = if close == ')' { '(' } else { '[' };
            if trimmed.matches(open).count() < trimmed.matches(close).count() {
                trimmed = &trimmed[..trimmed.len() - 1];
            }
        }
        if trimmed.len() == url.len() {
            return url;
        }
        url = trimmed;
    }
}

/// Byte offset of the next "http://", "https://" or "mailto:" that starts a
/// word, case-insensitively.
fn find_scheme(text: &str) -> Option<usize> {
    let lower = text.to_ascii_lowercase();
    let mut from = 0;
    loop {
        let found = ["https://", "http://", "mailto:"].iter()
            .filter_map(|s| lower[from..].find(s).map(|i| from + i))
            .min()?;
        if lower[..found].chars().next_back().is_none_or(|c| !c.is_alphanumeric()) {
            return Some(found);
        }
        from = found + 1;
    }
}

fn clean_spaces(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Readable text: no invisible spacer characters, no trailing spaces, at most
/// one blank line in a row, long separator lines shortened.
fn tidy(text: &str) -> String {
    let text: String = text
        .chars()
        .filter(|c| {
            !matches!(c, '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' | '\u{FEFF}' | '\u{034F}' | '\u{00AD}' | '\u{180E}' | '\r')
        })
        .map(|c| if matches!(c, '\u{00A0}' | '\u{2007}' | '\u{202F}' | '\t') { ' ' } else { c })
        .collect();
    let mut out = String::with_capacity(text.len());
    let mut blank = false;
    for line in text.lines() {
        let line = line.trim_end();
        let content = line.trim_start_matches(['>', ' ']);
        if content.is_empty() {
            blank = true;
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank { "\n\n" } else { "\n" });
        }
        blank = false;
        if content.chars().count() > 3 && content.chars().all(|c| matches!(c, '─' | '━' | '═' | '-' | '_' | '=' | '*')) {
            out.push_str(&line[..line.len() - content.len()]);
            out.push_str("---");
        } else {
            out.push_str(line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(body: &MessageBody) -> Vec<&str> {
        body.links.iter().map(|l| l.url.as_str()).collect()
    }

    const ALTERNATIVE: &str = "From: =?UTF-8?Q?J=C4=81nis_B=C4=93rzi=C5=86=C5=A1?= <janis@example.lv>\r
To: \"Toms\" <toms@example.com>, anna@example.com\r
Cc: Team <team@example.com>\r
Subject: Re: =?utf-8?q?R=C4=ABga?= meetup\r
 next week\r
Date: Tue, 14 Nov 2023 22:13:20 +0000\r
Message-ID: <abc@example.lv>\r
MIME-Version: 1.0\r
Content-Type: multipart/alternative; boundary=\"b1\"\r
\r
--b1\r
Content-Type: text/plain; charset=utf-8\r
Content-Transfer-Encoding: quoted-printable\r
\r
Sveiki!\r
\r
The agenda is at https://example.lv/agenda?id=3D7. See you there=\r
 (bring a laptop).\r
\r
\r
\r
> On Mon, Toms wrote:\r
> Sounds good, details: <https://example.com/details>\r
--b1\r
Content-Type: text/html; charset=utf-8\r
Content-Transfer-Encoding: quoted-printable\r
\r
<html><body><p>Sveiki!</p><p>The <a href=3D\"https://example.lv/agenda?id=3D7\">agenda</a> is up. See you there (bring a laptop).</p>\r
<blockquote>Sounds good, <a href=3D\"https://example.com/details\">details</a></blockquote></body></html>\r
--b1--\r
";

    #[test]
    fn multipart_alternative_prefers_plain() {
        let body = from_mime(ALTERNATIVE.as_bytes()).unwrap();
        assert_eq!(body.from, "Jānis Bērziņš <janis@example.lv>");
        assert_eq!(body.to, ["Toms <toms@example.com>", "anna@example.com"]);
        assert_eq!(body.cc, ["Team <team@example.com>"]);
        assert_eq!(body.subject, "Re: Rīga meetup next week");
        assert_eq!(body.date, 1_700_000_000);
        assert_eq!(
            body.text,
            "Sveiki!\n\nThe agenda is at https://example.lv/agenda?id=7. See you there (bring a laptop).\n\n\
             > On Mon, Toms wrote:\n> Sounds good, details: <https://example.com/details>"
        );
        assert_eq!(body.links, [
            Link { text: "agenda".into(), url: "https://example.lv/agenda?id=7".into() },
            Link { text: "details".into(), url: "https://example.com/details".into() },
        ]);
    }

    const NEWSLETTER_HEAD: &str = "From: Shop News <news@shop.example>\r
To: toms@example.com\r
Subject: =?UTF-8?B?8J+OiSBBdXR1bW4gc2FsZQ==?=\r
Date: Wed, 15 Nov 2023 08:00:00 +0200\r
MIME-Version: 1.0\r
Content-Type: text/html; charset=UTF-8\r
Content-Transfer-Encoding: base64\r
\r
";

    const NEWSLETTER_HTML: &str = r##"<!DOCTYPE html><html><head>
<style>.hide{display:none} body{font-family:Arial}</style>
<script>trackOpen()</script>
<title>Autumn sale</title></head>
<body>
<div class="hide">Preheader text you should not see&zwnj;&nbsp;&zwnj;&nbsp;&zwnj;&nbsp;</div>
<table width="100%"><tr><td><a href="https://shop.example/?utm=logo"><img src="https://shop.example/logo.png" alt=""></a></td></tr>
<tr><td><h1>Autumn sale</h1></td></tr>
<tr><td><p>Up&nbsp;to <b>50%</b> off   everything.</p>



<p>&nbsp;</p><p><br></p><p>&nbsp;</p>
<a href="https://shop.example/sale?utm=btn" style="color:#fff">Shop
  now</a></td></tr>
<tr><td style="font-size:10px">Questions? Write to <a href="mailto:help@shop.example">help@shop.example</a>
or visit https://shop.example/help.
<a href="https://shop.example/unsubscribe?u=1">Unsubscribe</a> | <a href="javascript:void(0)">x</a> | <a href="#top">Top</a>
<a href="https://shop.example/sale?utm=btn">again</a></td></tr></table>
<img src="https://track.example/open.gif" width="1" height="1" alt="">
</body></html>"##;

    #[test]
    fn html_only_newsletter() {
        let mut raw = NEWSLETTER_HEAD.to_owned();
        let encoded = base64_encode(NEWSLETTER_HTML.as_bytes());
        for chunk in encoded.as_bytes().chunks(76) {
            raw.push_str(std::str::from_utf8(chunk).unwrap());
            raw.push_str("\r\n");
        }
        let body = from_mime(raw.as_bytes()).unwrap();
        assert_eq!(body.subject, "🎉 Autumn sale");
        assert_eq!(body.from, "Shop News <news@shop.example>");
        assert_eq!(body.date, 1_700_028_000);
        assert!(!body.text.contains("Preheader"), "{}", body.text);
        assert!(!body.text.contains("trackOpen"), "{}", body.text);
        assert!(!body.text.contains("font-family"), "{}", body.text);
        assert!(body.text.contains("Autumn sale"), "{}", body.text);
        assert!(body.text.contains("Up to 50% off everything."), "{}", body.text);
        assert!(body.text.contains("Shop now"), "{}", body.text);
        assert!(!body.text.contains("\n\n\n"), "{:?}", body.text);
        assert!(!body.text.starts_with('\n') && !body.text.ends_with('\n'));
        assert_eq!(urls(&body), [
            "https://shop.example/sale?utm=btn",
            "mailto:help@shop.example",
            "https://shop.example/unsubscribe?u=1",
            "https://shop.example/help",
        ]);
        assert_eq!(body.links[0].text, "Shop now");
        assert_eq!(body.links[1].text, "help@shop.example");
    }

    #[test]
    fn plain_stub_falls_back_to_html() {
        let raw = "From: a@example.com\r\nSubject: x\r\nContent-Type: multipart/alternative; boundary=zz\r\n\r\n\
            --zz\r\nContent-Type: text/plain\r\n\r\nView this email in your browser.\r\n\
            --zz\r\nContent-Type: text/html\r\n\r\n<p>This is the real and much longer content of the message, with a \
            <a href=\"https://example.com/a\">link</a> and plenty of words to make it substantive.</p>\r\n--zz--\r\n";
        let body = from_mime(raw.as_bytes()).unwrap();
        assert!(body.text.starts_with("This is the real"), "{}", body.text);
        assert_eq!(urls(&body), ["https://example.com/a"]);
        assert_eq!(body.date, 0);
    }

    #[test]
    fn non_utf8_charset() {
        let raw = b"From: =?ISO-8859-1?Q?Andr=E9?= <andre@example.fr>\r\nSubject: =?windows-1252?Q?Caf=E9_=96_menu?=\r\n\
            Content-Type: text/plain; charset=iso-8859-1\r\nContent-Transfer-Encoding: 8bit\r\n\r\nLe caf\xe9 est ouvert \xe0 8h.\r\n";
        let body = from_mime(raw).unwrap();
        assert_eq!(body.from, "André <andre@example.fr>");
        assert_eq!(body.subject, "Café – menu");
        assert_eq!(body.text, "Le café est ouvert à 8h.");

        let raw = b"From: x@example.ru\r\nContent-Type: text/html; charset=koi8-r\r\n\r\n<p>\xf0\xd2\xc9\xd7\xc5\xd4</p>";
        assert_eq!(from_mime(raw).unwrap().text, "Привет");
    }

    #[test]
    fn mixed_with_attachment_and_nested_alternative() {
        let raw = "From: a@example.com\r\nContent-Type: multipart/mixed; boundary=outer\r\n\r\n\
            --outer\r\nContent-Type: multipart/alternative; boundary=inner\r\n\r\n\
            --inner\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nInvoice attached. Pay at https://pay.example/i/1.\r\n\
            --inner\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Invoice attached. <a href=\"https://pay.example/i/1\">Pay</a>.</p>\r\n\
            --inner--\r\n\
            --outer\r\nContent-Type: application/pdf; name=a.pdf\r\nContent-Disposition: attachment; filename=a.pdf\r\n\
            Content-Transfer-Encoding: base64\r\n\r\nJVBERi0xLjQK\r\n--outer--\r\n";
        let body = from_mime(raw.as_bytes()).unwrap();
        assert_eq!(body.text, "Invoice attached. Pay at https://pay.example/i/1.");
        assert_eq!(body.links, [Link { text: "Pay".into(), url: "https://pay.example/i/1".into() }]);
    }

    #[test]
    fn bare_link_edges() {
        let body = from_text(
            "See (https://a.example/x_(y)) and https://b.example/p).\nMail mailto:me@example.com, or\n\
             HTTPS://c.example/Q?a=1&b=2! notahttp://x.example ftp://no.example https://a.example/x_(y)",
        );
        assert_eq!(urls(&body), [
            "https://a.example/x_(y)",
            "https://b.example/p",
            "mailto:me@example.com",
            "HTTPS://c.example/Q?a=1&b=2",
        ]);
        assert_eq!(body.links[0].text, "https://a.example/x_(y)");
    }

    #[test]
    fn tidy_collapses() {
        assert_eq!(tidy("\n\n a \u{200c}\u{a0}\r\n\n\n\n>\n> b  \n────────────\n\n"), " a\n\n> b\n---");
    }

    #[test]
    fn garbage_is_not_a_message() {
        assert!(from_mime(b"").is_err());
    }

    fn base64_encode(data: &[u8]) -> String {
        const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(ALPHABET[(n >> (18 - 6 * i)) as usize & 63] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }
}
