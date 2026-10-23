//! Finding the video call link of a calendar event: shared by the Proton and
//! Google calendar providers.
//!
//! Self-contained on purpose: a few string scans are enough to pull links out
//! of locations and (often HTML) descriptions, and to turn those descriptions
//! into the plain text [`crate::EventDetails`] carries.

/// The first video call link in `texts`, searched in order (pass the
/// location before the description). Understands links wrapped in `<...>`,
/// quoted in HTML attributes, HTML-escaped (`&amp;`), followed by
/// punctuation, written without a scheme, or hidden behind Google and
/// Outlook redirect wrappers.
pub fn find_meeting_url(texts: &[&str]) -> Option<String> {
    texts.iter().find_map(|text| candidates(&decode_entities(text)).find_map(|url| meeting_link(&url)))
}

/// `url` with redirect wrappers removed, when it joins a video call.
pub fn meeting_url(url: &str) -> Option<String> {
    meeting_link(url.trim())
}

/// Plain text from an event description that may be HTML (Google writes
/// descriptions as HTML; iCalendar ones are plain). Block elements become
/// line breaks, links whose text differs from their target keep the target
/// in parentheses, entities are decoded, and blank runs are collapsed.
/// Plain text passes through with only whitespace tidied.
pub fn html_to_text(input: &str) -> String {
    if !looks_like_html(input) {
        return tidy(input);
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    // Target of the <a> being read, and where its text starts in `out`.
    let mut link: Option<(String, usize)> = None;
    while let Some(lt) = rest.find('<') {
        out.push_str(&decode_entities(&rest[..lt]).replace(['\n', '\r'], " "));
        let after = &rest[lt + 1..];
        let Some(gt) = after.find('>') else {
            out.push_str(&decode_entities(&rest[lt..]));
            rest = "";
            break;
        };
        let tag = &after[..gt];
        rest = &after[gt + 1..];
        let closing = tag.starts_with('/');
        let name = tag.trim_start_matches('/').chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        match name.as_str() {
            "br" | "p" | "div" | "ul" | "ol" | "table" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
            | "blockquote" | "pre" | "hr" => out.push('\n'),
            "li" if !closing => out.push_str("\n- "),
            "td" | "th" if closing => out.push(' '),
            "script" | "style" if !closing => {
                // Their content is not text.
                let end = format!("</{name}");
                rest = rest.to_ascii_lowercase().find(&end).map_or("", |at| &rest[at..]);
            }
            "a" if !closing => {
                link = attribute(tag, "href").map(|href| (unwrap_redirect(&decode_entities(&href)), out.len()));
            }
            "a" => {
                if let Some((href, start)) = link.take() {
                    let text = out[start..].trim();
                    let same = text.trim_end_matches('/') == href.trim_end_matches('/')
                        || href.strip_prefix("mailto:") == Some(text)
                        || unwrap_redirect(text) == href;
                    if !same && (href.starts_with("http") || href.starts_with("mailto:")) {
                        if text.is_empty() {
                            out.push_str(&href);
                        } else {
                            out.push_str(&format!(" ({href})"));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out.push_str(&decode_entities(rest).replace(['\n', '\r'], " "));
    tidy(&out)
}

/// At most `max` characters, cut at a character boundary with "…" appended.
pub fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", text[..at].trim_end()),
        None => text.to_owned(),
    }
}

fn looks_like_html(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "<br", "<p>", "<p ", "</p>", "<a ", "</a>", "<div", "<span", "<b>", "<i>", "<u>", "<ul", "<ol", "<li",
        "<strong", "<em>", "<html", "<table",
    ]
    .iter()
    .any(|tag| lower.contains(tag))
}

/// Trims lines, collapses runs of spaces and of blank lines.
fn tidy(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank = false;
    for line in text.replace('\u{a0}', " ").lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            blank = true;
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank { "\n\n" } else { "\n" });
        }
        blank = false;
        out.push_str(&line);
    }
    out
}

/// The value of `name="..."` (or single-quoted, or bare) inside a tag.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(found) = lower[from..].find(name) {
        let at = from + found;
        from = at + name.len();
        let preceded = at == 0 || lower.as_bytes()[at - 1].is_ascii_whitespace();
        let Some(rest) = tag[from..].trim_start().strip_prefix('=').filter(|_| preceded) else { continue };
        let rest = rest.trim_start();
        let value = match rest.chars().next() {
            Some(q @ ('"' | '\'')) => rest[1..].split(q).next().unwrap_or_default(),
            _ => rest.split(|c: char| c.is_whitespace() || c == '>').next().unwrap_or_default(),
        };
        return Some(value.to_owned());
    }
    None
}

/// Decodes the HTML entities descriptions use; unknown ones are left alone.
fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let decoded = rest[1..].find(';').filter(|&semi| semi <= 10).and_then(|semi| {
            let c = match &rest[1..1 + semi] {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                entity => {
                    let code = match entity.strip_prefix("#x").or_else(|| entity.strip_prefix("#X")) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok(),
                        None => entity.strip_prefix('#').and_then(|dec| dec.parse().ok()),
                    };
                    code.and_then(char::from_u32)
                }
            };
            c.map(|c| (c, semi + 2))
        });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Every URL-looking token, in order, with a scheme added where missing.
