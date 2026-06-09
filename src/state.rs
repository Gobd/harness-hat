use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::fs_util::{set_private_file_permissions, write_private_file};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub project: String,
    pub argv: Vec<String>,
    pub cwd: String,
    pub decision: DecisionKind,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<u64>,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    Auto,
    Approved,
    Remembered,
    Denied,
    DeniedByPolicy,
    TimedOut,
}

impl DecisionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "AUTO",
            Self::Approved => "APPR",
            Self::Remembered => "REMB",
            Self::Denied => "DENY",
            Self::DeniedByPolicy => "DENY*",
            Self::TimedOut => "TOUT",
        }
    }
}

#[derive(Clone)]
pub struct StateManager {
    log_dir: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl StateManager {
    pub fn open(log_dir: &Path) -> Result<Self> {
        create_private_dir_all(log_dir)
            .with_context(|| format!("creating log dir: {}", log_dir.display()))?;
        Ok(Self {
            log_dir: log_dir.to_path_buf(),
            lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn get_or_create_token(&self) -> Result<String> {
        let _guard = self.lock.lock().unwrap();
        let path = self.token_path();
        if path.exists() {
            // Open with O_NOFOLLOW so a planted symlink (e.g. log_dir/token ->
            // /etc/shadow) cannot trick us into reading arbitrary host files
            // and emitting their contents as Authorization tokens. Then check
            // the file type before reading so we cannot be tricked into reading
            // a fifo / device. Read first, then set perms — `set_permissions`
            // can fail on a read-only FS, but a successful read still produces
            // a usable token.
            let opened = open_no_follow_regular(&path)?;
            if let Some(mut file) = opened {
                use std::io::Read;
                let mut buf = String::new();
                file.read_to_string(&mut buf)
                    .with_context(|| format!("reading token file: {}", path.display()))?;
                drop(file);
                // Best-effort chmod after a successful read; do not fail token
                // retrieval if the FS is read-only.
                let _ = set_private_file_permissions(&path);
                let token = buf.trim().to_string();
                if !token.is_empty() {
                    return Ok(token);
                }
            }
        }

        let token = uuid::Uuid::new_v4().to_string().replace('-', "");
        write_private_file(&path, format!("{token}\n").as_bytes())
            .with_context(|| format!("writing token file: {}", path.display()))?;
        Ok(token)
    }

    /// Append one audit event to the current UTC day file as JSONL.
    pub fn log_audit(&self, entry: &AuditEntry) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        let path = self.audit_path_for(entry.timestamp.date_naive());
        let mut f = open_audit_log_append(&path)?;
        let line = serde_json::to_string(entry).context("serializing audit entry")?;
        f.write_all(line.as_bytes())
            .with_context(|| format!("writing audit log: {}", path.display()))?;
        f.write_all(b"\n")
            .with_context(|| format!("writing audit newline: {}", path.display()))?;
        Ok(())
    }

    /// Load the most recent audit events (newest first) from daily JSONL files.
    pub fn recent_audit(&self, limit: usize) -> Result<Vec<AuditEntry>> {
        let mut files: Vec<PathBuf> = fs::read_dir(&self.log_dir)
            .with_context(|| format!("reading log dir: {}", self.log_dir.display()))?
            .filter_map(|ent| ent.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("audit-") && n.ends_with(".log"))
                    .unwrap_or(false)
            })
            .collect();
        files.sort();
        files.reverse();

        let mut out = Vec::new();
        for path in files {
            if out.len() >= limit {
                break;
            }
            let f = match fs::File::open(&path) {
                Ok(f) => f,
                Err(_) => continue,
            };
            let reader = BufReader::new(f);
            let mut day_entries = Vec::new();
            for (lineno, line) in reader.lines().enumerate() {
                let Ok(line) = line else {
                    continue;
                };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<AuditEntry>(&line) {
                    Ok(entry) => day_entries.push(entry),
                    Err(e) => tracing::warn!(
                        path = %path.display(),
                        line = lineno + 1,
                        error = %e,
                        "audit log entry failed to parse"
                    ),
                }
            }
            day_entries.sort_by_key(|e| e.timestamp);
            day_entries.reverse();
            for entry in day_entries {
                out.push(entry);
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    fn token_path(&self) -> PathBuf {
        self.log_dir.join("token")
    }

    fn audit_path_for(&self, day: chrono::NaiveDate) -> PathBuf {
        self.log_dir
            .join(format!("audit-{}.log", day.format("%Y-%m-%d")))
    }
}

/// Create `path` (and parents) with restrictive directory permissions on Unix.
/// Mirrors `fs::create_dir_all` but applies `mode = 0o700` to newly-created
/// directories via `DirBuilderExt`; on non-Unix this is just `create_dir_all`.
fn create_private_dir_all(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut b = std::fs::DirBuilder::new();
        b.recursive(true);
        b.mode(0o700);
        b.create(path)?;
        // `DirBuilder` won't tighten an already-loose existing directory; do
        // that explicitly for the leaf so the invariant holds either way.
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.is_dir() {
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
            }
        }
        return Ok(());
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path)
    }
}

/// Open `path` for reading with `O_NOFOLLOW` (Unix) and verify it is a regular
/// file. Returns `Ok(None)` if the file does not exist; `Err` on any other I/O
/// failure or if a symlink/non-regular path is encountered.
fn open_no_follow_regular(path: &Path) -> Result<Option<std::fs::File>> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let file = match opts.open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(anyhow::Error::from(e))
                .with_context(|| format!("opening (O_NOFOLLOW) {}", path.display()));
        }
    };
    let meta = file
        .metadata()
        .with_context(|| format!("statting {}", path.display()))?;
    anyhow::ensure!(
        meta.file_type().is_file(),
        "refusing to read non-regular file at {}",
        path.display()
    );
    Ok(Some(file))
}

