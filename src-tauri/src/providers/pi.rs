use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;

use super::traits::TokenProvider;
use super::types::{AllStats, DailyUsage, ModelUsage};

// --- Cache infrastructure (mirrors codex.rs patterns) ---

struct IncrementalCache {
    stats: AllStats,
    computed_at: Instant,
    /// Per-file parsed entries keyed by dedup key (file:message_id)
    entries: HashMap<String, PiEntry>,
    /// File metadata for mtime-based change detection
    file_meta: HashMap<PathBuf, (SystemTime, u64)>,
}

static STATS_CACHE: Mutex<Option<IncrementalCache>> = Mutex::new(None);
static PARSING: AtomicBool = AtomicBool::new(false);
static CACHE_INVALIDATED: AtomicBool = AtomicBool::new(false);
const CACHE_TTL: Duration = Duration::from_secs(120);

/// Invalidate cache — called by file watcher on ~/.pi/ changes.
pub fn invalidate_stats_cache() {
    CACHE_INVALIDATED.store(true, Ordering::Relaxed);
}

/// Return cached stats without triggering a re-parse (used by tray update).
pub fn get_cached_stats() -> Option<AllStats> {
    STATS_CACHE.lock().ok()?.as_ref().map(|c| c.stats.clone())
}

// --- Entry type ---

#[derive(Clone)]
struct PiEntry {
    date: String,
    model: String,
    session_id: String,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    /// Pi stores pre-computed USD cost per message — trust it.
    cost_usd: f64,
}

// --- Provider ---

pub struct PiProvider {
    pub data_dir: PathBuf,
}

impl PiProvider {
    pub fn new() -> Self {
        Self {
            data_dir: Self::detect_data_dir(),
        }
    }

    /// Detect Pi agent data directory.
    /// `PI_DATA_DIR` env var overrides; otherwise falls back to `~/.pi/agent`.
    fn detect_data_dir() -> PathBuf {
        if let Ok(override_path) = std::env::var("PI_DATA_DIR") {
            return PathBuf::from(override_path);
        }
        let home = dirs::home_dir().unwrap_or_default();
        home.join(".pi").join("agent")
    }

    fn sessions_dir(&self) -> PathBuf {
        self.data_dir.join("sessions")
    }

    /// Collect mtime/size metadata for all session JSONL files.
    fn collect_file_meta(&self) -> HashMap<PathBuf, (SystemTime, u64)> {
        let mut meta = HashMap::new();
        let root = self.sessions_dir();
        if !root.exists() {
            return meta;
        }
        let pattern = root
            .join("**")
            .join("*.jsonl")
            .to_string_lossy()
            .to_string();
        let files = glob::glob(&pattern).unwrap_or_else(|_| glob::glob("").unwrap());
        for path in files.flatten() {
            if let Ok(m) = fs::metadata(&path) {
                let mtime = m.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                meta.insert(path, (mtime, m.len()));
            }
        }
        meta
    }

    /// Parse a single JSONL file and return entries keyed by dedup key.
    fn parse_single_file(path: &Path) -> HashMap<String, PiEntry> {
        let mut entries = HashMap::new();
        let Ok(file) = fs::File::open(path) else {
            return entries;
        };

        // Session id derived from filename UUID; may be overwritten by `session` event.
        let mut session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|stem| stem.rsplit('_').next())
            .unwrap_or("pi-session")
            .to_string();

        let file_key = path.to_string_lossy().to_string();
        let reader = BufReader::with_capacity(64 * 1024, file);
        let mut line_index: u32 = 0;

        for line in reader.lines().map_while(Result::ok) {
            line_index += 1;

            // Pre-filter: skip lines that clearly don't have usage data.
            if !line.contains("\"usage\"") && !line.contains("\"type\":\"session\"") {
                continue;
            }

            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };

