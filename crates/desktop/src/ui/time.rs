//! Workspace-local session timestamps; no locale or wall-clock dependency in tests.
use std::time::SystemTime;
pub fn relative(now: SystemTime, modified: SystemTime) -> String {
    let seconds = now.duration_since(modified).unwrap_or_default().as_secs();
    match seconds {
        0..60 => "Just now".into(),
        60..3600 => format!("{} min ago", seconds / 60),
        3600..86400 => format!("{} h ago", seconds / 3600),
        86400..172800 => "Yesterday".into(),
        172800..604800 => format!("{} days ago", seconds / 86400),
        604800..1209600 => "1 week ago".into(),
        1209600..2592000 => format!("{} weeks ago", seconds / 604800),
        2592000..5184000 => "1 month ago".into(),
        _ => format!("{} months ago", seconds / 2592000),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};
    #[test]
    fn every_bucket_and_clock_reversal() {
        let now = UNIX_EPOCH + Duration::from_secs(100_000_000);
        for (seconds, expected) in [
            (0, "Just now"),
            (59, "Just now"),
            (60, "1 min ago"),
            (720, "12 min ago"),
            (3599, "59 min ago"),
            (3600, "1 h ago"),
            (10800, "3 h ago"),
            (86400, "Yesterday"),
            (345600, "4 days ago"),
            (604800, "1 week ago"),
            (1814400, "3 weeks ago"),
            (12960000, "5 months ago"),
        ] {
            assert_eq!(relative(now, now - Duration::from_secs(seconds)), expected);
        }
        assert_eq!(relative(now, now + Duration::from_secs(100)), "Just now");
    }
}
