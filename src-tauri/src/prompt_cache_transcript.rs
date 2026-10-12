//! Bounded, display-only fallback for Claude builds without prompt-cache metadata.
//!
//! Reference (read only): chatworkspace/cache-research-20261011/
//! ilia-pluzhnikov--claude-code-statusline/statusline.js, prompt-cache section.
//! Official schema: https://code.claude.com/docs/en/statusline#prompt-cache-fields
//! Usage buckets: https://platform.claude.com/docs/en/build-with-claude/prompt-caching
//! No transcript text, path, or account data is copied into the enrichment.

use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    path::{Component, Path},
};

const MAX_TAIL_BYTES: usize = 64 * 1024;
const MAX_LINE_BYTES: usize = 64 * 1024;
const SOURCE: &str = "claude_transcript.usage.timestamp";

/// Clone the statusline, enriching only absent/invalid cache metadata when the
/// latest main-conversation usage can be verified. Any uncertainty leaves it
/// unchanged. `now` is only a future-timestamp guard, never the expiry anchor.
pub fn enrich_statusline(data: &Value) -> Value {
    enrich_at(data, chrono::Utc::now().timestamp())
}

fn enrich_at(data: &Value, now: i64) -> Value {
    let mut enriched = data.clone();
    let Some(observation) = observe(data, now) else {
        return enriched;
    };
    enriched["prompt_cache"] = json!({
        "caching_observed": true,
        "ttl": if observation.ttl_seconds == 3600 { "1h" } else { "5m" },
        "expires_at": observation.at + observation.ttl_seconds,
        "source": SOURCE,
    });
    enriched
}

// Preserve even partial valid official metadata. In particular, a reported
// cold cache or a null expiry must never be replaced by older transcript data.
fn authoritative(data: &Value) -> bool {
    let Some(cache) = data.get("prompt_cache").and_then(Value::as_object) else {
        return false;
    };
    cache.get("expires_at").is_some_and(Value::is_null)
        || cache.get("caching_observed").is_some_and(Value::is_boolean)
        || cache
            .get("ttl")
            .and_then(Value::as_str)
            .is_some_and(|ttl| matches!(ttl, "5m" | "1h"))
        || cache
            .get("expires_at")
            .and_then(Value::as_i64)
            .is_some_and(|at| at >= 0)
}

fn identifier(value: Option<&Value>, max: usize) -> Option<&str> {
    value.and_then(Value::as_str).filter(|s| {
        !s.is_empty()
            && s.len() <= max
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
            && !matches!(*s, "." | "..")
    })
}

fn count(value: &Value, key: &str) -> Option<i64> {
    value.get(key)?.as_i64().filter(|n| *n >= 0)
}

#[derive(Clone, Copy, Debug)]
struct Usage {
    read: i64,
    write: i64,
    input: Option<i64>,
}

impl Usage {
    fn parse(value: &Value) -> Option<Self> {
        let input = match value.get("input_tokens") {
            None => None,
            Some(_) => Some(count(value, "input_tokens")?),
        };
        Some(Self {
            read: count(value, "cache_read_input_tokens")?,
            write: count(value, "cache_creation_input_tokens")?,
            input,
        })
    }

    fn positive(self) -> bool {
        self.read > 0 || self.write > 0
    }

    fn matches(self, transcript: Self) -> bool {
        self.read == transcript.read
            && self.write == transcript.write
            && self.input.is_none_or(|n| transcript.input == Some(n))
    }
}

#[derive(Debug)]
struct Observation {
    at: i64,
    ttl_seconds: i64,
}

fn observe(data: &Value, now: i64) -> Option<Observation> {
    if !data.is_object() || authoritative(data) {
        return None;
    }
    let session = identifier(data.get("session_id"), 128)?;
    let model = identifier(data.pointer("/model/id"), 256)?;
    // Null/missing usage occurs before the first request and after /compact.
    // Require the current snapshot rather than resurrecting previous usage.
    let current = Usage::parse(data.pointer("/context_window/current_usage")?)?;
    if !current.positive() {
        return None;
    }
    let path = data.get("transcript_path")?.as_str()?;
    let tail = read_tail(Path::new(path), session)?;
    observe_tail(&tail, session, model, current, now)
}

