//! Just enough iCalendar (RFC 5545) for the agenda: the VEVENT properties a
//! Proton event is split into, and expansion of recurring events into
//! occurrences within a window.

use std::str::FromStr;

use anyhow::{Context, Result};
use chrono::{DateTime, Days, NaiveDate, NaiveDateTime, TimeZone};
use rrule::{RRule, Tz, Unvalidated};

/// A DTSTART/DTEND/EXDATE/RECURRENCE-ID value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Time {
    /// `VALUE=DATE`: an all-day date.
    Date(NaiveDate),
    /// Local wall-clock time without a zone.
    Floating(NaiveDateTime),
    Utc(NaiveDateTime),
    Zoned(NaiveDateTime, chrono_tz::Tz),
}

impl Time {
    fn parse(value: &str, tzid: Option<&str>, is_date: bool) -> Option<Self> {
        let value = value.trim();
        if is_date || value.len() == 8 {
            return NaiveDate::parse_from_str(value, "%Y%m%d").ok().map(Time::Date);
        }
        if let Some(utc) = value.strip_suffix('Z') {
            return NaiveDateTime::parse_from_str(utc, "%Y%m%dT%H%M%S").ok().map(Time::Utc);
        }
        let local = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").ok()?;
        Some(match tzid.and_then(parse_tzid) {
            Some(tz) => Time::Zoned(local, tz),
            None => Time::Floating(local),
        })
    }

    /// As a zoned datetime for `rrule`. Dates and floating times are taken in
    /// the local zone, so all-day events start at local midnight.
    fn to_datetime(self) -> Option<DateTime<Tz>> {
        match self {
            Time::Date(date) => Tz::LOCAL.from_local_datetime(&date.and_time(chrono::NaiveTime::MIN)).earliest(),
            Time::Floating(local) => Tz::LOCAL.from_local_datetime(&local).earliest(),
            Time::Utc(utc) => Some(Tz::UTC.from_utc_datetime(&utc)),
            Time::Zoned(local, tz) => Tz::Tz(tz).from_local_datetime(&local).earliest(),
        }
    }

    pub fn timestamp(self) -> Option<i64> {
        self.to_datetime().map(|dt| dt.timestamp())
    }
}

/// Accepts IANA names, also with the `/`-prefix some producers emit.
fn parse_tzid(tzid: &str) -> Option<chrono_tz::Tz> {
    let tzid = tzid.trim().trim_matches('"').trim_start_matches('/');
    chrono_tz::Tz::from_str(tzid).ok()
}

/// The merged VEVENT of one Proton event.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VEvent {
    pub uid: Option<String>,
    pub summary: Option<String>,
    pub location: Option<String>,
    pub status: Option<String>,
    pub start: Option<Time>,
    pub end: Option<Time>,
    /// DURATION in seconds, used when DTEND is absent.
    pub duration: Option<i64>,
    pub rrule: Option<String>,
    pub exdates: Vec<Time>,
    pub recurrence_id: Option<Time>,
}

