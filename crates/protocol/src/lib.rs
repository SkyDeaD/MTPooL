//! API types shared by the registrar and the agent.

pub mod agent;
pub mod problem;
pub mod public;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// A Unix time in milliseconds as an RFC 3339 string in UTC (`2026-11-03T12:00:00Z`). A value
/// outside the range the calendar covers gives the Unix epoch rather than an error: a
/// timestamp in a public document is never a reason to fail a request.
pub fn rfc3339(unix_ms: i64) -> String {
    let nanos = i128::from(unix_ms) * 1_000_000;
    OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

/// The inverse of [`rfc3339`]: an RFC 3339 string (any offset) as Unix milliseconds. `None`
/// when the text is not a timestamp or lies outside the range of `i64` milliseconds.
pub fn parse_rfc3339(text: &str) -> Option<i64> {
    let parsed = OffsetDateTime::parse(text, &Rfc3339).ok()?;
    i64::try_from(parsed.unix_timestamp_nanos().div_euclid(1_000_000)).ok()
}

#[cfg(test)]
mod tests {
    use super::{parse_rfc3339, rfc3339};

    #[test]
    fn formats_utc_with_optional_milliseconds() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_700_000_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(rfc3339(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
        assert_eq!(rfc3339(-1_000), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn parses_what_it_formats_and_other_offsets() {
        for ms in [0, 1_700_000_000_123, -1_000] {
            assert_eq!(parse_rfc3339(&rfc3339(ms)), Some(ms));
        }
        assert_eq!(
            parse_rfc3339("2023-11-15T00:13:20+02:00"),
            Some(1_700_000_000_000)
        );
        assert_eq!(parse_rfc3339("yesterday"), None);
        assert_eq!(parse_rfc3339(""), None);
    }

    #[test]
    fn out_of_range_gives_the_epoch() {
        assert_eq!(rfc3339(i64::MAX), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(i64::MIN), "1970-01-01T00:00:00Z");
    }
}