fn candidates(text: &str) -> impl Iterator<Item = String> + '_ {
    let is_break = |c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '`' | '|');
    text.split(is_break).filter_map(|token| {
        let token = match token.find("https://").or_else(|| token.find("http://")) {
            Some(at) => token[at..].to_owned(),
            None => {
                // "meet.google.com/abc-defg-hij" without a scheme.
                let token = token.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
                let host = token.split('/').next()?;
                if !token.contains('/') || !host.contains('.') || host.contains('@') {
                    return None;
                }
                format!("https://{token}")
            }
        };
        Some(token.trim_end_matches(|c: char| ".,;:!?)]}*…".contains(c)).to_owned())
    })
}

struct Parts<'a> {
    host: String,
    path: &'a str,
    query: &'a str,
}

/// Splits `scheme://host[:port]/path?query#fragment`; only http(s).
fn split(url: &str) -> Option<Parts<'_>> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = rest[..end].rsplit('@').next()?.split(':').next()?.to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    let rest = rest[end..].split('#').next().unwrap_or_default();
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    Some(Parts { host, path, query })
}

fn query_param(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then(|| percent_decode(value))
    })
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes.get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
                continue;
            }
            (b'+', _) => out.push(b' '),
            (b, _) => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The target of Google (`google.com/url?q=`) and Outlook safelinks
/// redirects, repeatedly; anything else unchanged.
fn unwrap_redirect(url: &str) -> String {
    let mut url = url.to_owned();
    for _ in 0..3 {
        let Some(parts) = split(&url) else { break };
        let host = parts.host.trim_start_matches("www.");
        let target = if (host == "google.com" || host.starts_with("google.")) && parts.path == "/url" {
            query_param(parts.query, "q").or_else(|| query_param(parts.query, "url"))
        } else if host.ends_with("safelinks.protection.outlook.com") {
            query_param(parts.query, "url")
        } else {
            None
        };
        match target {
            Some(target) if target.starts_with("http") => url = target,
            _ => break,
        }
    }
    url
}

