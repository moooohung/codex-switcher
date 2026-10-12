//! Passive native CLI discovery for the running count, independent of RC policy.
use super::{running_count::Identity, storage::Manager};
use anyhow::{ensure, Context, Result};
use serde_json::Value;
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

const LIMIT: u64 = 16 * 1024;
const PROFILE_ENTRY_LIMIT: usize = 128;
const PROFILE_LIMIT: usize = 64;
const DIRECTORY_ENTRY_LIMIT: usize = 128;
const TOTAL_ENTRY_LIMIT: usize = 1024;
const RECORD_LIMIT: usize = 256;

#[derive(Default)]
pub(super) struct Observation {
    pub identities: Vec<Identity>,
    pub error: Option<String>,
}
impl Observation {
    fn incomplete(&mut self) {
        self.error.get_or_insert_with(|| "일부 Claude 세션 정보를 안정적으로 확인할 수 없습니다. 확인된 실행 수만 표시합니다.".into());
    }
    fn keep(&mut self, candidate: Result<Option<Identity>>) {
        match candidate {
            Ok(Some(identity)) => self.identities.push(identity),
            Ok(None) => {}
            Err(_) => self.incomplete(),
        }
    }
}

struct Budget {
    entries: usize,
    records: usize,
}
impl Default for Budget {
    fn default() -> Self {
        Self {
            entries: TOTAL_ENTRY_LIMIT,
            records: RECORD_LIMIT,
        }
    }
}

fn record_identity(record: &Value, expected_pid: u32, now: i64) -> Result<Option<Identity>> {
    let kind = record["kind"]
        .as_str()
        .context("Missing Claude session kind")?;
    let entrypoint = record["entrypoint"]
        .as_str()
        .context("Missing Claude entrypoint")?;
    let domain = record["pidDomain"]
        .as_str()
        .context("Missing Claude PID domain")?;
    let spare = match &record["spare"] {
        Value::Null => false,
        value => value.as_bool().context("Invalid Claude spare marker")?,
    };
    if kind != "interactive"
        || entrypoint != "cli"
        || domain != "win32:maka"
        || spare
        || record["parkedJobId"] != Value::Null
    {
        return Ok(None);
    }
    let pid = u32::try_from(record["pid"].as_u64().context("Missing Claude PID")?)?;
    let created = record["procStart"]
        .as_str()
        .context("Missing Claude creation time")?
        .parse::<u64>()?;
    let started = record["startedAt"]
        .as_i64()
        .context("Missing Claude start time")?;
    let cwd = record["cwd"]
        .as_str()
        .context("Missing Claude working directory")?;
    uuid::Uuid::parse_str(
        record["sessionId"]
            .as_str()
            .context("Missing Claude session ID")?,
    )?;
    ensure!(
        pid == expected_pid
            && pid > 1
            && created > 0
            && Path::new(cwd).is_absolute()
            && started > 0
            && started <= now.saturating_mul(1000).saturating_add(30_000),
        "Invalid Claude runtime identity"
    );
    // Neither a bridge binding nor an RC ledger record proves a live process.
    Ok(Some(Identity { pid, created }))
}

fn read(path: &Path) -> Result<Value> {
    use std::os::windows::fs::MetadataExt;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.file_attributes() & 0x400 == 0
            && metadata.len() <= LIMIT,
        "Invalid Claude runtime observation"
    );
    let mut bytes = vec![];
    fs::File::open(path)?
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= LIMIT,
        "Claude runtime observation exceeds limit"
    );
    Ok(serde_json::from_slice(&bytes)?)
}

fn stable_identity(
    pid: u32,
    now: i64,
    mut observe: impl FnMut() -> Result<Value>,
) -> Result<Option<Identity>> {
    let first = observe()?;
    let Some(identity) = record_identity(&first, pid, now)? else {
        return Ok(None);
    };
    ensure!(
        observe()? == first,
        "Claude runtime observation changed during read"
    );
    Ok(Some(identity))
}

fn directory(path: &Path) -> Result<Option<PathBuf>> {
    use std::os::windows::fs::MetadataExt;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.file_attributes() & 0x400 == 0,
        "Claude observation directory was redirected"
    );
    Ok(Some(path.canonicalize()?))
}

