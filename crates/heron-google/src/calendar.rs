//! Google Calendar through Calendar API v3, polled every `calendar.poll_secs`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use heron_core::config::{CalendarSettings, GoogleAccount};
use heron_core::{CalendarEvent, Hub, SourceKind, SourceState};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tracing::{info, warn};
use url::Url;

use crate::oauth::{NeedsAuth, Tokens};
use crate::{Backoff, short};

const API: &str = "https://www.googleapis.com/calendar/v3/";

/// Publishes upcoming events until the login stops working. Only returns an
/// error, which is [`NeedsAuth`] unless something is badly wrong.
pub async fn run(
    account: GoogleAccount,
    settings: CalendarSettings,
    http: reqwest::Client,
    tokens: Arc<Tokens>,
    hub: Hub,
) -> Result<()> {
    let api = Api { http, tokens, account_id: account.id.clone() };
    let mut backoff = Backoff::default();
    let mut palette = None;
    let mut first = true;
    loop {
        let delay = match poll(&api, &account, settings.days_ahead, &mut palette).await {
            Ok(events) => {
                if first {
                    info!("google/{} calendar: {} upcoming events", account.id, events.len());
                    first = false;
                }
                hub.set_calendar(&account.id, events);
                hub.set_status(&account.id, SourceKind::GoogleCalendar, SourceState::Ok, None);
                backoff.reset();
                Duration::from_secs(settings.poll_secs.max(30))
            }
            Err(err) if crate::oauth::needs_auth(&err).is_some() => return Err(err),
            Err(err) => {
                let delay = backoff.next();
                warn!("google/{} calendar: {err:#}; retrying in {}s", account.id, delay.as_secs());
                hub.set_status(&account.id, SourceKind::GoogleCalendar, SourceState::Error, Some(short(&err)));
                delay
            }
        };
        tokio::time::sleep(delay).await;
    }
}

async fn poll(
    api: &Api,
    account: &GoogleAccount,
    days_ahead: u32,
    palette: &mut Option<HashMap<String, String>>,
) -> Result<Vec<CalendarEvent>> {
    let calendars: Vec<CalendarListEntry> = api.paged(api.url(&["users", "me", "calendarList"]), &[]).await
        .context("listing calendars")?;
    let calendars = choose(calendars, account.calendars.as_deref());

    if palette.is_none() {
        let colors: Colors = api.get(api.url(&["colors"])).await.context("reading the color palette")?;
        *palette = Some(colors.event.into_iter().map(|(id, c)| (id, c.background)).collect());
    }
    let palette = palette.as_ref().expect("set above");

    let now = Timestamp::now();
    let horizon = now + SignedDuration::from_hours(24 * i64::from(days_ahead));
    let (time_min, time_max) = (now.to_string(), horizon.to_string());
    let query = [
        ("timeMin", time_min.as_str()),
        ("timeMax", time_max.as_str()),
        ("singleEvents", "true"),
        ("orderBy", "startTime"),
        ("maxResults", "250"),
    ];
    let tz = TimeZone::system();

    let mut events = Vec::new();
    for calendar in &calendars {
        let items: Vec<Event> = api.paged(api.url(&["calendars", &calendar.id, "events"]), &query).await
            .with_context(|| format!("reading calendar {:?}", calendar.name))?;
        events.extend(items.into_iter().filter_map(|e| map_event(&account.id, calendar, e, palette, &tz)));
    }
    Ok(events)
}