impl VEvent {
    /// Merges the VEVENT properties of every decrypted part (shared signed,
    /// shared encrypted, calendar parts); later parts win, as in WebClients.
    pub fn from_parts<'a>(parts: impl IntoIterator<Item = &'a str>) -> Self {
        let mut event = VEvent::default();
        for part in parts {
            event.merge(part);
        }
        event
    }

    fn merge(&mut self, text: &str) {
        // Properties outside VEVENT, or in a nested VALARM, are skipped.
        let mut depth = 0usize;
        let mut in_event = false;
        for line in unfold(text) {
            let Some(prop) = Property::parse(&line) else { continue };
            match prop.name.as_str() {
                "BEGIN" => {
                    if prop.value.eq_ignore_ascii_case("VEVENT") && depth == 0 {
                        in_event = true;
                    } else if in_event {
                        depth += 1;
                    }
                    continue;
                }
                "END" => {
                    if in_event && depth > 0 {
                        depth -= 1;
                    } else if prop.value.eq_ignore_ascii_case("VEVENT") {
                        in_event = false;
                    }
                    continue;
                }
                _ => {}
            }
            // Parts may also be bare property lists without BEGIN:VEVENT.
            if depth > 0 || (!in_event && text.contains("BEGIN:VEVENT")) {
                continue;
            }
            let tzid = prop.param("TZID");
            let is_date = prop.param("VALUE").is_some_and(|v| v.eq_ignore_ascii_case("DATE"));
            match prop.name.as_str() {
                "UID" => self.uid = Some(prop.value.clone()),
                "SUMMARY" => self.summary = Some(unescape(&prop.value)),
                "LOCATION" => self.location = Some(unescape(&prop.value)),
                "STATUS" => self.status = Some(prop.value.to_ascii_uppercase()),
                "DTSTART" => self.start = Time::parse(&prop.value, tzid, is_date),
                "DTEND" => self.end = Time::parse(&prop.value, tzid, is_date),
                "DURATION" => self.duration = parse_duration(&prop.value),
                "RRULE" => self.rrule = Some(prop.value.clone()),
                "RECURRENCE-ID" => self.recurrence_id = Time::parse(&prop.value, tzid, is_date),
                "EXDATE" => self.exdates.extend(prop.value.split(',').filter_map(|v| Time::parse(v, tzid, is_date))),
                _ => {}
            }
        }
    }

    pub fn all_day(&self) -> bool {
        matches!(self.start, Some(Time::Date(_)))
    }

    pub fn cancelled(&self) -> bool {
        self.status.as_deref() == Some("CANCELLED")
    }
}

/// One occurrence as unix seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Occurrence {
    pub start: i64,
    pub end: i64,
}

/// Occurrences overlapping `[from, to)`. `skip` holds start times replaced by
/// separate RECURRENCE-ID events. A recurrence rule `rrule` cannot handle is
/// an error; the caller may fall back to the first occurrence.
pub fn occurrences(event: &VEvent, skip: &[i64], from: i64, to: i64) -> Result<Vec<Occurrence>> {
    let start_time = event.start.context("event has no DTSTART")?;
    let start = start_time.to_datetime().context("DTSTART does not exist in its time zone")?;
    let span = Span::of(event, start_time, start.timestamp());
    // Zero-length events count while their instant is inside the window.
    let overlaps = |o: &Occurrence| o.start < to && (o.end > from || (o.start == o.end && o.start >= from));

    let Some(rule) = &event.rrule else {
        let single = span.occurrence(start);
        return Ok(Vec::from_iter(Some(single).filter(overlaps)));
    };

    let rule = rule.trim().trim_start_matches("RRULE:");
    let rule: RRule<Unvalidated> = rule.parse().with_context(|| format!("RRULE {rule:?}"))?;
    let mut set = rule.build(start).with_context(|| format!("RRULE for start {start}"))?;
    for exdate in &event.exdates {
        if let Some(dt) = exdate.to_datetime() {
            set = set.exdate(dt);
        }
    }
    let after = Tz::UTC.timestamp_opt(from - span.longest(), 0).single().context("window start")?;
    let before = Tz::UTC.timestamp_opt(to, 0).single().context("window end")?;
    let result = set.after(after).before(before).all(1000);
    Ok(result.dates.into_iter()
        .filter(|dt| !skip.contains(&dt.timestamp()))
        .map(|dt| span.occurrence(dt))
        .filter(overlaps)
        .collect())
}

/// How long each occurrence lasts.
enum Span {
    /// All-day: whole local days, so DST changes keep occurrences at midnight.
    Days(u64),
    Seconds(i64),
}