fn profile_directories(m: &Manager, observation: &mut Observation) -> Vec<PathBuf> {
    let base = match directory(&m.root.join("profiles")) {
        Ok(Some(base)) => base,
        Ok(None) => return vec![],
        Err(_) => {
            observation.incomplete();
            return vec![];
        }
    };
    let entries = match fs::read_dir(&base) {
        Ok(entries) => entries,
        Err(_) => {
            observation.incomplete();
            return vec![];
        }
    };
    let mut profiles = vec![];
    for (index, entry) in entries.enumerate() {
        if index >= PROFILE_ENTRY_LIMIT || profiles.len() >= PROFILE_LIMIT {
            observation.incomplete();
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                observation.incomplete();
                continue;
            }
        };
        let name = entry.file_name();
        let Some(id) = name.to_str() else { continue };
        if !uuid::Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id) {
            continue;
        }
        match directory(&entry.path()) {
            Ok(Some(profile)) if profile == base.join(id) => profiles.push(profile),
            _ => observation.incomplete(),
        }
    }
    profiles
}

fn scan(
    root: &Path,
    now: i64,
    budget: &mut Budget,
    observation: &mut Observation,
    process_absent: &mut impl FnMut(u32) -> bool,
) -> Result<()> {
    let Some(root) = directory(root)? else {
        return Ok(());
    };
    let Some(dir) = directory(&root.join("sessions"))? else {
        return Ok(());
    };
    ensure!(
        dir == root.join("sessions"),
        "Claude runtime directory was redirected"
    );
    for (index, entry) in fs::read_dir(&dir)?.enumerate() {
        if index >= DIRECTORY_ENTRY_LIMIT || budget.entries == 0 || budget.records == 0 {
            observation.incomplete();
            break;
        }
        budget.entries -= 1;
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                observation.incomplete();
                continue;
            }
        };
        let name = entry.file_name();
        let Some(stem) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
            continue;
        };
        let Ok(pid) = stem.parse::<u32>() else {
            continue;
        };
        if pid.to_string() != stem {
            continue;
        }
        // A corrupt old publication cannot identify its generation. Ignore it
        // only when the OS proves this PID absent/exited, without comparing a
        // fabricated creation stamp. Access denied and live recycled PIDs must
        // still be parsed and qualified as incomplete if their record is bad.
        if process_absent(pid) {
            continue;
        }
        budget.records -= 1;
        let path = entry.path();
        // Retain valid peers, but never call a failed or changing read complete.
        observation.keep(stable_identity(pid, now, || read(&path)));
    }
    Ok(())
}

pub(super) fn discover(m: &Manager) -> Observation {
    discover_with_probe(m, &mut super::remote_control_console::process_absent)
}

