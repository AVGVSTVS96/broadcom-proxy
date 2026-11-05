use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use serde::Serialize;

/// Writes `bytes` to a temporary file beside `path` and renames it over `path`, so a reader
/// sees the old file or the new one, never half of one.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    static WRITES: AtomicU64 = AtomicU64::new(0);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp.{}.{}", std::process::id(), WRITES.fetch_add(1, Ordering::Relaxed)));
    let mut file = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&tmp, path)
}

pub fn write_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    write_atomic(path, &serde_json::to_vec_pretty(value).map_err(io::Error::other)?)
}

/// Appends `value` as one line to the ndjson file `name` in `dir`.
pub fn append(dir: &Path, name: &str, value: &impl Serialize) {
    let mut line = serde_json::to_vec(value).expect("records serialize");
    line.push(b'\n');
    let result = fs::create_dir_all(dir)
        .and_then(|()| OpenOptions::new().append(true).create(true).mode(0o600).open(dir.join(name))?.write_all(&line));
    if let Err(err) = result {
        eprintln!("append {name}: {err}");
    }
}

pub fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

/// A time as the records write it: RFC 3339 in UTC.
pub fn utc(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms).map(|t| t.to_string()).unwrap_or_default()
}

/// A time as credential files write it: RFC 3339 with the local offset.
pub fn local(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%Y-%m-%dT%H:%M:%S%:z").to_string())
        .unwrap_or_default()
}

/// A time as people read it in the reset plan: "Mon Jan 2 3:04pm", local.
pub fn when(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%a %b %-d %-I:%M%P").to_string())
        .unwrap_or_default()
}

pub fn parse_ms(text: &str) -> Option<i64> {
    text.parse::<jiff::Timestamp>().ok().map(jiff::Timestamp::as_millisecond)
}