fn local_path(path: &Path, session: &str) -> bool {
    let Some(text) = path.to_str() else {
        return false;
    };
    if text.len() > 4096
        || text.chars().any(char::is_control)
        || !path.is_absolute()
        || text.starts_with("\\\\")
        || text.starts_with("//")
        || path.components().any(|c| matches!(c, Component::ParentDir))
        || path.extension().and_then(|s| s.to_str()) != Some("jsonl")
        || path.file_stem().and_then(|s| s.to_str()) != Some(session)
    {
        return false;
    }
    #[cfg(windows)]
    {
        use std::path::Prefix;
        // Exclude UNC, device, verbatim and drive-relative paths/NTFS streams.
        if !matches!(path.components().next(), Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::Disk(_)))
            || text.get(2..).is_none_or(|rest| rest.contains(':'))
        {
            return false;
        }
    }
    true
}

fn plain_metadata(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Junctions and other reparse points can redirect an apparently local path.
        if metadata.file_attributes() & 0x400 != 0 {
            return false;
        }
    }
    true
}

fn read_tail(path: &Path, session: &str) -> Option<Vec<u8>> {
    if !local_path(path, session) {
        return None;
    }
    for ancestor in path.ancestors() {
        if !plain_metadata(&fs::symlink_metadata(ancestor).ok()?) {
            return None;
        }
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Inspect the opened file itself if the final component is replaced.
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let mut file: File = options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || !plain_metadata(&metadata) {
        return None;
    }
    let tail = bounded_tail(&mut file, metadata.len())?;
    let after = file.metadata().ok()?;
    if after.len() != metadata.len() || after.modified().ok()? != metadata.modified().ok()? {
        return None; // A concurrently rewritten/appended snapshot is ambiguous.
    }
    Some(tail)
}

// No look-behind byte and no read-to-end: even a growing file costs at most
// 64 KiB of transcript I/O. Discard the first line of a clipped window because
// its start cannot be verified without reading outside that strict budget.
fn bounded_tail(reader: &mut (impl Read + Seek), length: u64) -> Option<Vec<u8>> {
    let start = length.saturating_sub(MAX_TAIL_BYTES as u64);
    let mut bytes = vec![0; (length - start) as usize];
    reader.seek(SeekFrom::Start(start)).ok()?;
    reader.read_exact(&mut bytes).ok()?;
    if start != 0 {
        let first_newline = bytes.iter().position(|b| *b == b'\n')?;
        bytes.drain(..=first_newline);
    }
    Some(bytes)
}

fn compact(record: &Value) -> bool {
    matches!(
        record.get("type").and_then(Value::as_str),
        Some("summary" | "compact_boundary")
    ) || record.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
        || record.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
        || record.get("compactMetadata").is_some_and(|v| !v.is_null())
        || (record.get("type").and_then(Value::as_str) == Some("user")
            && record
                .pointer("/message/content")
                .and_then(Value::as_str)
                .is_some_and(|s| {
                    matches!(s.trim(), "/compact" | "/clear")
                        || s.contains("<command-name>/compact</command-name>")
                        || s.contains("<command-name>/clear</command-name>")
                }))
}

fn same_session(record: &Value, session: &str) -> bool {
    ["sessionId", "session_id"].into_iter().all(|key| {
        record
            .get(key)
            .is_none_or(|value| value.as_str() == Some(session))
    })
}

fn request_at(record: &Value, now: i64) -> Option<i64> {
    let text = record.get("timestamp")?.as_str()?;
    if text.len() > 64 {
        return None;
    }
    let parsed = chrono::DateTime::parse_from_rfc3339(text).ok()?;
    let at = parsed.timestamp();
    (at >= 0 && at <= now && at.checked_add(3600).is_some()).then_some(at)
}

fn write_ttl(usage: &Value, write: i64) -> Option<i64> {
    let buckets = usage.get("cache_creation")?.as_object()?;
    let bucket = |key: &str| -> Option<Option<i64>> {
        match buckets.get(key) {
            None => Some(None),
            Some(value) => Some(Some(value.as_i64().filter(|n| *n >= 0)?)),
        }
    };
    let hour = bucket("ephemeral_1h_input_tokens")?;
    let five = bucket("ephemeral_5m_input_tokens")?;
    let total = hour.unwrap_or(0).checked_add(five.unwrap_or(0))?;
    if write <= 0 || total != write {
        return None;
    }
    // A single bar covers the observed prefix. Mixed lifetime segments must
    // use the shorter deadline, otherwise the 5m suffix could expire silently.
    if five.is_some_and(|n| n > 0) {
        Some(300)
    } else if hour.is_some_and(|n| n > 0) {
        Some(3600)
    } else if hour == Some(0) || five == Some(write) {
        Some(300)
    } else {
        None // No known bucket: never assume a default lifetime.
    }
}

fn observe_tail(
    bytes: &[u8],
    session: &str,
    model: &str,
    current: Usage,
    now: i64,
) -> Option<Observation> {
    let mut latest = None;
    let mut newer_at = now;
    for line in bytes.rsplit(|b| *b == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if line.len() > MAX_LINE_BYTES {
            return None;
        }
        // Malformed/truncated rows are barriers, not permission to use an old hit.
        let record: Value = serde_json::from_slice(line).ok()?;
        if !record.is_object() || compact(&record) {
            return None;
        }
        if record.get("isSidechain").is_some_and(|v| !v.is_boolean()) {
            return None;
        }
        if record.get("isSidechain").and_then(Value::as_bool) == Some(true)
            || record.get("agentId").is_some_and(|v| !v.is_null())
        {
            continue;
        }
        if !same_session(&record, session) {
            return None;
        }
        if record.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if record.get("sessionId").is_none() && record.get("session_id").is_none() {
            return None;
        }
        if record.pointer("/message/model").and_then(Value::as_str) != Some(model) {
            return None;
        }
        if record
            .pointer("/message/role")
            .is_some_and(|v| v.as_str() != Some("assistant"))
        {
            return None;
        }
        let at = request_at(&record, now)?;
        if at > newer_at {
            return None;
        }
        newer_at = at;
        let raw_usage = record.pointer("/message/usage")?;
        let usage = Usage::parse(raw_usage)?;
        if !usage.positive() || (latest.is_none() && !current.matches(usage)) {
            return None;
        }
        let latest_at = *latest.get_or_insert(at);
        if usage.write > 0 {
            return Some(Observation {
                at: latest_at,
                ttl_seconds: write_ttl(raw_usage, usage.write)?,
            });
        }
        // Read-only hits have no TTL bucket. Search only this uninterrupted
        // session/model segment for its most recent write; expiry still uses
        // the latest request timestamp. No writer inside the window = unknown.
        if raw_usage.get("cache_creation").is_some_and(|b| {
            !b.is_object()
                || ["ephemeral_1h_input_tokens", "ephemeral_5m_input_tokens"]
                    .into_iter()
                    .any(|key| b.get(key).is_some_and(|v| v.as_i64() != Some(0)))
        }) {
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mixed_cache_lifetimes_use_the_earliest_segment_expiry() {
        let usage = json!({"cache_creation":{"ephemeral_1h_input_tokens":100,"ephemeral_5m_input_tokens":200}});
        assert_eq!(write_ttl(&usage, 300), Some(300));
        assert_eq!(
            write_ttl(
                &json!({"cache_creation":{"ephemeral_1h_input_tokens":40}}),
                100
            ),
            None
        );
        assert!(authoritative(&json!({"prompt_cache":{"expires_at":null}})));
    }
    use std::{
        io::Cursor,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    const AT: i64 = 1_800_000_000;
    const SESSION: &str = "fixture-session";
    const MODEL: &str = "claude-fixture";
    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        dir: PathBuf,
        path: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "switcher-transcript-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&dir).unwrap();
            // macOS exposes its temporary directory through the /var symlink.
            // Fixtures must use an ordinary path to exercise the production
            // guard without relaxing its rejection of symlink ancestors.
            #[cfg(unix)]
            let dir = fs::canonicalize(dir).unwrap();
            let path = dir.join(format!("{SESSION}.jsonl"));
            Self { dir, path }
        }

        fn write(&self, records: &[Value]) {
            let mut bytes = Vec::new();
            for record in records {
                serde_json::to_writer(&mut bytes, record).unwrap();
                bytes.push(b'\n');
            }
            fs::write(&self.path, bytes).unwrap();
        }

        fn payload(&self, usage: &Value) -> Value {
            json!({"session_id":SESSION, "model":{"id":MODEL},
                "transcript_path":self.path, "context_window":{"current_usage":usage},
                "unrelated":{"keep":true}})
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.dir).unwrap();
        }
    }

    fn usage(read: i64, write: i64, hour: i64) -> Value {
        json!({"input_tokens":7,"cache_read_input_tokens":read,
            "cache_creation_input_tokens":write,"cache_creation":{
                "ephemeral_1h_input_tokens":hour,"ephemeral_5m_input_tokens":write-hour}})
    }

    fn assistant(at: i64, usage: &Value) -> Value {
        json!({"type":"assistant","sessionId":SESSION,"isSidechain":false,
            "timestamp":chrono::DateTime::from_timestamp(at, 0).unwrap().to_rfc3339(),
            "message":{"role":"assistant","model":MODEL,"usage":usage,
                "content":[{"type":"text","text":"DO_NOT_COPY_TRANSCRIPT"}]}})
    }

    #[test]
    fn five_minute_and_hour_expiry_use_request_time_and_preserve_input() {
        let fixture = Fixture::new();
        for (hour, ttl, seconds) in [(0, "5m", 300), (30, "1h", 3600)] {
            let usage = usage(120, 30, hour);
            fixture.write(&[assistant(AT, &usage)]);
            let data = fixture.payload(&usage);
            let original = data.clone();
            let enriched = enrich_at(&data, AT + 60);
            assert_eq!(
                enriched["prompt_cache"],
                json!({"caching_observed":true,
                "ttl":ttl,"expires_at":AT+seconds,"source":SOURCE})
            );
            assert_eq!(data, original);
            let mut without_cache = enriched.clone();
            without_cache
                .as_object_mut()
                .unwrap()
                .remove("prompt_cache");
            assert_eq!(without_cache, original);
            assert_eq!(enrich_at(&data, AT + 10_000), enriched); // expired stays expired
            assert_eq!(enrich_at(&enriched, AT + 10_000), enriched);
            assert!(!enriched["prompt_cache"]
                .to_string()
                .contains("DO_NOT_COPY_TRANSCRIPT"));
        }
    }

    #[test]
    fn official_metadata_including_cold_and_partial_is_authoritative() {
        let fixture = Fixture::new();
        let usage = usage(120, 30, 30);
        fixture.write(&[assistant(AT, &usage)]);
        for cache in [
            json!({"caching_observed":false,"expires_at":null}),
            json!({"caching_observed":true,"ttl":"5m","expires_at":AT+1}),
            json!({"ttl":"1h"}),
            json!({"expires_at":AT}),
            json!({"caching_observed":true,"ttl":"bad"}),
        ] {
            let mut data = fixture.payload(&usage);
            data["prompt_cache"] = cache;
            assert_eq!(enrich_at(&data, AT + 60), data);
        }
    }

    #[test]
    fn absent_null_empty_or_malformed_cache_can_be_enriched() {
        let fixture = Fixture::new();
        let usage = usage(120, 30, 0);
        fixture.write(&[assistant(AT, &usage)]);
        for cache in [
            Value::Null,
            json!({}),
            json!("bad"),
            json!({"ttl":"bad","caching_observed":"true","expires_at":"now"}),
        ] {
            let mut data = fixture.payload(&usage);
            data["prompt_cache"] = cache;
            assert_eq!(enrich_at(&data, AT + 60)["prompt_cache"]["source"], SOURCE);
        }
    }

    #[test]
    fn read_only_hit_uses_latest_timestamp_and_same_segment_writer_bucket() {
        let fixture = Fixture::new();
        let write = usage(0, 100, 100);
        let hit = usage(100, 0, 0);
        fixture.write(&[
            assistant(AT - 60, &write),
            assistant(AT, &hit),
            json!({"type":"user","sessionId":SESSION,"message":{"content":"next"}}),
        ]);
        let enriched = enrich_at(&fixture.payload(&hit), AT + 60);
        assert_eq!(enriched["prompt_cache"]["ttl"], "1h");
        assert_eq!(enriched["prompt_cache"]["expires_at"], AT + 3600);
    }

    #[test]
    fn read_only_usage_without_known_bucket_never_defaults_to_five_minutes() {
        let fixture = Fixture::new();
        let hit = usage(100, 0, 0);
        for mut write in [usage(0, 100, 0), usage(0, 100, 100)] {
            write.as_object_mut().unwrap().remove("cache_creation");
            fixture.write(&[assistant(AT - 60, &write), assistant(AT, &hit)]);
            let data = fixture.payload(&hit);
            assert_eq!(enrich_at(&data, AT + 60), data);
        }
        fixture.write(&[assistant(AT, &hit)]);
        let data = fixture.payload(&hit);
        assert_eq!(enrich_at(&data, AT + 60), data);
    }

    #[test]
    fn session_model_compaction_and_cold_rows_are_search_barriers() {
        let fixture = Fixture::new();
        let write = usage(0, 100, 100);
        let hit = usage(100, 0, 0);
        let mut other_model = assistant(AT - 30, &hit);
        other_model["message"]["model"] = json!("another-model");
        let mut other_session = assistant(AT - 30, &hit);
        other_session["sessionId"] = json!("another-session");
        let barriers = [
            other_model,
            other_session,
            json!({"type":"system","sessionId":SESSION,"subtype":"compact_boundary"}),
            json!({"type":"summary","summary":"compacted"}),
            json!({"type":"user","sessionId":SESSION,"isCompactSummary":true}),
            json!({"type":"user","sessionId":SESSION,"message":{"content":"<command-name>/compact</command-name>"}}),
            assistant(AT - 30, &usage(0, 0, 0)),
        ];
        for barrier in barriers {
            fixture.write(&[
                assistant(AT - 60, &write),
                barrier.clone(),
                assistant(AT, &hit),
            ]);
            let data = fixture.payload(&hit);
            assert_eq!(enrich_at(&data, AT + 60), data, "barrier: {barrier}");
            fixture.write(&[assistant(AT - 60, &write), barrier]);
            assert_eq!(enrich_at(&data, AT + 60), data);
        }
    }

    #[test]
    fn new_write_after_model_change_or_compact_establishes_its_own_bucket() {
        let fixture = Fixture::new();
        let mut old = assistant(AT - 60, &usage(0, 100, 100));
        old["message"]["model"] = json!("another-model");
        let fresh = usage(0, 30, 0);
        fixture.write(&[
            old,
            json!({"type":"system","subtype":"compact_boundary"}),
            assistant(AT, &fresh),
        ]);
        assert_eq!(
            enrich_at(&fixture.payload(&fresh), AT + 60)["prompt_cache"]["ttl"],
            "5m"
        );
    }

    #[test]
    fn null_missing_cold_or_disagreeing_current_usage_never_resurrects_cache() {
        let fixture = Fixture::new();
        let cached = usage(120, 30, 30);
        fixture.write(&[assistant(AT, &cached)]);
        for current in [
            Value::Null,
            json!({}),
            usage(0, 0, 0),
            usage(121, 30, 30),
            json!({"cache_read_input_tokens":120,
                "cache_creation_input_tokens":30,"input_tokens":8}),
        ] {
            let data = fixture.payload(&current);
            assert_eq!(enrich_at(&data, AT + 60), data);
        }
        let mut data = fixture.payload(&cached);
        data["context_window"]
            .as_object_mut()
            .unwrap()
            .remove("current_usage");
        assert_eq!(enrich_at(&data, AT + 60), data);
    }

    #[test]
    fn latest_assistant_must_have_valid_matching_scope_timestamp_and_usage() {
        let fixture = Fixture::new();
        let usage = usage(120, 30, 30);
        let good = assistant(AT, &usage);
        let mutations = [
            ("/sessionId", json!("another-session")),
            ("/sessionId", Value::Null),
            ("/isSidechain", json!("false")),
            ("/message/model", json!("another-model")),
            ("/message/role", json!("user")),
            ("/timestamp", json!("bad")),
            ("/timestamp", json!(AT)),
            ("/timestamp", json!("2027-01-15T08:01:01Z")),
            ("/timestamp", json!("1969-12-31T23:59:59Z")),
            ("/message/usage", Value::Null),
            ("/message/usage/cache_read_input_tokens", json!(-1)),
            ("/message/usage/cache_read_input_tokens", json!(1.5)),
            ("/message/usage/cache_read_input_tokens", json!("120")),
            ("/message/usage/cache_read_input_tokens", json!(u64::MAX)),
            (
                "/message/usage/cache_creation/ephemeral_1h_input_tokens",
                json!(31),
            ),
            (
                "/message/usage/cache_creation/ephemeral_1h_input_tokens",
                json!("30"),
            ),
        ];
        for (pointer, value) in mutations {
            let mut latest = good.clone();
            *latest.pointer_mut(pointer).unwrap() = value;
            fixture.write(&[good.clone(), latest]);
            let data = fixture.payload(&usage);
            assert_eq!(enrich_at(&data, AT + 60), data, "pointer: {pointer}");
        }
        let mut unscoped = good.clone();
        unscoped.as_object_mut().unwrap().remove("sessionId");
        fixture.write(&[good, unscoped]);
        let data = fixture.payload(&usage);
        assert_eq!(enrich_at(&data, AT + 60), data);
    }

    #[test]
    fn subagent_records_do_not_supply_or_override_main_conversation_usage() {
        let fixture = Fixture::new();
        let main = usage(120, 30, 0);
        let mut sidechain = assistant(AT + 1, &usage(50, 40, 40));
        sidechain["isSidechain"] = json!(true);
        sidechain["sessionId"] = json!("subagent-session");
        fixture.write(&[assistant(AT, &main), sidechain.clone()]);
        assert_eq!(
            enrich_at(&fixture.payload(&main), AT + 60)["prompt_cache"]["ttl"],
            "5m"
        );
        fixture.write(&[sidechain]);
        let data = fixture.payload(&main);
        assert_eq!(enrich_at(&data, AT + 60), data);
    }

    #[test]
    fn timestamp_offsets_are_honored_and_reversed_request_order_is_ambiguous() {
        let fixture = Fixture::new();
        let write = usage(0, 30, 30);
        let mut row = assistant(AT, &write);
        row["timestamp"] = json!("2027-01-15T17:00:00+09:00");
        fixture.write(&[row]);
        assert_eq!(
            enrich_at(&fixture.payload(&write), AT + 60)["prompt_cache"]["expires_at"],
            AT + 3600
        );
        let hit = usage(100, 0, 0);
        fixture.write(&[assistant(AT + 1, &write), assistant(AT, &hit)]);
        let data = fixture.payload(&hit);
        assert_eq!(enrich_at(&data, AT + 60), data);
    }

    #[test]
    fn path_must_be_absolute_local_regular_jsonl_named_for_session() {
        let fixture = Fixture::new();
        let usage = usage(120, 30, 30);
        fixture.write(&[assistant(AT, &usage)]);
        fs::copy(&fixture.path, fixture.dir.join("another-session.jsonl")).unwrap();
        fs::copy(&fixture.path, fixture.dir.join("fixture-session.txt")).unwrap();
        for path in [
            "fixture-session.jsonl".to_owned(),
            "https://example.com/fixture-session.jsonl".to_owned(),
            "\\\\server\\share\\fixture-session.jsonl".to_owned(),
            "\\\\?\\C:\\fixture-session.jsonl".to_owned(),
            fixture
                .dir
                .join("another-session.jsonl")
                .to_string_lossy()
                .into_owned(),
            fixture
                .dir
                .join("fixture-session.txt")
                .to_string_lossy()
                .into_owned(),
            fixture
                .dir
                .join("..\nfixture-session.jsonl")
                .to_string_lossy()
                .into_owned(),
            fixture
                .dir
                .join("..")
                .join(fixture.dir.file_name().unwrap())
                .join(format!("{SESSION}.jsonl"))
                .to_string_lossy()
                .into_owned(),
        ] {
            let mut data = fixture.payload(&usage);
            data["transcript_path"] = json!(path);
            assert_eq!(enrich_at(&data, AT + 60), data);
        }
        fs::remove_file(&fixture.path).unwrap();
        fs::create_dir(&fixture.path).unwrap();
        let data = fixture.payload(&usage);
        assert_eq!(enrich_at(&data, AT + 60), data);
    }

    #[test]
    fn malformed_utf8_truncated_and_oversized_latest_lines_do_not_use_older_hit() {
        let fixture = Fixture::new();
        let usage = usage(120, 30, 30);
        for suffix in [
            b"{\"type\":\"assistant\"".to_vec(),
            vec![0xff, b'\n'],
            vec![b'x'; MAX_LINE_BYTES + 1],
        ] {
            fixture.write(&[assistant(AT, &usage)]);
            let mut file = OpenOptions::new().append(true).open(&fixture.path).unwrap();
            std::io::Write::write_all(&mut file, &suffix).unwrap();
            drop(file);
            let data = fixture.payload(&usage);
            assert_eq!(enrich_at(&data, AT + 60), data);
        }
    }

    #[test]
    fn complete_final_line_at_exact_limit_is_accepted_but_one_byte_more_is_not() {
        let fixture = Fixture::new();
        let usage = usage(120, 30, 0);
        let mut record = assistant(AT, &usage);
        record["padding"] = json!("");
        let base = serde_json::to_vec(&record).unwrap().len();
        record["padding"] = json!("x".repeat(MAX_LINE_BYTES - base));
        let mut bytes = serde_json::to_vec(&record).unwrap();
        assert_eq!(bytes.len(), MAX_LINE_BYTES);
        fs::write(&fixture.path, &bytes).unwrap(); // complete JSON, no newline
        let data = fixture.payload(&usage);
        assert_eq!(enrich_at(&data, AT + 60)["prompt_cache"]["ttl"], "5m");
        bytes.push(b' ');
        assert!(observe_tail(
            &bytes,
            SESSION,
            MODEL,
            Usage::parse(&usage).unwrap(),
            AT + 60
        )
        .is_none());
        fs::write(&fixture.path, &bytes).unwrap();
        assert_eq!(enrich_at(&data, AT + 60), data);
        assert!(bounded_tail(&mut Cursor::new(vec![0; 10]), 20).is_none());
    }

    #[test]
    fn missing_or_conflicting_bucket_evidence_is_unknown() {
        let fixture = Fixture::new();
        let mut write = usage(0, 30, 0);
        for bucket in [
            Value::Null,
            json!({}),
            json!("1h"),
            json!({"ephemeral_5m_input_tokens":20}),
            json!({"ephemeral_1h_input_tokens":-1}),
            json!({"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":29}),
        ] {
            write["cache_creation"] = bucket;
            fixture.write(&[assistant(AT, &write)]);
            let data = fixture.payload(&write);
            assert_eq!(enrich_at(&data, AT + 60), data);
        }
        // Zero hour writes alone do not account for the positive write total.
        write["cache_creation"] = json!({"ephemeral_1h_input_tokens":0});
        fixture.write(&[assistant(AT, &write)]);
        let data = fixture.payload(&write);
        assert_eq!(enrich_at(&data, AT + 60), data);
        write["cache_creation"] =
            json!({"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":30});
        fixture.write(&[assistant(AT, &write)]);
        assert_eq!(
            enrich_at(&fixture.payload(&write), AT + 60)["prompt_cache"]["ttl"],
            "5m"
        );
    }

    #[test]
    fn read_budget_is_strict_and_clipped_first_line_is_discarded() {
        struct Measured {
            inner: Cursor<Vec<u8>>,
            read: usize,
        }
        impl Read for Measured {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                let n = self.inner.read(output)?;
                self.read += n;
                Ok(n)
            }
        }
        impl Seek for Measured {
            fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
                self.inner.seek(position)
            }
        }
        let usage = usage(120, 30, 30);
        let record = serde_json::to_vec(&assistant(AT, &usage)).unwrap();
        let mut bytes = vec![b'x'; MAX_TAIL_BYTES * 2];
        bytes.push(b'\n');
        bytes.extend_from_slice(&record);
        bytes.push(b'\n');
        let length = bytes.len() as u64;
        let mut reader = Measured {
            inner: Cursor::new(bytes),
            read: 0,
        };
        let tail = bounded_tail(&mut reader, length).unwrap();
        assert_eq!(reader.read, MAX_TAIL_BYTES);
        assert_eq!(tail, [record, vec![b'\n']].concat());
        assert!(observe_tail(
            &tail,
            SESSION,
            MODEL,
            Usage::parse(&usage).unwrap(),
            AT + 60
        )
        .is_some());
        // A suffix that happens to be valid JSON has no verified line start.
        let mut bytes = vec![b'x'; MAX_TAIL_BYTES];
        let forged = serde_json::to_vec(&assistant(AT, &usage)).unwrap();
        bytes.extend(vec![b' '; MAX_TAIL_BYTES - forged.len() - 1]);
        bytes.extend(forged);
        bytes.push(b'\n');
        let length = bytes.len() as u64;
        assert!(bounded_tail(&mut Cursor::new(bytes), length)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn writer_outside_tail_is_not_reused_for_read_only_latest_request() {
        let fixture = Fixture::new();
        let hit = usage(100, 0, 0);
        fixture.write(&[assistant(AT - 60, &usage(0, 100, 100))]);
        let mut file = OpenOptions::new().append(true).open(&fixture.path).unwrap();
        std::io::Write::write_all(&mut file, &vec![b' '; MAX_TAIL_BYTES]).unwrap();
        std::io::Write::write_all(&mut file, b"\n").unwrap();
        serde_json::to_writer(&mut file, &assistant(AT, &hit)).unwrap();
        drop(file);
        let data = fixture.payload(&hit);
        assert_eq!(enrich_at(&data, AT + 60), data);
    }

    #[test]
    fn no_evidence_and_nonobject_payloads_are_unchanged() {
        for data in [
            Value::Null,
            json!(false),
            json!([]),
            json!({}),
            json!({"session_id":"../escape","model":{"id":MODEL}}),
        ] {
            assert_eq!(enrich_statusline(&data), data);
        }
    }
}