fn discover_with_probe(m: &Manager, process_absent: &mut impl FnMut(u32) -> bool) -> Observation {
    let mut observation = Observation::default();
    let mut budget = Budget::default();
    let now = chrono::Utc::now().timestamp();
    if scan(
        &m.live_dir,
        now,
        &mut budget,
        &mut observation,
        process_absent,
    )
    .is_err()
    {
        observation.incomplete();
    }
    // Passive UUID-directory enumeration never restores accounts.json, reads
    // credentials, or assigns account/managed ownership to a discovered PID.
    for profile in profile_directories(m, &mut observation) {
        if budget.entries == 0 || budget.records == 0 {
            observation.incomplete();
            break;
        }
        if scan(&profile, now, &mut budget, &mut observation, process_absent).is_err() {
            observation.incomplete();
        }
    }
    observation
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record() -> Value {
        json!({"pid":66088,"procStart":"123","kind":"interactive","entrypoint":"cli",
            "pidDomain":"win32:maka","cwd":"C:/synthetic","startedAt":90000,
            "sessionId":"a3426688-2937-4bba-b39c-e8fbbe7f0d8f"})
    }

    #[test]
    fn runtime_candidates_require_native_interactive_identity_without_rc_binding() {
        let expected = Some(Identity {
            pid: 66088,
            created: 123,
        });
        assert_eq!(record_identity(&record(), 66088, 100).unwrap(), expected);
        let mut bound = record();
        bound["bridgeSessionId"] = json!("unverified-binding");
        assert_eq!(record_identity(&bound, 66088, 100).unwrap(), expected);
        for (field, value) in [
            ("pid", json!(66089)),
            ("procStart", json!("0")),
            ("procStart", json!("invalid")),
            ("kind", json!("headless")),
            ("entrypoint", json!("sdk")),
            ("pidDomain", json!("posix")),
            ("spare", json!(true)),
            ("parkedJobId", json!("job")),
            ("cwd", json!("relative")),
            ("startedAt", json!(0)),
            ("startedAt", json!(130001)),
            ("sessionId", json!("invalid")),
        ] {
            let mut invalid = record();
            invalid[field] = value;
            let result = record_identity(&invalid, 66088, 100);
            assert!(!matches!(result, Ok(Some(_))), "{field}");
        }
    }

    #[test]
    fn read_failures_and_second_read_changes_preserve_peers_and_mark_incomplete() {
        let mut observation = Observation::default();
        observation.keep(stable_identity(66088, 100, || Ok(record())));
        observation.keep(stable_identity(66089, 100, || {
            anyhow::bail!("first read denied")
        }));
        let mut second = false;
        observation.keep(stable_identity(66088, 100, || {
            if second {
                anyhow::bail!("second read denied");
            }
            second = true;
            Ok(record())
        }));
        let mut second = false;
        observation.keep(stable_identity(66088, 100, || {
            let mut value = record();
            if second {
                value["status"] = json!("busy");
            }
            second = true;
            Ok(value)
        }));
        assert_eq!(
            observation.identities,
            [Identity {
                pid: 66088,
                created: 123
            }]
        );
        assert!(observation.error.is_some());
        let mut empty = Observation::default();
        empty.keep(Err(anyhow::anyhow!("no stable observations")));
        assert!(empty.identities.is_empty() && empty.error.is_some());
    }

    #[test]
    fn malformed_native_metadata_is_incomplete_but_headless_sessions_are_excluded() {
        let mut invalid = record();
        invalid["procStart"] = json!("invalid");
        let mut observation = Observation::default();
        observation.keep(stable_identity(66088, 100, || Ok(invalid.clone())));
        assert!(observation.identities.is_empty() && observation.error.is_some());
        let mut headless = record();
        headless["kind"] = json!("headless");
        let mut observation = Observation::default();
        observation.keep(stable_identity(66088, 100, || Ok(headless.clone())));
        assert!(observation.identities.is_empty() && observation.error.is_none());
    }

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> (Self, Manager) {
            let root = std::env::temp_dir().join(format!(
                "claude-running-observation-{}",
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(&root).unwrap();
            let manager = Manager {
                root: root.clone(),
                live_dir: root.join("live"),
                live_config: root.join("unused-config"),
            };
            (Self(root), manager)
        }
        fn session(&self, directory: &Path, pid: u32) {
            assert!(directory.starts_with(&self.0));
            fs::create_dir_all(directory.join("sessions")).unwrap();
            let mut value = record();
            value["pid"] = json!(pid);
            fs::write(
                directory.join("sessions").join(format!("{pid}.json")),
                serde_json::to_vec(&value).unwrap(),
            )
            .unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // Only this test's absolute, privately allocated fixture is removed.
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn profile_clis_survive_empty_managed_map_and_deduplicate_with_global_without_account_repair() {
        let (fixture, manager) = Fixture::new();
        let profile = manager
            .root
            .join("profiles")
            .join(uuid::Uuid::new_v4().to_string());
        fixture.session(&manager.live_dir, 66088);
        fixture.session(&profile, 66088);
        fixture.session(&profile, 66089);
        fs::write(manager.root.join("accounts.json.bak"), b"must not restore").unwrap();
        let observation = discover_with_probe(&manager, &mut |_| false);
        assert!(observation.error.is_none());
        let (owned, count, confirmed) =
            super::super::running_count::merge(&[], &observation.identities, |pid| {
                Some(Identity { pid, created: 123 })
            });
        assert!(owned.is_empty());
        assert_eq!(count, 2);
        assert_eq!(confirmed.len(), 2);
        assert!(!manager.root.join("accounts.json").exists());
        assert_eq!(
            fs::read(manager.root.join("accounts.json.bak")).unwrap(),
            b"must not restore"
        );
    }

    #[test]
    fn malformed_profile_pid_json_preserves_valid_global_peer_and_reports_incomplete() {
        let (fixture, manager) = Fixture::new();
        let profile = manager
            .root
            .join("profiles")
            .join(uuid::Uuid::new_v4().to_string());
        fixture.session(&manager.live_dir, 66088);
        fixture.session(&profile, 66089);
        fs::write(profile.join("sessions/66089.json"), b"in-progress JSON").unwrap();
        let observation = discover_with_probe(&manager, &mut |_| false);
        assert_eq!(
            observation.identities,
            [Identity {
                pid: 66088,
                created: 123
            }]
        );
        assert!(observation.error.is_some());
    }

    #[test]
    fn record_and_total_entry_limits_keep_a_lower_bound_and_report_truncation() {
        let (fixture, manager) = Fixture::new();
        fixture.session(&manager.live_dir, 66088);
        fixture.session(&manager.live_dir, 66089);
        for mut budget in [
            Budget {
                entries: 1,
                records: RECORD_LIMIT,
            },
            Budget {
                entries: TOTAL_ENTRY_LIMIT,
                records: 1,
            },
        ] {
            let mut observation = Observation::default();
            scan(
                &manager.live_dir,
                100,
                &mut budget,
                &mut observation,
                &mut |_| false,
            )
            .unwrap();
            assert_eq!(observation.identities.len(), 1);
            assert!(observation.error.is_some());
        }
    }

    #[test]
    fn profile_enumeration_is_bounded_and_does_not_treat_other_directories_as_accounts() {
        let (fixture, manager) = Fixture::new();
        fs::create_dir_all(manager.root.join("profiles/not-an-account/sessions")).unwrap();
        let mut observation = Observation::default();
        assert!(profile_directories(&manager, &mut observation).is_empty());
        assert!(observation.error.is_none());
        for _ in 0..=PROFILE_LIMIT {
            fs::create_dir_all(
                fixture
                    .0
                    .join("profiles")
                    .join(uuid::Uuid::new_v4().to_string()),
            )
            .unwrap();
        }
        let profiles = profile_directories(&manager, &mut observation);
        assert_eq!(profiles.len(), PROFILE_LIMIT);
        assert!(observation.error.is_some());
    }

    #[test]
    fn demonstrably_absent_pid_skips_corrupt_json_without_changing_file_or_valid_peer() {
        let (fixture, manager) = Fixture::new();
        fixture.session(&manager.live_dir, 66088);
        let stale = manager.live_dir.join("sessions/17360.json");
        let original = b"{corrupt old publication, no readable creation stamp";
        fs::write(&stale, original).unwrap();
        let mut probed = vec![];
        let observation = discover_with_probe(&manager, &mut |pid| {
            probed.push(pid);
            pid == 17360
        });
        assert_eq!(
            observation.identities,
            [Identity {
                pid: 66088,
                created: 123
            }]
        );
        assert!(observation.error.is_none());
        assert!(probed.contains(&17360) && probed.contains(&66088));
        assert_eq!(fs::read(&stale).unwrap(), original);
        let all_absent = discover_with_probe(&manager, &mut |_| true);
        assert!(all_absent.identities.is_empty() && all_absent.error.is_none());
        assert_eq!(fs::read(&stale).unwrap(), original);
    }

    #[test]
    fn absence_not_proven_keeps_corrupt_pid_incomplete_and_preserves_valid_peer() {
        let (fixture, manager) = Fixture::new();
        fixture.session(&manager.live_dir, 66088);
        let corrupt = manager.live_dir.join("sessions/17360.json");
        let original = b"{corrupt current or inaccessible PID publication";
        fs::write(&corrupt, original).unwrap();
        // process_absent returns false for a live PID (including reuse), access
        // denied, and any other unknown outcome. None authorizes skipping JSON.
        let observation = discover_with_probe(&manager, &mut |_| false);
        assert_eq!(
            observation.identities,
            [Identity {
                pid: 66088,
                created: 123
            }]
        );
        assert!(observation.error.is_some());
        assert_eq!(fs::read(&corrupt).unwrap(), original);
    }
}