            match value.get("type").and_then(|v| v.as_str()) {
                Some("session") => {
                    if let Some(id) = value.get("id").and_then(|v| v.as_str()) {
                        session_id = id.to_string();
                    }
                }
                Some("message") => {
                    let Some(entry) = parse_message_entry(&value, &session_id) else {
                        continue;
                    };
                    // Dedup key: prefer message id, fall back to file+line
                    let msg_id = value
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let key = if msg_id.is_empty() {
                        format!("{}:{}", file_key, line_index)
                    } else {
                        format!("{}:{}", file_key, msg_id)
                    };
                    entries.insert(key, entry);
                }
                _ => {}
            }
        }

        entries
    }

    /// Incrementally parse only changed files, reusing cached entries for unchanged files.
    fn parse_incremental(
        current_meta: &HashMap<PathBuf, (SystemTime, u64)>,
        cached_entries: &HashMap<String, PiEntry>,
        cached_meta: &HashMap<PathBuf, (SystemTime, u64)>,
    ) -> HashMap<String, PiEntry> {
        let has_deleted = cached_meta.keys().any(|p| !current_meta.contains_key(p));
        if has_deleted {
            let mut fresh = HashMap::new();
            for path in current_meta.keys() {
                fresh.extend(Self::parse_single_file(path));
            }
            return fresh;
        }

        let mut entries = cached_entries.clone();
        let mut changed_paths: Vec<&PathBuf> = Vec::new();
        for (path, meta) in current_meta {
            match cached_meta.get(path) {
                Some(cached) if cached == meta => {}
                _ => changed_paths.push(path),
            }
        }

        let changed_count = changed_paths.len();
        if changed_count > 0 {
            let start = Instant::now();
            // Drop stale entries from changed files before re-parsing.
            for path in &changed_paths {
                let prefix = format!("{}:", path.to_string_lossy());
                entries.retain(|k, _| !k.starts_with(&prefix));
            }
            for path in &changed_paths {
                entries.extend(Self::parse_single_file(path));
            }
            eprintln!(
                "[PERF][Pi] Incremental parse: {} changed files in {:?} (total {} files)",
                changed_count,
                start.elapsed(),
                current_meta.len()
            );
        }

        entries
    }

    fn build_stats(entries: &HashMap<String, PiEntry>) -> AllStats {
        let mut daily_map: HashMap<String, DailyUsage> = HashMap::new();
        let mut model_usage_map: HashMap<String, ModelUsage> = HashMap::new();
        let mut total_messages: u32 = 0;
        let mut first_date: Option<String> = None;
        let mut daily_session_ids: HashMap<String, HashSet<String>> = HashMap::new();

        for entry in entries.values() {
            total_messages += 1;

            if first_date.as_ref().map_or(true, |d| entry.date < *d) {
                first_date = Some(entry.date.clone());
            }

            let total_tokens = entry.input_tokens + entry.output_tokens;

            let daily = daily_map
                .entry(entry.date.clone())
                .or_insert_with(|| DailyUsage {
                    date: entry.date.clone(),
                    tokens: HashMap::new(),
                    cost_usd: 0.0,
                    messages: 0,
                    sessions: 0,
                    tool_calls: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                });
            *daily.tokens.entry(entry.model.clone()).or_insert(0) += total_tokens;
            daily.cost_usd += entry.cost_usd;
            daily.messages += 1;
            daily.input_tokens += entry.input_tokens;
            daily.output_tokens += entry.output_tokens;
            daily.cache_read_tokens += entry.cache_read_tokens;
            daily.cache_write_tokens += entry.cache_write_tokens;

            daily_session_ids
                .entry(entry.date.clone())
                .or_default()
                .insert(entry.session_id.clone());

            let mu = model_usage_map
                .entry(entry.model.clone())
                .or_insert_with(|| ModelUsage {
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read: 0,
                    cache_write: 0,
                    cost_usd: 0.0,
                });
            mu.input_tokens += entry.input_tokens;
            mu.output_tokens += entry.output_tokens;
            mu.cache_read += entry.cache_read_tokens;
            mu.cache_write += entry.cache_write_tokens;
            mu.cost_usd += entry.cost_usd;
        }

        for (date, session_ids) in &daily_session_ids {
            if let Some(daily) = daily_map.get_mut(date) {
                daily.sessions = session_ids.len() as u32;
            }
        }

        let mut daily: Vec<DailyUsage> = daily_map.into_values().collect();
        daily.sort_by(|a, b| a.date.cmp(&b.date));
        let total_sessions = daily.iter().map(|d| d.sessions).sum();

        AllStats {
            daily,
            model_usage: model_usage_map,
            total_sessions,
            total_messages,
            first_session_date: first_date,
            analytics: None,
        }
    }

    fn do_fetch_stats(&self) -> Result<AllStats, String> {
        let start = Instant::now();
        let current_meta = self.collect_file_meta();

        if current_meta.is_empty() {
            return Err("No Pi agent session data found".to_string());
        }

        let entries = if let Ok(cache) = STATS_CACHE.lock() {
            if let Some(ref cached) = *cache {
                if cached.file_meta == current_meta {
                    drop(cache);
                    if let Ok(mut cache) = STATS_CACHE.lock() {
                        if let Some(ref mut cached) = *cache {
                            cached.computed_at = Instant::now();
                        }
                    }
                    eprintln!(
                        "[PERF][Pi] No files changed, reusing cache ({:?})",
                        start.elapsed()
                    );
                    if let Ok(cache) = STATS_CACHE.lock() {
                        if let Some(ref cached) = *cache {
                            return Ok(cached.stats.clone());
                        }
                    }
                    return Err("Cache lost during refresh".to_string());
                }
                Self::parse_incremental(&current_meta, &cached.entries, &cached.file_meta)
            } else {
                drop(cache);
                eprintln!(
                    "[PERF][Pi] First run, full parse of {} files...",
                    current_meta.len()
                );
                let mut entries = HashMap::new();
                for path in current_meta.keys() {
                    entries.extend(Self::parse_single_file(path));
                }
                entries
            }
        } else {
            return Err("Failed to acquire cache lock".to_string());
        };

        let stats = Self::build_stats(&entries);

        if let Ok(mut cache) = STATS_CACHE.lock() {
            *cache = Some(IncrementalCache {
                stats: stats.clone(),
                computed_at: Instant::now(),
                entries,
                file_meta: current_meta,
            });
        }

        eprintln!("[PERF][Pi] Total fetch_stats: {:?}", start.elapsed());
        Ok(stats)
    }
}