/// A calendar that will be read.
#[derive(Debug, Clone, PartialEq)]
pub struct Calendar {
    pub id: String,
    pub name: String,
    pub color: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarListEntry {
    pub id: String,
    pub summary: Option<String>,
    pub summary_override: Option<String>,
    pub background_color: Option<String>,
    #[serde(default)]
    pub selected: bool,
    #[serde(default)]
    pub primary: bool,
}

/// The configured ids (where "primary" means the account's own calendar),
/// or else every calendar ticked in Google Calendar's sidebar.
pub fn choose(entries: Vec<CalendarListEntry>, wanted: Option<&[String]>) -> Vec<Calendar> {
    entries
        .into_iter()
        .filter(|e| match wanted {
            Some(ids) => ids.iter().any(|id| *id == e.id || (id == "primary" && e.primary)),
            None => e.selected,
        })
        .map(|e| Calendar {
            name: e.summary_override.or(e.summary).unwrap_or_else(|| e.id.clone()),
            id: e.id,
            color: e.background_color,
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct Colors {
    #[serde(default)]
    event: HashMap<String, Color>,
}

#[derive(Debug, Deserialize)]
struct Color {
    background: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub id: String,
    pub status: Option<String>,
    pub summary: Option<String>,
    pub location: Option<String>,
    pub start: Option<EventTime>,
    pub end: Option<EventTime>,
    pub html_link: Option<String>,
    pub color_id: Option<String>,
    #[serde(default)]
    pub attendees: Vec<Attendee>,
    pub event_type: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventTime {
    pub date: Option<String>,
    pub date_time: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attendee {
    #[serde(rename = "self", default)]
    pub is_self: bool,
    pub response_status: Option<String>,
}

/// Unix seconds and whether it is a whole day. Dates count from local midnight.
fn parse_time(time: &EventTime, tz: &TimeZone) -> Option<(i64, bool)> {
    if let Some(date_time) = &time.date_time {
        return date_time.parse::<Timestamp>().ok().map(|t| (t.as_second(), false));
    }
    let date: jiff::civil::Date = time.date.as_deref()?.parse().ok()?;
    let midnight = date.to_zoned(tz.clone()).ok()?;
    Some((midnight.timestamp().as_second(), true))
}

/// Maps one expanded occurrence; `None` for cancelled, declined and
/// working-location entries.
pub fn map_event(
    source: &str,
    calendar: &Calendar,
    event: Event,
    palette: &HashMap<String, String>,
    tz: &TimeZone,
) -> Option<CalendarEvent> {
    if event.status.as_deref() == Some("cancelled") || event.event_type.as_deref() == Some("workingLocation") {
        return None;
    }
    if event.attendees.iter().any(|a| a.is_self && a.response_status.as_deref() == Some("declined")) {
        return None;
    }
    let (start, all_day) = parse_time(event.start.as_ref()?, tz)?;
    let end = event.end.as_ref().and_then(|t| parse_time(t, tz)).map(|(t, _)| t)
        .unwrap_or(start + if all_day { 86_400 } else { 0 });
    let color = event.color_id.as_ref().and_then(|id| palette.get(id).cloned()).or_else(|| calendar.color.clone());
    let title = event.summary.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "(No title)".into());
    Some(CalendarEvent {
        // The same event shows up in every calendar it was shared to.
        id: format!("{}/{}", calendar.id, event.id),
        source: source.to_owned(),
        calendar: calendar.name.clone(),
        title,
        location: event.location.filter(|l| !l.trim().is_empty()),
        start,
        end,
        all_day,
        color,
        url: event.html_link,
            meeting_url: None,
            details: None,
    })
}

struct Api {
    http: reqwest::Client,
    tokens: Arc<Tokens>,
    account_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Page<T> {
    #[serde(default = "Vec::new")]
    items: Vec<T>,
    next_page_token: Option<String>,
}

impl Api {
    /// Percent-encodes each segment; calendar ids contain '@' and '#'.
    fn url(&self, segments: &[&str]) -> Url {
        let mut url = Url::parse(API).expect("valid constant URL");
        url.path_segments_mut().expect("base URL").pop_if_empty().extend(segments);
        url
    }

    async fn paged<T: DeserializeOwned>(&self, url: Url, query: &[(&str, &str)]) -> Result<Vec<T>> {
        let mut items = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut page_url = url.clone();
            {
                let mut pairs = page_url.query_pairs_mut();
                pairs.extend_pairs(query);
                if let Some(token) = &token {
                    pairs.append_pair("pageToken", token);
                }
            }
            let page: Page<T> = self.get(page_url).await?;
            items.extend(page.items);
            match page.next_page_token {
                Some(next) => token = Some(next),
                None => return Ok(items),
            }
        }
    }

    /// GET with the access token, retrying once with a new one after a 401.
    async fn get<T: DeserializeOwned>(&self, url: Url) -> Result<T> {
        for attempt in 0..2 {
            let token = self.tokens.access_token().await?;
            let response = self.http.get(url.clone()).bearer_auth(token).send().await
                .context("contacting the Google Calendar API")?;
            let status = response.status();
            if status.is_success() {
                return response.json().await.context("unexpected answer from the Google Calendar API");
            }
            if status == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
                self.tokens.invalidate().await;
                continue;
            }
            let body = response.bytes().await.unwrap_or_default();
            return Err(api_error(status.as_u16(), &body, &self.account_id));
        }
        unreachable!("the loop returns on its second attempt")
    }
}

/// Turns a Google API error body into an actionable message.
pub fn api_error(status: u16, body: &[u8], account_id: &str) -> anyhow::Error {
    #[derive(Deserialize)]
    struct Body {
        error: Detail,
    }
    #[derive(Deserialize)]
    struct Detail {
        #[serde(default)]
        message: String,
        #[serde(default)]
        status: String,
        #[serde(default)]
        errors: Vec<Reason>,
    }
    #[derive(Deserialize)]
    struct Reason {
        #[serde(default)]
        reason: String,
    }
    let Ok(Body { error }) = serde_json::from_slice::<Body>(body) else {
        return anyhow::anyhow!("Google Calendar API answered HTTP {status}");
    };
    let reasons: Vec<&str> = error.errors.iter().map(|r| r.reason.as_str()).collect();
    if reasons.contains(&"accessNotConfigured") || error.message.contains("has not been used in project") {
        return anyhow::anyhow!(
            "the Google Calendar API is not enabled for the OAuth client's Cloud project; enable it at \
             https://console.cloud.google.com/apis/library/calendar-json.googleapis.com"
        );
    }
    if reasons.contains(&"insufficientPermissions") || error.message.contains("insufficient authentication scopes") {
        return NeedsAuth(format!("calendar access was not granted; run `heron auth google {account_id}`")).into();
    }
    if status == 401 {
        return NeedsAuth(format!("login rejected; run `heron auth google {account_id}`")).into();
    }
    let detail = if error.message.is_empty() { error.status } else { error.message };
    anyhow::anyhow!("Google Calendar API: {detail} (HTTP {status})")
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = r##"{"kind":"calendar#calendarList","items":[
        {"id":"me@gmail.com","summary":"me@gmail.com","summaryOverride":"Personal","backgroundColor":"#9fe1e7","selected":true,"primary":true},
        {"id":"en.latvian#holiday@group.v.calendar.google.com","summary":"Holidays in Latvia","backgroundColor":"#16a765","selected":true},
        {"id":"hidden@group.calendar.google.com","summary":"Hidden","backgroundColor":"#000000"}
    ]}"##;

    const EVENTS: &str = r##"{"kind":"calendar#events","items":[
        {"id":"abc_20261023T070000Z","status":"confirmed","summary":"Standup","location":"Room 1",
         "start":{"dateTime":"2026-10-23T10:00:00+03:00","timeZone":"Europe/Riga"},
         "end":{"dateTime":"2026-10-23T10:15:00+03:00"},"htmlLink":"https://www.google.com/calendar/event?eid=x","colorId":"11"},
        {"id":"allday","status":"confirmed","summary":"Holiday","start":{"date":"2026-11-18"},"end":{"date":"2026-11-19"}},
        {"id":"gone","status":"cancelled"},
        {"id":"nope","status":"confirmed","summary":"Declined sync","start":{"dateTime":"2026-10-24T09:00:00Z"},"end":{"dateTime":"2026-10-24T10:00:00Z"},
         "attendees":[{"email":"boss@x.com","responseStatus":"accepted"},{"email":"me@gmail.com","self":true,"responseStatus":"declined"}]},
        {"id":"accepted","status":"confirmed","start":{"dateTime":"2026-10-25T09:00:00Z"},"end":{"dateTime":"2026-10-25T10:00:00Z"},
         "attendees":[{"email":"me@gmail.com","self":true,"responseStatus":"accepted"}]},
        {"id":"office","eventType":"workingLocation","start":{"date":"2026-10-26"},"end":{"date":"2026-10-27"}}
    ],"nextPageToken":null}"##;