impl Span {
    fn of(event: &VEvent, start: Time, start_ts: i64) -> Self {
        if let Time::Date(start_date) = start {
            let days = match event.end {
                Some(Time::Date(end)) => (end - start_date).num_days(),
                _ => event.duration.map_or(1, |secs| secs / 86_400),
            };
            return Span::Days(days.max(1) as u64);
        }
        let seconds = match event.end.and_then(Time::timestamp) {
            Some(end) => end - start_ts,
            None => event.duration.unwrap_or(0),
        };
        Span::Seconds(seconds.max(0))
    }

    fn occurrence(&self, start: DateTime<Tz>) -> Occurrence {
        let end = match self {
            Span::Seconds(secs) => start.timestamp() + secs,
            Span::Days(days) => start.date_naive().checked_add_days(Days::new(*days))
                .and_then(|date| Time::Date(date).timestamp())
                .unwrap_or(start.timestamp() + *days as i64 * 86_400),
        };
        Occurrence { start: start.timestamp(), end }
    }

    /// Upper bound in seconds, for finding occurrences that started earlier.
    fn longest(&self) -> i64 {
        match self {
            Span::Seconds(secs) => *secs,
            Span::Days(days) => *days as i64 * 86_400 + 3_600,
        }
    }
}

/// Lines with RFC 5545 folding undone.
fn unfold(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        match (raw.strip_prefix(' ').or_else(|| raw.strip_prefix('\t')), lines.last_mut()) {
            (Some(rest), Some(last)) => last.push_str(rest),
            _ if raw.is_empty() => {}
            _ => lines.push(raw.to_owned()),
        }
    }
    lines
}

struct Property {
    name: String,
    params: Vec<(String, String)>,
    value: String,
}

impl Property {
    /// `NAME;PARAM=V;PARAM="V:X":VALUE`
    fn parse(line: &str) -> Option<Self> {
        let mut in_quotes = false;
        let colon = line.char_indices().find(|&(_, c)| {
            if c == '"' {
                in_quotes = !in_quotes;
            }
            c == ':' && !in_quotes
        })?.0;
        let (head, value) = (&line[..colon], &line[colon + 1..]);
        let mut pieces = head.split(';');
        let name = pieces.next()?.trim().to_ascii_uppercase();
        let params = pieces
            .filter_map(|p| p.split_once('='))
            .map(|(k, v)| (k.trim().to_ascii_uppercase(), v.trim().trim_matches('"').to_owned()))
            .collect();
        Some(Self { name, params, value: value.to_owned() })
    }

