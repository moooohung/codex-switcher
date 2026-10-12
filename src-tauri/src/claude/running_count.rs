//! Reconcile candidate processes against one current observation per PID.
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Identity {
    pub pid: u32,
    pub created: u64,
}

pub(super) fn merge(
    managed: &[(String, Identity)],
    discovered: &[Identity],
    mut current: impl FnMut(u32) -> Option<Identity>,
) -> (Vec<String>, usize, Vec<Identity>) {
    let mut observed = BTreeMap::new();
    let mut alive = BTreeSet::new();
    let mut managed_ids = BTreeSet::new();
    for (account, expected) in managed
        .iter()
        .map(|(account, identity)| (Some(account), identity))
        .chain(discovered.iter().map(|identity| (None, identity)))
    {
        if expected.pid <= 1 || expected.created == 0 {
            continue;
        }
        let actual = observed
            .entry(expected.pid)
            .or_insert_with(|| current(expected.pid));
        if *actual == Some(*expected) {
            alive.insert(*expected);
            if let Some(account) = account {
                managed_ids.insert(account.clone());
            }
        }
    }
    (
        managed_ids.into_iter().collect(),
        alive.len(),
        alive.into_iter().collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(pid: u32, created: u64) -> Identity {
        Identity { pid, created }
    }

    #[test]
    fn external_native_claude_counts_without_a_managed_account_or_rc_connection() {
        let native = identity(66088, 123);
        let (managed, count, confirmed) = merge(&[], &[native], |_| Some(native));
        assert!(managed.is_empty());
        assert_eq!(count, 1);
        assert_eq!(confirmed, [native]);
    }

    #[test]
    fn managed_and_discovered_count_each_actual_process_only_once() {
        let native = identity(66088, 123);
        let other = identity(66089, 456);
        let candidates = vec![("account-a".into(), native), ("account-a".into(), native)];
        let mut probes = 0;
        let (managed, count, _) = merge(&candidates, &[native, native, other], |pid| {
            probes += 1;
            Some(if pid == native.pid { native } else { other })
        });
        assert_eq!(managed, ["account-a"]);
        assert_eq!(count, 2);
        assert_eq!(probes, 2);
    }

    #[test]
    fn stale_and_exited_registry_records_never_count_as_running() {
        let stale = identity(66088, 123);
        let exited = identity(66089, 456);
        let recycled = identity(stale.pid, 789);
        let (managed, count, _) = merge(&[("account-a".into(), stale)], &[stale, exited], |pid| {
            (pid == stale.pid).then_some(recycled)
        });
        assert!(managed.is_empty());
        assert_eq!(count, 0);
    }

    #[test]
    fn recycled_pid_can_count_new_discovery_without_reclaiming_managed_ownership() {
        let old = identity(66088, 123);
        let new = identity(old.pid, 456);
        let (managed, count, _) = merge(&[("old-account".into(), old)], &[old, new], |_| Some(new));
        assert!(managed.is_empty());
        assert_eq!(count, 1);
    }

    #[test]
    fn invalid_or_unobservable_identities_do_not_count() {
        let invalid = [identity(0, 123), identity(1, 123), identity(66088, 0)];
        let (_, count, _) = merge(&[], &invalid, |_| {
            panic!("invalid identity must not be probed")
        });
        assert_eq!(count, 0);
        let (_, count, _) = merge(&[], &[identity(66088, 123)], |_| None);
        assert_eq!(count, 0);
    }
}