    #[test]
    fn chooses_selected_or_configured() {
        let entries = || serde_json::from_str::<Page<CalendarListEntry>>(LIST).unwrap().items;
        let names: Vec<String> = choose(entries(), None).into_iter().map(|c| c.name).collect();
        assert_eq!(names, ["Personal", "Holidays in Latvia"]);

        let wanted = vec!["primary".to_owned(), "hidden@group.calendar.google.com".to_owned()];
        let ids: Vec<String> = choose(entries(), Some(&wanted)).into_iter().map(|c| c.id).collect();
        assert_eq!(ids, ["me@gmail.com", "hidden@group.calendar.google.com"]);
    }

    #[test]
    fn maps_events() {
        let page: Page<Event> = serde_json::from_str(EVENTS).unwrap();
        let calendar = Calendar { id: "me@gmail.com".into(), name: "Personal".into(), color: Some("#9fe1e7".into()) };
        let palette = HashMap::from([("11".to_owned(), "#dc2127".to_owned())]);
        let tz = TimeZone::fixed(jiff::tz::offset(2));
        let events: Vec<CalendarEvent> =
            page.items.into_iter().filter_map(|e| map_event("personal", &calendar, e, &palette, &tz)).collect();

        let ids: Vec<&str> = events.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["me@gmail.com/abc_20261023T070000Z", "me@gmail.com/allday", "me@gmail.com/accepted"]);