/// Open the audit-log file for append with `mode = 0o600` at create time and
/// `O_NOFOLLOW` so a symlink planted at `log_dir/audit-YYYY-MM-DD.log` cannot
/// redirect appends out of the log directory.
fn open_audit_log_append(path: &Path) -> Result<std::fs::File> {
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let f = opts
        .open(path)
        .with_context(|| format!("opening audit log: {}", path.display()))?;
    // Belt-and-suspenders: on existing files our `mode(0o600)` is ignored, so
    // tighten perms after open. The chmod here is safe (target is already an
    // open fd we just confirmed via O_NOFOLLOW is not a symlink).
    let _ = set_private_file_permissions(path);
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn get_or_create_token_is_persistent() {
        let dir = tempdir().expect("create temp dir");
        let state1 = StateManager::open(dir.path()).expect("open state1");
        let token1 = state1.get_or_create_token().expect("get token1");

        // Re-open same dir
        let state2 = StateManager::open(dir.path()).expect("open state2");
        let token2 = state2.get_or_create_token().expect("get token2");

        assert_eq!(token1, token2);
    }

    #[test]
    fn log_audit_and_recent_audit_works() {
        let dir = tempdir().expect("create temp dir");
        let state = StateManager::open(dir.path()).expect("open state");

        let now = Utc::now();
        let entry1 = AuditEntry {
            project: "p1".to_string(),
            argv: vec!["ls".into()],
            cwd: "/".into(),
            decision: DecisionKind::Auto,
            exit_code: Some(0),
            duration_ms: Some(10),
            timestamp: now - chrono::Duration::seconds(10),
        };
        let entry2 = AuditEntry {
            project: "p1".to_string(),
            argv: vec!["pwd".into()],
            cwd: "/".into(),
            decision: DecisionKind::Approved,
            exit_code: Some(0),
            duration_ms: Some(5),
            timestamp: now,
        };

        state.log_audit(&entry1).expect("log 1");
        state.log_audit(&entry2).expect("log 2");

        let recent = state.recent_audit(10).expect("recent");
        assert_eq!(recent.len(), 2);
        // Should be newest first
        assert_eq!(recent[0].argv[0], "pwd");
        assert_eq!(recent[1].argv[0], "ls");
    }

    #[test]
    fn recent_audit_handles_malformed_lines() {
        let dir = tempdir().expect("create temp dir");
        let state = StateManager::open(dir.path()).expect("open state");
        let path = state.audit_path_for(Utc::now().date_naive());

        fs::write(&path, "not json\n{\"project\":\"p\"}\n").expect("write malformed");

        // Only valid JSON lines should be returned (though my simple test entry is incomplete,
        // AuditEntry requires more fields, so it might skip both if not valid).
        // Let's write one valid entry and one invalid.
        let entry = AuditEntry {
            project: "valid".to_string(),
            argv: vec![],
            cwd: "".into(),
            decision: DecisionKind::Auto,
            exit_code: None,
            duration_ms: None,
            timestamp: Utc::now(),
        };
        state.log_audit(&entry).expect("log valid");

        let recent = state.recent_audit(10).expect("recent");
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].project, "valid");
    }

    #[test]
    fn recent_audit_spans_multiple_days() {
        let dir = tempdir().expect("create temp dir");
        let state = StateManager::open(dir.path()).expect("open state");

        let day1 = Utc::now() - chrono::Duration::days(1);
        let day2 = Utc::now();

        let entry1 = AuditEntry {
            project: "p1".to_string(),
            argv: vec!["day1".into()],
            cwd: "".into(),
            decision: DecisionKind::Auto,
            exit_code: None,
            duration_ms: None,
            timestamp: day1,
        };
        let entry2 = AuditEntry {
            project: "p1".to_string(),
            argv: vec!["day2".into()],
            cwd: "".into(),
            decision: DecisionKind::Auto,
            exit_code: None,
            duration_ms: None,
            timestamp: day2,
        };

        state.log_audit(&entry1).expect("log 1");
        state.log_audit(&entry2).expect("log 2");

        let recent = state.recent_audit(10).expect("recent");
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].argv[0], "day2");
        assert_eq!(recent[1].argv[0], "day1");
    }
}
