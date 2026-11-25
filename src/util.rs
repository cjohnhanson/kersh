//! Small helpers with no home of their own.

/// The current time as RFC 3339 in UTC, to the millisecond.
#[must_use]
pub fn now_rfc3339() -> String {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    rfc3339(since)
}

/// A time since the Unix epoch as RFC 3339 in UTC, to the millisecond.
#[must_use]
pub fn rfc3339(since_epoch: std::time::Duration) -> String {
    let secs = since_epoch.as_secs();
    let millis = since_epoch.subsec_millis();
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rest = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

/// The proleptic Gregorian date of a day count since 1970-01-01, by
/// Howard Hinnant's `civil_from_days` algorithm.
const fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

/// 12 random hex characters, for a session id.
#[must_use]
pub fn short_id() -> String {
    nonce().chars().take(12).collect()
}

/// A per-run nonce: 16 random bytes as hex. It tags the untrusted context
/// marker so the content, written before the run, cannot forge the tag.
#[must_use]
pub fn nonce() -> String {
    let mut bytes = [0u8; 16];
    // A failure here is not worth aborting a run over; fall back to a
    // fixed but unusual tag. The threat is untrusted content guessing the
    // tag, and content is authored before the run either way.
    if getrandom::fill(&mut bytes).is_err() {
        return "kersh-nonce-fallback".to_owned();
    }
    let mut hex = String::with_capacity(32);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_formats_known_instants() {
        assert_eq!(
            rfc3339(std::time::Duration::from_secs(0)),
            "1970-01-01T00:00:00.000Z"
        );
        // 2026-11-24T10:20:30.5Z
        assert_eq!(
            rfc3339(std::time::Duration::from_millis(1_795_515_630_500)),
            "2026-11-24T10:20:30.500Z"
        );
        // A leap day.
        assert_eq!(
            rfc3339(std::time::Duration::from_secs(951_782_400)),
            "2000-02-29T00:00:00.000Z"
        );
    }
}