/// `url` (after unwrapping redirects) when it joins a video call.
fn meeting_link(url: &str) -> Option<String> {
    let url = unwrap_redirect(url);
    let parts = split(&url)?;
    let host = parts.host.strip_prefix("www.").unwrap_or(&parts.host);
    let path = parts.path;
    let first = path.trim_start_matches('/').split('/').next().unwrap_or_default();
    let under = |domain: &str| host == domain || host.ends_with(&format!(".{domain}"));
    let starts = |prefixes: &[&str]| prefixes.iter().any(|p| path.starts_with(p));

    let ok = match host {
        "meet.google.com" => !first.is_empty() && !["_meet", "landing", "new"].contains(&first),
        "teams.microsoft.com" | "teams.live.com" | "teams.cloud.microsoft" => {
            starts(&["/l/meetup-join/", "/meet/", "/v2/meetup-join/"])
        }
        "app.slack.com" => starts(&["/huddle/"]),
        "meet.jit.si" | "8x8.vc" | "gotomeet.me" | "meet.goto.com" | "join.skype.com" | "chime.aws" | "discord.gg" => {
            !first.is_empty()
        }
        "discord.com" | "discordapp.com" => starts(&["/invite/", "/channels/", "/events/"]),
        _ if under("zoom.us") || under("zoom.com") || under("zoomgov.com") => {
            starts(&["/j/", "/my/", "/w/", "/s/", "/wc/join/"])
        }
        _ if under("webex.com") => starts(&["/meet/", "/join/", "/wbxmjs/joinservice/"]) || path.contains("/j.php"),
        _ if under("whereby.com") => {
            !first.is_empty() && !["information", "user", "org", "login", "signup", "pricing"].contains(&first)
        }
        _ if under("gotomeeting.com") => starts(&["/join/", "/meeting/"]),
        _ if host.starts_with("meet.proton.") => starts(&["/join/"]),
        _ => false,
    };
    ok.then_some(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(text: &str) -> Option<String> {
        find_meeting_url(&[text])
    }

    #[test]
    fn finds_common_providers() {
        let teams = "https://teams.microsoft.com/l/meetup-join/19%3ameeting_ABC%40thread.v2/0?context=%7b%22Tid%22%7d";
        let teams_text = format!("Microsoft Teams meeting {teams}");
        let cases = [
            ("Join: https://meet.google.com/abc-defg-hij.", "https://meet.google.com/abc-defg-hij"),
            ("meet.google.com/abc-defg-hij", "https://meet.google.com/abc-defg-hij"),
            ("https://us02web.zoom.us/j/81234567890?pwd=abcDEF123", "https://us02web.zoom.us/j/81234567890?pwd=abcDEF123"),
            ("https://acme.zoom.us/my/jane.doe", "https://acme.zoom.us/my/jane.doe"),
            ("Zoom: <https://zoom.us/j/123456789>", "https://zoom.us/j/123456789"),
            (teams_text.as_str(), teams),
            ("(https://teams.live.com/meet/9876543210?p=abcd)", "https://teams.live.com/meet/9876543210?p=abcd"),
            ("https://acme.webex.com/meet/jdoe", "https://acme.webex.com/meet/jdoe"),
            ("https://acme.webex.com/acme/j.php?MTID=m123", "https://acme.webex.com/acme/j.php?MTID=m123"),
            ("https://whereby.com/team-room!", "https://whereby.com/team-room"),
            ("https://meet.jit.si/MyRoom", "https://meet.jit.si/MyRoom"),
            ("https://global.gotomeeting.com/join/123456789", "https://global.gotomeeting.com/join/123456789"),
            ("https://app.slack.com/huddle/T123/C456", "https://app.slack.com/huddle/T123/C456"),
            ("https://discord.gg/abc123", "https://discord.gg/abc123"),
            // The fragment holds the password; it is kept.
            ("https://meet.proton.me/join/id-ABC123#pwd-xyz", "https://meet.proton.me/join/id-ABC123#pwd-xyz"),
        ];
        for (text, want) in cases {
            assert_eq!(find(text).as_deref(), Some(want), "{text}");
        }
    }

    #[test]
    fn ignores_other_links() {
        for text in [
            "https://support.google.com/a/users/answer/9282720",
            "https://zoom.us/download",
            "https://www.microsoft.com/en-us/microsoft-teams/join-a-meeting",
            "https://example.com/zoom.us/j/1",
            "Room 4, floor 2",
            "mail me@zoom.us/j/1",
            "https://meet.google.com/",
            "https://evilzoom.us/j/1",
        ] {
            assert_eq!(find(text), None, "{text}");
        }
    }

    #[test]
    fn unwraps_redirects_and_html() {
        let google = r#"<a href="https://www.google.com/url?q=https%3A%2F%2Fus06web.zoom.us%2Fj%2F8812345%3Fpwd%3Dqq&amp;sa=D&amp;source=calendar">Join Zoom Meeting</a>"#;
        assert_eq!(find(google).as_deref(), Some("https://us06web.zoom.us/j/8812345?pwd=qq"));

        let plain_q = "https://www.google.com/url?q=https://meet.google.com/abc-defg-hij&sa=D";
        assert_eq!(find(plain_q).as_deref(), Some("https://meet.google.com/abc-defg-hij"));

        let outlook = "https://eur01.safelinks.protection.outlook.com/?url=https%3A%2F%2Fteams.microsoft.com%2Fl%2Fmeetup-join%2Fabc&data=x";
        assert_eq!(find(outlook).as_deref(), Some("https://teams.microsoft.com/l/meetup-join/abc"));

        let escaped = "Join<br>https://zoom.us/j/99?pwd=a&amp;uname=b</p>";
        assert_eq!(find(escaped).as_deref(), Some("https://zoom.us/j/99?pwd=a&uname=b"));
        assert_eq!(meeting_url(plain_q).as_deref(), Some("https://meet.google.com/abc-defg-hij"));
        assert_eq!(meeting_url("https://example.com"), None);
    }

    #[test]
    fn searches_texts_in_order() {
        let location = "https://teams.microsoft.com/l/meetup-join/xyz";
        let description = "or https://zoom.us/j/1";
        assert_eq!(find_meeting_url(&[location, description]).as_deref(), Some(location));
        assert_eq!(find_meeting_url(&["Office", description]).as_deref(), Some("https://zoom.us/j/1"));
        assert_eq!(find_meeting_url(&[]), None);
    }

    #[test]
    fn converts_html_descriptions() {
        let html = "Agenda:<br><ul><li>Budget &amp; plan</li><li>Q&amp;A</li></ul><p>Join <a href=\"https://www.google.com/url?q=https://zoom.us/j/1&amp;sa=D\">here</a></p><p></p><p>Bye&nbsp;now</p>";
        assert_eq!(html_to_text(html), "Agenda:\n\n- Budget & plan\n- Q&A\n\nJoin here (https://zoom.us/j/1)\n\nBye now");

        let same = r#"<a href="https://zoom.us/j/1">https://zoom.us/j/1</a>"#;
        assert_eq!(html_to_text(same), "https://zoom.us/j/1");

        assert_eq!(html_to_text("  Plain\r\n\r\n\r\n text  with  spaces <3 "), "Plain\n\ntext with spaces <3");
        assert_eq!(html_to_text("<b>Bold</b><style>p{}</style> x"), "Bold x");
    }

    #[test]
    fn helpers() {
        assert_eq!(truncate("abcdef", 3), "abc…");
        assert_eq!(truncate("añb", 5), "añb");
        assert_eq!(percent_decode("a%20b+c%zz%4"), "a b c%zz%4");
        assert_eq!(decode_entities("&#39;x&#x27; &unknown; & y"), "'x' &unknown; & y");
    }
}