impl TokenProvider for PiProvider {
    fn name(&self) -> &str {
        "Pi"
    }

    fn fetch_stats(&self) -> Result<AllStats, String> {
        let was_invalidated = CACHE_INVALIDATED.swap(false, Ordering::Relaxed);

        if !was_invalidated {
            if let Ok(cache) = STATS_CACHE.lock() {
                if let Some(ref cached) = *cache {
                    if cached.computed_at.elapsed() < CACHE_TTL {
                        return Ok(cached.stats.clone());
                    }
                }
            }
        }

        if PARSING
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            if let Ok(cache) = STATS_CACHE.lock() {
                if let Some(ref cached) = *cache {
                    return Ok(cached.stats.clone());
                }
            }
            std::thread::sleep(Duration::from_millis(100));
            if let Ok(cache) = STATS_CACHE.lock() {
                if let Some(ref cached) = *cache {
                    return Ok(cached.stats.clone());
                }
            }
            return Err("Pi stats computation in progress".to_string());
        }

        let result = self.do_fetch_stats();
        PARSING.store(false, Ordering::SeqCst);
        result
    }

    fn is_available(&self) -> bool {
        self.sessions_dir().is_dir()
    }
}

// --- Helpers ---

/// Parse a Pi `message` JSONL event into a `PiEntry`.
/// Only assistant messages with a `usage` block are kept.
fn parse_message_entry(value: &Value, fallback_session: &str) -> Option<PiEntry> {
    let message = value.get("message")?;
    let role = message.get("role").and_then(|v| v.as_str())?;
    if role != "assistant" {
        return None;
    }

    let usage = message.get("usage")?;

    let input = usage.get("input").and_then(|v| v.as_u64()).unwrap_or(0);
    let output = usage.get("output").and_then(|v| v.as_u64()).unwrap_or(0);
    let cache_read = usage.get("cacheRead").and_then(|v| v.as_u64()).unwrap_or(0);
    let cache_write = usage
        .get("cacheWrite")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    if input == 0 && output == 0 && cache_read == 0 && cache_write == 0 {
        return None;
    }

    // Pi pre-computes cost per message — prefer that over a re-derivation.
    let cost_usd = usage
        .pointer("/cost/total")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);

    let model = message
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let timestamp = value
        .get("timestamp")
        .and_then(|v| v.as_str())
        .or_else(|| message.get("timestamp").and_then(|v| v.as_str()))?;
    let date = extract_local_date(timestamp)?;

    Some(PiEntry {
        date,
        model,
        session_id: fallback_session.to_string(),
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        cost_usd,
    })
}

