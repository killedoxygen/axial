use chrono::{SecondsFormat, Utc};

pub fn timestamp_utc() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}
