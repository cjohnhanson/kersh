//! Small helpers with no home of their own.

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