/// Convert an ISO-8601 timestamp to a local-timezone `YYYY-MM-DD` string.
fn extract_local_date(ts: &str) -> Option<String> {
    use chrono::{DateTime, Utc};
    if let Ok(utc_dt) = ts.parse::<DateTime<Utc>>() {
        return Some(
            utc_dt
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string(),
        );
    }
    ts.get(..10).map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_message_entry_extracts_usage() {
        let v: Value = serde_json::json!({
            "type": "message",
            "id": "msg-1",
            "timestamp": "2026-04-14T17:13:24.895Z",
            "message": {
                "role": "assistant",
                "model": "gpt-5.4",
                "provider": "openai-codex",
                "usage": {
                    "input": 7221,
                    "output": 222,
                    "cacheRead": 100,
                    "cacheWrite": 50,
                    "totalTokens": 7593,
                    "cost": {
                        "input": 0.018,
                        "output": 0.003,
                        "cacheRead": 0.0,
                        "cacheWrite": 0.0,
                        "total": 0.021382
                    }
                }
            }
        });
        let entry = parse_message_entry(&v, "fallback-session").unwrap();
        assert_eq!(entry.model, "gpt-5.4");
        assert_eq!(entry.input_tokens, 7221);
        assert_eq!(entry.output_tokens, 222);
        assert_eq!(entry.cache_read_tokens, 100);
        assert_eq!(entry.cache_write_tokens, 50);
        assert!((entry.cost_usd - 0.021382).abs() < 1e-9);
        assert_eq!(entry.session_id, "fallback-session");
    }

    #[test]
    fn parse_message_entry_rejects_user() {
        let v: Value = serde_json::json!({
            "type": "message",
            "message": {
                "role": "user",
                "usage": {"input": 100, "output": 0}
            },
            "timestamp": "2026-04-14T17:13:24.895Z"
        });
        assert!(parse_message_entry(&v, "s").is_none());
    }

    #[test]
    fn parse_message_entry_rejects_zero_tokens() {
        let v: Value = serde_json::json!({
            "type": "message",
            "message": {
                "role": "assistant",
                "model": "gpt-5.4",
                "usage": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}
            },
            "timestamp": "2026-04-14T17:13:24.895Z"
        });
        assert!(parse_message_entry(&v, "s").is_none());
    }

    #[test]
    fn parse_message_entry_handles_missing_cost() {
        let v: Value = serde_json::json!({
            "type": "message",
            "message": {
                "role": "assistant",
                "model": "gpt-5.4",
                "usage": {"input": 100, "output": 50, "cacheRead": 0, "cacheWrite": 0}
            },
            "timestamp": "2026-04-14T17:13:24.895Z"
        });
        let entry = parse_message_entry(&v, "s").unwrap();
        assert_eq!(entry.cost_usd, 0.0);
    }

    #[test]
    fn build_stats_aggregates_by_date_model_session() {
        let mut entries = HashMap::new();
        entries.insert(
            "k1".to_string(),
            PiEntry {
                date: "2026-04-14".to_string(),
                model: "gpt-5.4".to_string(),
                session_id: "s1".to_string(),
                input_tokens: 1000,
                output_tokens: 200,
                cache_read_tokens: 50,
                cache_write_tokens: 10,
                cost_usd: 0.012,
            },
        );
        entries.insert(
            "k2".to_string(),
            PiEntry {
                date: "2026-04-14".to_string(),
                model: "gpt-5.4".to_string(),
                session_id: "s1".to_string(),
                input_tokens: 500,
                output_tokens: 100,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cost_usd: 0.008,
            },
        );
        entries.insert(
            "k3".to_string(),
            PiEntry {
                date: "2026-04-15".to_string(),
                model: "claude-sonnet-4-6".to_string(),
                session_id: "s2".to_string(),
                input_tokens: 800,
                output_tokens: 300,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cost_usd: 0.05,
            },
        );
        let stats = PiProvider::build_stats(&entries);
        assert_eq!(stats.total_messages, 3);
        assert_eq!(stats.daily.len(), 2);
        assert_eq!(stats.total_sessions, 2);
        let day0 = &stats.daily[0];
        assert_eq!(day0.date, "2026-04-14");
        assert_eq!(day0.messages, 2);
        assert_eq!(day0.sessions, 1);
        assert_eq!(day0.input_tokens, 1500);
        assert!((day0.cost_usd - 0.02).abs() < 1e-9);
        assert!(stats.model_usage.contains_key("gpt-5.4"));
        assert!(stats.model_usage.contains_key("claude-sonnet-4-6"));
    }

    #[test]
    fn extract_local_date_parses_iso_timestamp() {
        let d = extract_local_date("2026-04-14T17:13:24.895Z").unwrap();
        assert_eq!(d.len(), 10);
        assert!(d.starts_with("2026-04-1"));
    }

    #[test]
    fn extract_local_date_fallback_substring() {
        let d = extract_local_date("2026-04-14").unwrap();
        assert_eq!(d, "2026-04-14");
    }

    /// Combined into one test to avoid races on the shared `PI_DATA_DIR` env var
    /// when `cargo test` runs tests in parallel.
    #[test]
    fn detect_data_dir_env_override_and_default() {
        let prev = std::env::var("PI_DATA_DIR").ok();

        std::env::set_var("PI_DATA_DIR", "/tmp/custom-pi");
        let provider = PiProvider::new();
        assert_eq!(provider.data_dir, PathBuf::from("/tmp/custom-pi"));

        std::env::remove_var("PI_DATA_DIR");
        let provider = PiProvider::new();
        let s = provider.data_dir.to_string_lossy().into_owned();
        assert!(s.contains(".pi") && s.ends_with("agent"), "got: {s}");

        if let Some(v) = prev {
            std::env::set_var("PI_DATA_DIR", v);
        }
    }

    #[test]
    fn parse_single_file_reads_session_id_and_messages() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("ai-tm-pi-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("2026-04-14T00-00-00_abc.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"session","id":"session-xyz","timestamp":"2026-04-14T00:00:00Z","cwd":"/x"}}"#
        )
        .unwrap();
        writeln!(f, r#"{{"type":"message","id":"m1","timestamp":"2026-04-14T00:01:00Z","message":{{"role":"assistant","model":"gpt-5.4","usage":{{"input":100,"output":50,"cacheRead":0,"cacheWrite":0,"cost":{{"total":0.001}}}}}}}}"#).unwrap();
        writeln!(
            f,
            r#"{{"type":"model_change","provider":"openai-codex","modelId":"gpt-5.4"}}"#
        )
        .unwrap();
        drop(f);

        let entries = PiProvider::parse_single_file(&path);
        assert_eq!(entries.len(), 1);
        let entry = entries.values().next().unwrap();
        assert_eq!(entry.session_id, "session-xyz");
        assert_eq!(entry.model, "gpt-5.4");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
