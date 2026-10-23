//! Proton Calendar, read-only. Event content is end-to-end encrypted:
//! address keys decrypt each calendar's member passphrase, which unlocks the
//! calendar keys, which decrypt each event's session keys, which decrypt the
//! iCalendar parts. Follows WebClients `calendar/crypto` and `deserialize.ts`.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use heron_core::meeting::{find_meeting_url, html_to_text, meeting_url, truncate};
use heron_core::{CalendarEvent, EventDetails, Hub, SourceKind, SourceState};
use proton_crypto_account::keys::UnlockedAddressKey;
use proton_crypto_account::proton_crypto::crypto::{DataEncoding, Decryptor, DecryptorSync, PGPProviderSync};
use serde::Deserialize;
use tracing::{debug, warn};

use crate::Backoff;
use crate::api::{Api, NeedsAuth, flag, needs_auth, or_default};
use crate::ical::{self, Occurrence, Time, VEvent};
use crate::keys::Keyring;

/// Member flags that mean the calendar cannot be decrypted (WebClients `CALENDAR_FLAGS`).
const UNUSABLE: u32 = 4 | 8 | 16;
/// Event part types: bit 1 encrypted, bit 2 signed.
const PART_ENCRYPTED: u8 = 1;
const PAGE_SIZE: usize = 100;
/// Description characters kept in [`EventDetails`].
const DESCRIPTION_MAX: usize = 2000;
/// Attendees kept in [`EventDetails`].
const ATTENDEES_MAX: usize = 30;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ApiCalendar {
    #[serde(rename = "ID")]
    id: String,
    #[serde(default, deserialize_with = "or_default")]
    members: Vec<Member>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Member {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "AddressID")]
    address_id: Option<String>,
    #[serde(default, deserialize_with = "or_default")]
    name: String,
    color: Option<String>,
    /// 0 when hidden in Proton Calendar.
    display: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "or_default")]
    flags: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Bootstrap {
    keys: Vec<CalendarKey>,
    passphrase: Passphrase,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CalendarKey {
    private_key: String,
    #[serde(rename = "PassphraseID")]
    passphrase_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Passphrase {
    #[serde(rename = "ID")]
    id: String,
    #[serde(default, deserialize_with = "or_default")]
    member_passphrases: Vec<MemberPassphrase>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct MemberPassphrase {
    #[serde(rename = "MemberID")]
    member_id: String,
    passphrase: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ApiEvent {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "CalendarID", default, deserialize_with = "or_default")]
    calendar_id: String,
    #[serde(rename = "UID", default, deserialize_with = "or_default")]
    uid: String,
    #[serde(default, deserialize_with = "or_default")]
    start_time: i64,
    #[serde(default, deserialize_with = "or_default")]
    end_time: i64,
    #[serde(default, deserialize_with = "flag")]
    full_day: bool,
    #[serde(rename = "RecurrenceID")]
    recurrence_id: Option<i64>,
    shared_key_packet: Option<String>,
    calendar_key_packet: Option<String>,
    address_key_packet: Option<String>,
    #[serde(rename = "AddressID")]
    address_id: Option<String>,
    #[serde(default, deserialize_with = "or_default")]
    shared_events: Vec<Part>,
    #[serde(default, deserialize_with = "or_default")]
    calendar_events: Vec<Part>,
    /// ATTENDEE properties, encrypted with the shared session key.
    #[serde(default, deserialize_with = "or_default")]
    attendees_events: Vec<Part>,
    /// Each attendee's answer in the clear, keyed by the `X-PM-TOKEN` of
    /// their ATTENDEE property.
    #[serde(default, deserialize_with = "or_default")]
    attendees_info: AttendeesInfo,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AttendeesInfo {
    #[serde(default, deserialize_with = "or_default")]
    attendees: Vec<AttendeeInfo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AttendeeInfo {
    #[serde(default, deserialize_with = "or_default")]
    token: String,
    /// WebClients `ATTENDEE_STATUS_API`.
    #[serde(default, deserialize_with = "or_default")]
    status: u8,
}

/// iCalendar PARTSTAT for an `ATTENDEE_STATUS_API` value.
fn partstat(status: u8) -> &'static str {
    match status {
        1 => "TENTATIVE",
        2 => "DECLINED",
        3 => "ACCEPTED",
        _ => "NEEDS-ACTION",
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Part {
    #[serde(rename = "Type")]
    kind: u8,
    data: String,
}

/// Runs until the session needs a new login, which is returned as the error.
pub async fn run(api: &Api, account: &str, days_ahead: u32, poll_secs: u64, hub: &Hub) -> anyhow::Error {
    let provider = proton_crypto_account::proton_crypto::new_pgp_provider();
    let mut backoff = Backoff::default();
    loop {
        let Err(err) = connected(&provider, api, account, days_ahead, poll_secs, hub, &mut backoff).await;
        if needs_auth(&err).is_some() {
            return err;
        }
        let delay = backoff.next();
        warn!("proton/{account} calendar: {err:#}; retrying in {}s", delay.as_secs());
        hub.set_status(account, SourceKind::ProtonCalendar, SourceState::Error, Some(crate::short(&err)));
        tokio::time::sleep(delay).await;
    }
}

async fn connected<P: PGPProviderSync>(
    provider: &P,
    api: &Api,
    account: &str,
    days_ahead: u32,
    poll_secs: u64,
    hub: &Hub,
    backoff: &mut Backoff,
) -> Result<Infallible> {
    let passphrase = api.session().await.and_then(|s| s.key_passphrase)
        .ok_or_else(|| NeedsAuth("the stored session has no key passphrase".into()))?;
    let keyring = Keyring::unlock(provider, api, &passphrase).await?;
    let mut cache: HashMap<String, Vec<P::PrivateKey>> = HashMap::new();
    let timezone = local_timezone();

    loop {
        let from = heron_core::now();
        let to = from + i64::from(days_ahead) * 86_400;
        let events = sync(provider, api, &keyring, &mut cache, account, &timezone, from, to).await?;
        debug!("proton/{account}: {} calendar occurrences", events.len());
        hub.set_calendar(account, events);
        hub.set_status(account, SourceKind::ProtonCalendar, SourceState::Ok, None);
        backoff.reset();
        tokio::time::sleep(Duration::from_secs(poll_secs.max(30))).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn sync<P: PGPProviderSync>(
    provider: &P,
    api: &Api,
    keyring: &Keyring<P>,
    cache: &mut HashMap<String, Vec<P::PrivateKey>>,
    account: &str,
    timezone: &str,
    from: i64,
    to: i64,
) -> Result<Vec<CalendarEvent>> {
    #[derive(Deserialize)]
    struct Calendars {
        #[serde(rename = "Calendars")]
        calendars: Vec<ApiCalendar>,
    }
    let calendars = api.get::<Calendars>("/calendar/v1", &[]).await?.calendars;
    debug!("proton/{account}: {} calendars", calendars.len());

    let mut out = Vec::new();
    for calendar in calendars {
        let Some(member) = calendar.members.iter()
            .find(|m| m.address_id.as_ref().is_some_and(|a| keyring.addresses.contains_key(a)))
            .cloned()
        else {
            debug!("calendar {}: no member with an unlocked address", calendar.id);
            continue;
        };
        let hidden = member.display.as_ref().is_some_and(|d| d.as_i64() == Some(0) || d.as_bool() == Some(false));
        if hidden || member.flags & UNUSABLE != 0 {
            debug!("calendar {:?}: skipped (hidden: {hidden}, flags: {})", member.name, member.flags);
            continue;
        }
        let address_keys = &keyring.addresses[member.address_id.as_deref().unwrap_or_default()];

        if !cache.contains_key(&calendar.id) {
            let bootstrap: Bootstrap = api.get(&format!("/calendar/v2/{}/bootstrap", calendar.id), &[]).await?;
            match unlock_calendar(provider, &bootstrap, &member, address_keys) {
                Ok(keys) => {
                    cache.insert(calendar.id.clone(), keys);
                }
                Err(err) => {
                    warn!("calendar {:?}: {err:#}", member.name);
                    continue;
                }
            }
        }
        let calendar_keys = &cache[&calendar.id];

        let events = fetch_events(api, &calendar.id, timezone, from, to).await?;
        debug!("calendar {:?}: {} events in window", member.name, events.len());
        let mut decrypted = Vec::with_capacity(events.len());
        let mut failed = 0;
        for event in events {
            match decrypt_event(provider, calendar_keys, keyring, &event) {
                Ok(vevent) => decrypted.push((event, vevent)),
                Err(err) => {
                    failed += 1;
                    warn!("calendar {:?}, event {}: {err:#}", member.name, event.id);
                }
            }
        }
        if failed > 0 {
            // Keys may have rotated; bootstrap again next time.
            cache.remove(&calendar.id);
        }
        out.extend(to_occurrences(&decrypted, account, &member, &keyring.emails, from, to));
    }
    Ok(out)
}

/// Decrypts the member passphrase with the address keys and unlocks the
/// calendar keys with it.
fn unlock_calendar<P: PGPProviderSync>(
    provider: &P,
    bootstrap: &Bootstrap,
    member: &Member,
    address_keys: &[UnlockedAddressKey<P>],
) -> Result<Vec<P::PrivateKey>> {
    let armored = &bootstrap.passphrase.member_passphrases.iter()
        .find(|p| p.member_id == member.id)
        .context("no passphrase for our calendar member")?
        .passphrase;
    // The signature over the passphrase is not verified; heron only reads.
    let passphrase = provider.new_decryptor()
        .with_decryption_key_refs(address_keys)
        .decrypt(armored, DataEncoding::Armor)
        .context("decrypting the calendar passphrase")?;
    let passphrase: &[u8] = passphrase.as_ref();

    let keys: Vec<P::PrivateKey> = bootstrap.keys.iter()
        .filter(|k| k.passphrase_id.as_ref().is_none_or(|id| *id == bootstrap.passphrase.id))
        .filter_map(|k| provider.private_key_import(&k.private_key, passphrase, DataEncoding::Armor).ok())
        .collect();
    anyhow::ensure!(!keys.is_empty(), "no calendar key could be unlocked");
    Ok(keys)
}

/// Proton answers "Time window is too big" for long ranges, so the window is
/// queried in chunks of this many seconds.
const CHUNK: i64 = 30 * 86_400;

/// Every event touching the window, in chunks Proton accepts.
async fn fetch_events(api: &Api, calendar: &str, timezone: &str, from: i64, to: i64) -> Result<Vec<ApiEvent>> {
    let mut seen = HashSet::new();
    let mut events = Vec::new();
    let mut start = from;
    while start < to {
        let end = (start + CHUNK).min(to);
        for event in fetch_chunk(api, calendar, timezone, start, end).await? {
            if seen.insert(event.id.clone()) {
                events.push(event);
            }
        }
        start = end;
    }
    Ok(events)
}

/// Every event touching one chunk, through the four query types the web app uses.
async fn fetch_chunk(api: &Api, calendar: &str, timezone: &str, from: i64, to: i64) -> Result<Vec<ApiEvent>> {
    #[derive(Deserialize)]
    struct Page {
        #[serde(rename = "Events", default)]
        events: Vec<ApiEvent>,
        #[serde(rename = "More", default, deserialize_with = "flag")]
        more: bool,
    }
    let mut seen = HashSet::new();
    let mut events = Vec::new();
    // PartDayInsideWindow, PartDayBeforeWindow, FullDayInsideWindow, FullDayBeforeWindow
    for kind in 0..4 {
        for page in 0..50 {
            let query = [
                ("Start", from.max(0).to_string()),
                ("End", to.max(0).to_string()),
                ("Timezone", timezone.to_owned()),
                ("Type", kind.to_string()),
                ("Page", page.to_string()),
                ("PageSize", PAGE_SIZE.to_string()),
            ];
            let response: Page = api.get(&format!("/calendar/v1/{calendar}/events"), &query).await?;
            for event in response.events {
                if seen.insert(event.id.clone()) {
                    events.push(event);
                }
            }
            if !response.more {
                break;
            }
        }
    }
    Ok(events)
}

fn decrypt_event<P: PGPProviderSync>(
    provider: &P,
    calendar_keys: &[P::PrivateKey],
    keyring: &Keyring<P>,
    event: &ApiEvent,
) -> Result<VEvent> {
    // Invitations added automatically are encrypted to the address, not the calendar.
    let address_keys = event.address_key_packet.as_ref()
        .and(event.address_id.as_ref())
        .and_then(|id| keyring.addresses.get(id));
    let session_key = |packet: &str| -> Result<P::SessionKey> {
        let packet = BASE64.decode(packet).context("key packet is not base64")?;
        let decryptor = provider.new_decryptor();
        let decryptor = match address_keys {
            Some(keys) => decryptor.with_decryption_key_refs(keys),
            None => decryptor.with_decryption_keys(calendar_keys),
        };
        decryptor.decrypt_session_key(packet).context("decrypting the session key")
    };
    let shared_key = event.address_key_packet.as_deref().or(event.shared_key_packet.as_deref())
        .map(session_key).transpose()?;
    let calendar_key = event.calendar_key_packet.as_deref().map(session_key).transpose()?;

    let mut parts = Vec::new();
    for part in &event.shared_events {
        parts.push(read_part(provider, part, shared_key.as_ref())?);
    }
    for part in &event.calendar_events {
        // Personal details (status, notes); not needed for the agenda, so a failure is not fatal.
        match read_part(provider, part, calendar_key.as_ref().or(shared_key.as_ref())) {
            Ok(text) => parts.push(text),
            Err(err) => debug!("event {}: calendar part: {err:#}", event.id),
        }
    }
    for part in &event.attendees_events {
        // Only for details and declined invitations; not fatal either.
        match read_part(provider, part, shared_key.as_ref()) {
            Ok(text) => parts.push(text),
            Err(err) => debug!("event {}: attendees part: {err:#}", event.id),
        }
    }

    let mut vevent = VEvent::from_parts(parts.iter().map(String::as_str));
    apply_answers(&mut vevent, &event.attendees_info);
    if vevent.start.is_none() {
        // Fall back to the clear metadata.
        let time = |ts: i64| chrono::DateTime::from_timestamp(ts, 0).map(|dt| dt.naive_utc());
        vevent.start = time(event.start_time).map(|t| if event.full_day { Time::Date(t.date()) } else { Time::Utc(t) });
        vevent.end = time(event.end_time).map(|t| if event.full_day { Time::Date(t.date()) } else { Time::Utc(t) });
    }
    Ok(vevent)
}

/// Answers live in the clear `AttendeesInfo`, not in the encrypted
/// ATTENDEE properties (WebClients `toInternalAttendee`).
fn apply_answers(vevent: &mut VEvent, info: &AttendeesInfo) {
    for attendee in &mut vevent.attendees {
        let answer = attendee.token.as_ref().and_then(|token| info.attendees.iter().find(|a| a.token == *token));
        if let Some(answer) = answer {
            attendee.partstat = Some(partstat(answer.status).to_owned());
        }
    }
}

/// True when the user was invited and declined with every address invited.
fn declined(vevent: &VEvent, me: &[String]) -> bool {
    let mut mine = vevent.attendees.iter().filter(|a| a.email.as_ref().is_some_and(|e| me.contains(e))).peekable();
    mine.peek().is_some() && mine.all(|a| a.partstat.as_deref() == Some("DECLINED"))
}

/// The link of a conference property (Proton's, Google's, Microsoft's or
/// RFC 7986 `CONFERENCE`, taken as is when https; `URL` only when it is a
/// known video service), else a link in the location or description.
fn event_meeting_url(vevent: &VEvent) -> Option<String> {
    vevent.conference_urls()
        .find_map(|(property, url)| match meeting_url(url) {
            Some(url) => Some(url),
            None if property != "URL" && url.starts_with("https://") => Some(url.to_owned()),
            None => None,
        })
        .or_else(|| {
            let texts = [vevent.location.as_deref(), vevent.description.as_deref()];
            find_meeting_url(&texts.into_iter().flatten().collect::<Vec<_>>())
        })
}

/// Description, organizer unless it is the user, and the other people
/// invited (not the user, the organizer or rooms).
fn details(vevent: &VEvent, me: &[String]) -> EventDetails {
    let is_me = |email: &Option<String>| email.as_ref().is_some_and(|e| me.contains(e));
    let description = vevent.description.as_deref().map(html_to_text).filter(|d| !d.is_empty())
        .map(|d| truncate(&d, DESCRIPTION_MAX));
    let organizer = vevent.organizer.as_ref().filter(|o| !is_me(&o.email));
    let organizer_email = vevent.organizer.as_ref().and_then(|o| o.email.clone());
    let attendees = vevent.attendees.iter()
        .filter(|a| !is_me(&a.email) && !a.is_resource())
        .filter(|a| organizer_email.is_none() || a.email != organizer_email)
        .filter_map(|a| a.label())
        .take(ATTENDEES_MAX)
        .collect();
    EventDetails { description, organizer: organizer.and_then(|o| o.label()), attendees }
}

fn read_part<P: PGPProviderSync>(provider: &P, part: &Part, key: Option<&P::SessionKey>) -> Result<String> {
    if part.kind & PART_ENCRYPTED == 0 {
        return Ok(part.data.clone());
    }
    let key = key.context("encrypted part without a session key")?;
    let data = BASE64.decode(&part.data).context("part is not base64")?;
    let plain = provider.new_decryptor()
        .with_session_key_ref(key)
        .decrypt(data, DataEncoding::Bytes)
        .context("decrypting an event part")?;
    let plain: &[u8] = plain.as_ref();
    Ok(String::from_utf8_lossy(plain).into_owned())
}

/// Expands decrypted events into agenda entries. Occurrences replaced by a
/// RECURRENCE-ID event are dropped from the series; cancelled ones and
/// invitations the user (one of `me`, lowercase addresses) declined vanish.
fn to_occurrences(
    events: &[(ApiEvent, VEvent)],
    account: &str,
    member: &Member,
    me: &[String],
    from: i64,
    to: i64,
) -> Vec<CalendarEvent> {
    let uid = |(api, vevent): &(ApiEvent, VEvent)| vevent.uid.clone().unwrap_or_else(|| api.uid.clone());
    let recurrence = |(api, vevent): &(ApiEvent, VEvent)| vevent.recurrence_id.and_then(Time::timestamp).or(api.recurrence_id);

    let mut overrides: HashMap<String, Vec<i64>> = HashMap::new();
    for event in events {
        if let Some(at) = recurrence(event) {
            overrides.entry(uid(event)).or_default().push(at);
        }
    }

    let mut out = Vec::new();
    for event @ (api, vevent) in events {
        if vevent.cancelled() || declined(vevent, me) {
            continue;
        }
        let exception = recurrence(event).is_some();
        let occurrences = if exception {
            // An edited occurrence: its own DTSTART, no rule.
            ical::occurrences(&VEvent { rrule: None, ..vevent.clone() }, &[], from, to)
        } else {
            let skip = overrides.get(&uid(event)).map(Vec::as_slice).unwrap_or_default();
            ical::occurrences(vevent, skip, from, to).or_else(|err| {
                warn!("event {}: {err:#}; showing only its first occurrence", api.id);
                ical::occurrences(&VEvent { rrule: None, ..vevent.clone() }, &[], from, to)
            })
        };
        let occurrences: Vec<Occurrence> = match occurrences {
            Ok(list) => list,
            Err(err) => {
                warn!("event {}: {err:#}", api.id);
                continue;
            }
        };
        let recurring = vevent.rrule.is_some() || exception;
        let meeting_url = event_meeting_url(vevent);
        let details = details(vevent, me);
        for occurrence in occurrences {
            let recurrence_id = recurring.then(|| recurrence(event).unwrap_or(occurrence.start));
            out.push(CalendarEvent {
                id: format!("{}-{}", api.id, occurrence.start),
                source: account.to_owned(),
                calendar: member.name.clone(),
                title: vevent.summary.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "(no title)".into()),
                location: vevent.location.clone().filter(|s| !s.is_empty()),
                start: occurrence.start,
                end: occurrence.end,
                all_day: vevent.all_day(),
                color: member.color.clone(),
                url: Some(event_url(&api.calendar_id, &api.id, recurrence_id)),
                meeting_url: meeting_url.clone(),
                details: Some(details.clone()),
            });
        }
    }
    out
}

/// WebClients `getLinkToCalendarEvent`.
fn event_url(calendar: &str, event: &str, recurrence: Option<i64>) -> String {
    let mut url = format!("https://calendar.proton.me/event?Action=VIEW&EventID={event}&CalendarID={calendar}");
    if let Some(at) = recurrence {
        url.push_str(&format!("&RecurrenceID={at}"));
    }
    url
}

/// IANA name of the local zone, sent so the API picks full-day events correctly.
fn local_timezone() -> String {
    let valid = |name: &str| chrono_tz::Tz::from_str(name).is_ok();
    if let Ok(tz) = std::env::var("TZ") {
        let tz = tz.trim_start_matches(':');
        if valid(tz) {
            return tz.to_owned();
        }
    }
    if let Ok(target) = std::fs::read_link("/etc/localtime")
        && let Some((_, name)) = target.to_string_lossy().split_once("zoneinfo/")
        && valid(name)
    {
        return name.to_owned();
    }
    if let Ok(name) = std::fs::read_to_string("/etc/timezone")
        && valid(name.trim())
    {
        return name.trim().to_owned();
    }
    "UTC".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp()
    }

    fn member() -> Member {
        serde_json::from_str(r##"{"ID":"mem","AddressID":"addr","Name":"Work","Color":"#5252CC","Display":1,"Flags":1}"##).unwrap()
    }

    fn event(json: &str, parts: &[&str]) -> (ApiEvent, VEvent) {
        (serde_json::from_str(json).unwrap(), VEvent::from_parts(parts.iter().copied()))
    }

    #[test]
    fn parses_api_shapes() {
        let calendars: Vec<ApiCalendar> = serde_json::from_str(r##"[{"ID":"cal","Type":0,"Members":[
            {"ID":"mem","AddressID":"addr","Email":"me@proton.me","Name":"Personal","Color":"#8080FF","Display":1,"Flags":1,"Permissions":127}]}]"##).unwrap();
        assert_eq!(calendars[0].members[0].name, "Personal");
        let lenient: Member = serde_json::from_str(r#"{"ID":"m","AddressID":null,"Name":null,"Color":null,"Display":true,"Flags":null}"#).unwrap();
        assert_eq!((lenient.name.as_str(), lenient.flags), ("", 0));

        let bootstrap: Bootstrap = serde_json::from_str(r#"{"Code":1000,
            "Keys":[{"ID":"k","CalendarID":"cal","PrivateKey":"-----BEGIN PGP PRIVATE KEY BLOCK-----","PassphraseID":"pp","Flags":3}],
            "Passphrase":{"ID":"pp","Flags":0,"MemberPassphrases":[{"MemberID":"mem","Passphrase":"-----BEGIN PGP MESSAGE-----","Signature":"sig"}],"Invitations":[]},
            "Members":[],"CalendarSettings":{}}"#).unwrap();
        assert_eq!(bootstrap.passphrase.member_passphrases[0].member_id, "mem");
        assert_eq!(bootstrap.keys[0].passphrase_id.as_deref(), Some("pp"));

        let event: ApiEvent = serde_json::from_str(r#"{"ID":"ev","CalendarID":"cal","UID":"u","StartTime":1,"EndTime":2,
            "FullDay":0,"RecurrenceID":null,"SharedKeyPacket":"AAAA","CalendarKeyPacket":null,"AddressKeyPacket":null,"AddressID":null,
            "SharedEvents":[{"Type":2,"Data":"BEGIN:VCALENDAR","Signature":"s","Author":"me@proton.me"},{"Type":3,"Data":"AAAA","Signature":"s","Author":"me@proton.me"}],
            "CalendarEvents":[],"AttendeesEvents":[],"Notifications":null}"#).unwrap();
        assert_eq!(event.shared_events[1].kind & PART_ENCRYPTED, 1);
        assert!(!event.full_day);
    }

    #[test]
    fn exceptions_replace_series_occurrences() {
        let series = event(r#"{"ID":"ev1","CalendarID":"cal","UID":"u1"}"#, &[
            "BEGIN:VEVENT\r\nUID:u1\r\nDTSTART:20261020T090000Z\r\nDTEND:20261020T100000Z\r\nRRULE:FREQ=DAILY;COUNT=4\r\nEND:VEVENT",
            "BEGIN:VEVENT\r\nSUMMARY:Daily\r\nEND:VEVENT",
        ]);
        let moved = event(r#"{"ID":"ev2","CalendarID":"cal","UID":"u1","RecurrenceID":1792573200}"#, &[
            "BEGIN:VEVENT\r\nUID:u1\r\nRECURRENCE-ID:20261021T090000Z\r\nDTSTART:20261021T150000Z\r\nDTEND:20261021T160000Z\r\nEND:VEVENT",
            "BEGIN:VEVENT\r\nSUMMARY:Daily (moved)\r\nEND:VEVENT",
        ]);
        let cancelled = event(r#"{"ID":"ev3","CalendarID":"cal","UID":"u1"}"#, &[
            "BEGIN:VEVENT\r\nUID:u1\r\nRECURRENCE-ID:20261022T090000Z\r\nDTSTART:20261022T090000Z\r\nSTATUS:CANCELLED\r\nEND:VEVENT",
        ]);
        let events = [series, moved, cancelled];
        let list = to_occurrences(&events, "proton", &member(), &[], ts("2026-10-20T00:00:00Z"), ts("2026-10-30T00:00:00Z"));

        let got: Vec<(&str, i64)> = list.iter().map(|e| (e.title.as_str(), e.start)).collect();
        assert_eq!(got, [
            ("Daily", ts("2026-10-20T09:00:00Z")),
            ("Daily", ts("2026-10-23T09:00:00Z")),
            ("Daily (moved)", ts("2026-10-21T15:00:00Z")),
        ]);
        let first = &list[0];
        assert_eq!(first.id, format!("ev1-{}", ts("2026-10-20T09:00:00Z")));
        assert_eq!((first.calendar.as_str(), first.color.as_deref(), first.source.as_str()), ("Work", Some("#5252CC"), "proton"));
        assert_eq!(first.end - first.start, 3_600);
        assert_eq!(
            first.url.as_deref(),
            Some(format!("https://calendar.proton.me/event?Action=VIEW&EventID=ev1&CalendarID=cal&RecurrenceID={}", ts("2026-10-20T09:00:00Z")).as_str())
        );
        assert_eq!(list[2].url.as_deref(), Some("https://calendar.proton.me/event?Action=VIEW&EventID=ev2&CalendarID=cal&RecurrenceID=1792573200"));
    }

    /// Encrypts like a Proton client and decrypts through the real code path.
    #[test]
    fn decrypts_calendar_and_invite_events() {
        use proton_crypto_account::keys::{DecryptedAddressKey, KeyFlag, KeyId};
        use proton_crypto_account::proton_crypto::crypto::{
            Encryptor, EncryptorSync, KeyGenerator, KeyGeneratorSync, PGPMessage,
        };

        let provider = proton_crypto_account::proton_crypto::new_pgp_provider();
        let generate = |email: &str| provider.new_key_generator().with_user_id("t", email).generate().unwrap();
        let address_key = generate("me@proton.me");
        let address_public = provider.private_key_to_public_key(&address_key).unwrap();
        let calendar_key = generate("cal@proton.me");
        let calendar_public = provider.private_key_to_public_key(&calendar_key).unwrap();

        let locked = provider.private_key_export(&calendar_key, "calendar-secret", DataEncoding::Armor).unwrap();
        let member_passphrase = provider.new_encryptor().with_encryption_key(&address_public)
            .encrypt(b"calendar-secret").unwrap().armor().unwrap();
        let bootstrap = Bootstrap {
            keys: vec![CalendarKey {
                private_key: String::from_utf8(locked.as_ref().to_vec()).unwrap(),
                passphrase_id: Some("pp".into()),
            }],
            passphrase: Passphrase {
                id: "pp".into(),
                member_passphrases: vec![MemberPassphrase {
                    member_id: "mem".into(),
                    passphrase: String::from_utf8(member_passphrase).unwrap(),
                }],
            },
        };
        let address = DecryptedAddressKey {
            id: KeyId("ak".into()),
            flags: KeyFlag::default(),
            primary: true,
            is_v6: false,
            private_key: address_key,
            public_key: address_public.clone(),
        };
        let calendar_keys = unlock_calendar(&provider, &bootstrap, &member(), std::slice::from_ref(&address)).unwrap();
        let keyring = Keyring { addresses: HashMap::from([("addr".to_owned(), vec![address])]), emails: vec!["me@proton.me".into()] };

        let signed = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:u\r\nDTSTART:20261025T120000Z\r\nDTEND:20261025T130000Z\r\nEND:VEVENT\r\nEND:VCALENDAR";
        let secret = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:u\r\nSUMMARY:Dentist\r\nLOCATION:Main St 1\r\nEND:VEVENT\r\nEND:VCALENDAR";
        let split = |public| {
            let message = provider.new_encryptor().with_encryption_key(public).encrypt(secret).unwrap();
            (BASE64.encode(message.as_key_packets()), BASE64.encode(message.as_data_packet()))
        };
        let json = |packet: (String, String), invite: bool| {
            let (key_field, address) = if invite { ("AddressKeyPacket", "\"addr\"") } else { ("SharedKeyPacket", "null") };
            serde_json::json!({
                "ID": "ev", "CalendarID": "cal", "UID": "u", key_field: packet.0, "AddressID": serde_json::from_str::<serde_json::Value>(address).unwrap(),
                "SharedEvents": [{"Type": 2, "Data": signed}, {"Type": 3, "Data": packet.1}],
            })
            .to_string()
        };

        for (public, invite) in [(&calendar_public, false), (&address_public, true)] {
            let event: ApiEvent = serde_json::from_str(&json(split(public), invite)).unwrap();
            let vevent = decrypt_event(&provider, &calendar_keys, &keyring, &event).unwrap();
            assert_eq!(vevent.summary.as_deref(), Some("Dentist"));
            assert_eq!(vevent.location.as_deref(), Some("Main St 1"));
            assert_eq!(vevent.start.and_then(Time::timestamp), Some(ts("2026-10-25T12:00:00Z")));
        }

        // A wrong member passphrase must not unlock anything.
        let mut wrong = bootstrap;
        wrong.passphrase.member_passphrases[0].member_id = "other".into();
        assert!(unlock_calendar(&provider, &wrong, &member(), &keyring.addresses["addr"]).is_err());
    }

    #[test]
    fn details_meeting_links_and_declined_invitations() {
        const SHARED: &str = "BEGIN:VEVENT\r\nUID:{uid}\r\nDTSTART:20261025T120000Z\r\nDTEND:20261025T130000Z\r\n\
            SUMMARY:{uid}\r\nORGANIZER;CN=Boss:mailto:boss@x.com\r\n{extra}END:VEVENT";
        const ATTENDEES: &str = "BEGIN:VEVENT\r\nUID:{uid}\r\n\
            ATTENDEE;CN=Boss;X-PM-TOKEN=t0:mailto:boss@x.com\r\n\
            ATTENDEE;CN=me@proton.me;PARTSTAT=NEEDS-ACTION;X-PM-TOKEN=t1:mailto:Me@Proton.me\r\n\
            ATTENDEE;CN=Ann;X-PM-TOKEN=t2:mailto:ann@x.com\r\n\
            ATTENDEE;CUTYPE=ROOM;CN=Room:mailto:room@x.com\r\nEND:VEVENT";
        let make = |uid: &str, extra: &str, my_status: u8| {
            let mut api: ApiEvent = serde_json::from_str(&format!(r#"{{"ID":"{uid}","CalendarID":"cal","UID":"{uid}",
                "AttendeesInfo":{{"Attendees":[{{"Token":"t1","Status":{my_status}}},{{"Token":"t2","Status":3}}],"MoreAttendees":0}}}}"#)).unwrap();
            let shared = SHARED.replace("{uid}", uid).replace("{extra}", extra);
            let mut vevent = VEvent::from_parts([shared.as_str(), ATTENDEES.replace("{uid}", uid).as_str()]);
            apply_answers(&mut vevent, &std::mem::take(&mut api.attendees_info));
            (api, vevent)
        };
        let events = [
            make("teams", "LOCATION:Teams https://teams.microsoft.com/l/meetup-join/abc\r\n", 0),
            make("zoom", "DESCRIPTION:Notes\\nJoin: <https://acme.zoom.us/j/1?pwd=x>.\r\n", 3),
            make("proton", "X-PM-CONFERENCE-URL:https://meet.proton.me/join/id-a#pwd-b\r\nDESCRIPTION:https://zoom.us/j/2\r\n", 1),
            make("declined", "", 2),
            make("info", "URL:https://example.com/agenda\r\n", 3),
        ];
        let me = ["me@proton.me".to_owned()];
        let list = to_occurrences(&events, "p", &member(), &me, ts("2026-10-22T00:00:00Z"), ts("2026-10-29T00:00:00Z"));

        let got: Vec<(&str, Option<&str>)> = list.iter().map(|e| (e.title.as_str(), e.meeting_url.as_deref())).collect();
        assert_eq!(got, [
            ("teams", Some("https://teams.microsoft.com/l/meetup-join/abc")),
            ("zoom", Some("https://acme.zoom.us/j/1?pwd=x")),
            ("proton", Some("https://meet.proton.me/join/id-a#pwd-b")),
            ("info", None),
        ]);
        let zoom = list[1].details.as_ref().unwrap();
        assert_eq!(zoom.description.as_deref(), Some("Notes\nJoin: <https://acme.zoom.us/j/1?pwd=x>."));
        assert_eq!(zoom.organizer.as_deref(), Some("Boss"));
        assert_eq!(zoom.attendees, ["Ann"]);

        // Without knowing the user's addresses nothing is declined, and the user is listed.
        let all = to_occurrences(&events, "p", &member(), &[], ts("2026-10-22T00:00:00Z"), ts("2026-10-29T00:00:00Z"));
        assert_eq!(all.len(), 5);
        assert_eq!(all[0].details.as_ref().unwrap().attendees, ["me@proton.me", "Ann"]);
    }

    #[test]
    fn untitled_single_event() {
        let single = event(r#"{"ID":"ev","CalendarID":"cal","UID":"u"}"#, &[
            "BEGIN:VEVENT\r\nUID:u\r\nDTSTART:20261025T120000Z\r\nDTEND:20261025T123000Z\r\nLOCATION:Cafe\r\nEND:VEVENT",
        ]);
        let list = to_occurrences(&[single], "p", &member(), &[], ts("2026-10-22T00:00:00Z"), ts("2026-10-29T00:00:00Z"));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "(no title)");
        assert_eq!(list[0].location.as_deref(), Some("Cafe"));
        assert_eq!(list[0].url.as_deref(), Some("https://calendar.proton.me/event?Action=VIEW&EventID=ev&CalendarID=cal"));
    }
}