        let standup = &events[0];
        assert_eq!(standup.source, "personal");
        assert_eq!(standup.calendar, "Personal");
        assert_eq!(standup.title, "Standup");
        assert_eq!(standup.location.as_deref(), Some("Room 1"));
        assert_eq!(standup.start, 1_792_738_800); // 2026-10-23T07:00:00Z
        assert_eq!(standup.end - standup.start, 15 * 60);
        assert!(!standup.all_day);
        assert_eq!(standup.color.as_deref(), Some("#dc2127"));
        assert_eq!(standup.url.as_deref(), Some("https://www.google.com/calendar/event?eid=x"));

        let holiday = &events[1];
        assert!(holiday.all_day);
        assert_eq!(holiday.start, 1_794_952_800); // 2026-11-18T00:00:00+02:00
        assert_eq!(holiday.end - holiday.start, 86_400);
        assert_eq!(holiday.color.as_deref(), Some("#9fe1e7"));

        assert_eq!(events[2].title, "(No title)");
    }

    #[test]
    fn calendar_ids_are_escaped() {
        let api = Api { http: reqwest::Client::new(), tokens: Arc::new(dummy_tokens()), account_id: "x".into() };
        let url = api.url(&["calendars", "en.latvian#holiday@group.v.calendar.google.com", "events"]);
        assert_eq!(
            url.as_str(),
            "https://www.googleapis.com/calendar/v3/calendars/en.latvian%23holiday@group.v.calendar.google.com/events"
        );
    }

    fn dummy_tokens() -> Tokens {
        let client = heron_core::config::GoogleClient { client_id: String::new(), client_secret: String::new() };
        Tokens::new(reqwest::Client::new(), client, String::new(), "x".into())
    }

    #[test]
    fn api_errors() {
        let disabled = br#"{"error":{"code":403,"message":"Google Calendar API has not been used in project 123 before or it is disabled.","errors":[{"reason":"accessNotConfigured"}],"status":"PERMISSION_DENIED"}}"#;
        assert!(api_error(403, disabled, "p").to_string().contains("calendar-json.googleapis.com"));

        let scope = br#"{"error":{"code":403,"message":"Request had insufficient authentication scopes.","errors":[{"reason":"insufficientPermissions"}],"status":"PERMISSION_DENIED"}}"#;
        assert!(crate::oauth::needs_auth(&api_error(403, scope, "p")).is_some());

        assert!(api_error(500, b"oops", "p").to_string().contains("500"));
    }
}