    fn param(&self, name: &str) -> Option<&str> {
        self.params.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// TEXT value escapes: `\\`, `\;`, `\,`, `\n`.
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push(' '),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out.trim().to_owned()
}

/// `[+-]P[nW][nD][T[nH][nM][nS]]` in seconds.
fn parse_duration(value: &str) -> Option<i64> {
    let value = value.trim();
    let (sign, rest) = match value.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, value.strip_prefix('+').unwrap_or(value)),
    };
    let rest = rest.strip_prefix('P')?;
    let (mut total, mut number, mut in_time) = (0i64, String::new(), false);
    for c in rest.chars() {
        match c {
            '0'..='9' => number.push(c),
            'T' => in_time = true,
            unit => {
                let n: i64 = number.parse().ok()?;
                number.clear();
                total += n * match (unit, in_time) {
                    ('W', false) => 604_800,
                    ('D', false) => 86_400,
                    ('H', true) => 3_600,
                    ('M', true) => 60,
                    ('S', true) => 1,
                    _ => return None,
                };
            }
        }
    }
    Some(sign * total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> i64 {
        DateTime::parse_from_rfc3339(s).unwrap().timestamp()
    }

    fn local_midnight(y: i32, m: u32, d: u32) -> i64 {
        Time::Date(NaiveDate::from_ymd_opt(y, m, d).unwrap()).timestamp().unwrap()
    }

    // Shared signed part (clear) and shared encrypted part, as Proton splits them.
    const SIGNED: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:abc@proton.me\r\nDTSTAMP:20260101T000000Z\r\n\
        DTSTART;TZID=Europe/Riga:20261020T090000\r\nDTEND;TZID=Europe/Riga:20261020T093000\r\n\
        RRULE:FREQ=WEEKLY;BYDAY=TU,TH;UNTIL=20261231T215959Z\r\nEXDATE;TZID=Europe/Riga:20261027T090000\r\n\
        SEQUENCE:0\r\nEND:VEVENT\r\nEND:VCALENDAR";
    const ENCRYPTED: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:abc@proton.me\r\nDTSTAMP:20260101T000000Z\r\n\
        SUMMARY:Stand-up\\, team\r\nLOCATION:Room 4\\;\r\n  floor 2\r\nDESCRIPTION:Daily\\nsync\r\n\
        BEGIN:VALARM\r\nSUMMARY:not this one\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR";

    #[test]
    fn merges_parts() {
        let event = VEvent::from_parts([SIGNED, ENCRYPTED]);
        assert_eq!(event.uid.as_deref(), Some("abc@proton.me"));
        assert_eq!(event.summary.as_deref(), Some("Stand-up, team"));
        assert_eq!(event.location.as_deref(), Some("Room 4; floor 2"));
        let riga = chrono_tz::Europe::Riga;
        let start = NaiveDate::from_ymd_opt(2026, 10, 20).unwrap().and_hms_opt(9, 0, 0).unwrap();
        assert_eq!(event.start, Some(Time::Zoned(start, riga)));
        assert_eq!(event.exdates.len(), 1);
        assert!(event.rrule.as_deref().unwrap().starts_with("FREQ=WEEKLY"));
        assert!(!event.all_day());
    }

    #[test]
    fn expands_weekly_with_exdate_and_dst() {
        let event = VEvent::from_parts([SIGNED, ENCRYPTED]);
        // Window 2026-10-21 .. 2026-11-04 (DST in Riga ends 2026-10-25).
        let from = ts("2026-10-21T00:00:00Z");
        let to = ts("2026-11-04T00:00:00Z");
        let starts: Vec<i64> = occurrences(&event, &[], from, to).unwrap().iter().map(|o| o.start).collect();
        assert_eq!(starts, [
            ts("2026-10-22T09:00:00+03:00"),
            // 2026-10-27 is excluded by EXDATE.
            ts("2026-10-29T09:00:00+02:00"),
            ts("2026-11-03T09:00:00+02:00"),
        ]);
        let first = occurrences(&event, &[], from, to).unwrap()[0];
        assert_eq!(first.end - first.start, 30 * 60);
    }

    #[test]
    fn skips_overridden_occurrences_and_respects_until() {
        let event = VEvent::from_parts([SIGNED]);
        let moved = ts("2026-10-22T09:00:00+03:00");
        let all = occurrences(&event, &[moved], ts("2026-10-21T00:00:00Z"), ts("2027-06-01T00:00:00Z")).unwrap();
        assert!(!all.iter().any(|o| o.start == moved));
        assert!(all.iter().all(|o| o.start <= ts("2026-12-31T21:59:59Z")));
        assert_eq!(all.last().unwrap().start, ts("2026-12-31T09:00:00+02:00"));
    }

    #[test]
    fn ongoing_occurrence_is_included() {
        let event = VEvent::from_parts([SIGNED]);
        let during = ts("2026-10-22T09:10:00+03:00");
        let found = occurrences(&event, &[], during, during + 3_600).unwrap();
        assert_eq!(found.first().map(|o| o.start), Some(ts("2026-10-22T09:00:00+03:00")));
    }

    #[test]
    fn all_day_events_span_local_days() {
        let text = "BEGIN:VEVENT\r\nUID:x\r\nDTSTART;VALUE=DATE:20261024\r\nDTEND;VALUE=DATE:20261026\r\nSUMMARY:Trip\r\nEND:VEVENT";
        let event = VEvent::from_parts([text]);
        assert!(event.all_day());
        let found = occurrences(&event, &[], local_midnight(2026, 10, 1), local_midnight(2026, 11, 1)).unwrap();
        assert_eq!(found, [Occurrence { start: local_midnight(2026, 10, 24), end: local_midnight(2026, 10, 26) }]);
    }

    #[test]
    fn yearly_all_day_with_date_until_and_count() {
        let text = "BEGIN:VEVENT\r\nUID:bday\r\nDTSTART;VALUE=DATE:20200315\r\nRRULE:FREQ=YEARLY;UNTIL=20280315\r\nEND:VEVENT";
        let event = VEvent::from_parts([text]);
        let found = occurrences(&event, &[], local_midnight(2026, 1, 1), local_midnight(2030, 1, 1)).unwrap();
        let starts: Vec<i64> = found.iter().map(|o| o.start).collect();
        assert_eq!(starts, [local_midnight(2026, 3, 15), local_midnight(2027, 3, 15), local_midnight(2028, 3, 15)]);
        assert_eq!(found[0].end, local_midnight(2026, 3, 16));

        let counted = "BEGIN:VEVENT\r\nDTSTART:20261001T080000Z\r\nDURATION:PT45M\r\nRRULE:FREQ=DAILY;COUNT=3\r\nEND:VEVENT";
        let event = VEvent::from_parts([counted]);
        let found = occurrences(&event, &[], ts("2026-01-01T00:00:00Z"), ts("2027-01-01T00:00:00Z")).unwrap();
        assert_eq!(found.len(), 3);
        assert_eq!(found[2], Occurrence { start: ts("2026-10-03T08:00:00Z"), end: ts("2026-10-03T08:45:00Z") });
    }

    #[test]
    fn single_events_outside_the_window_are_dropped() {
        let text = "BEGIN:VEVENT\r\nDTSTART:20261001T080000Z\r\nDTEND:20261001T090000Z\r\nEND:VEVENT";
        let event = VEvent::from_parts([text]);
        assert!(occurrences(&event, &[], ts("2026-10-02T00:00:00Z"), ts("2026-10-09T00:00:00Z")).unwrap().is_empty());
        assert_eq!(occurrences(&event, &[], ts("2026-10-01T08:30:00Z"), ts("2026-10-09T00:00:00Z")).unwrap().len(), 1);
    }

    #[test]
    fn recurrence_id_and_cancellation() {
        let text = "BEGIN:VEVENT\r\nUID:abc\r\nRECURRENCE-ID;TZID=Europe/Riga:20261022T090000\r\n\
            DTSTART;TZID=Europe/Riga:20261022T110000\r\nDTEND;TZID=Europe/Riga:20261022T113000\r\nSTATUS:cancelled\r\nEND:VEVENT";
        let event = VEvent::from_parts([text]);
        assert_eq!(event.recurrence_id.and_then(Time::timestamp), Some(ts("2026-10-22T09:00:00+03:00")));
        assert!(event.cancelled());
    }

    #[test]
    fn parses_values() {
        assert_eq!(parse_duration("PT1H30M"), Some(5_400));
        assert_eq!(parse_duration("P1W"), Some(604_800));
        assert_eq!(parse_duration("-P1DT2S"), Some(-86_402));
        assert_eq!(parse_duration("1H"), None);
        assert_eq!(parse_tzid("/Europe/Berlin"), Some(chrono_tz::Europe::Berlin));
        let prop = Property::parse("ATTENDEE;CN=\"Doe: J\";ROLE=REQ:mailto:j@example.com").unwrap();
        assert_eq!(prop.param("CN"), Some("Doe: J"));
        assert_eq!(prop.value, "mailto:j@example.com");
        // An unknown TZID degrades to local time rather than failing.
        assert!(matches!(Time::parse("20261022T090000", Some("Mars/Olympus"), false), Some(Time::Floating(_))));
    }
}
