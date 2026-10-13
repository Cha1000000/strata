// SPDX-License-Identifier: MIT

//! What each mount was seen to support. Entries order fallback rungs and
//! interpret ambiguous errors; every ladder rung refuses to replace on its own,
//! so a stale entry costs speed or a refusal. The exception is the rename last
//! rung, which an entry alone enables, with its small documented window.

use std::{collections::BTreeMap, sync::Mutex};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum MountKey {
    Unique(u64),
    Mount {
        id: u64,
        device: u64,
    },
    Device(u64),
    #[cfg(test)]
    Test(u64),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct MountCapabilities {
    pub no_replace: Option<bool>,
    pub exchange: Option<bool>,
    pub hard_links: Option<bool>,
    pub rename_over_empty_directory: Option<bool>,
}

static CAPABILITIES: Mutex<BTreeMap<MountKey, MountCapabilities>> = Mutex::new(BTreeMap::new());

fn capabilities() -> std::sync::MutexGuard<'static, BTreeMap<MountKey, MountCapabilities>> {
    CAPABILITIES
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

pub(crate) fn mount_capabilities(key: MountKey) -> MountCapabilities {
    capabilities().get(&key).copied().unwrap_or_default()
}

pub(crate) fn learn_mount_capability(key: MountKey, learn: impl FnOnce(&mut MountCapabilities)) {
    learn(capabilities().entry(key).or_default());
}

/// Mount ids can be reused once a mount goes away, so a mount change drops
/// everything learned from real mounts.
pub(crate) fn forget_mount_capabilities() {
    capabilities().retain(|key, _| is_test_key(key));
}

#[cfg(test)]
fn is_test_key(key: &MountKey) -> bool {
    matches!(key, MountKey::Test(_))
}

#[cfg(not(test))]
fn is_test_key(_: &MountKey) -> bool {
    false
}

#[cfg(test)]
pub(crate) fn forget_test_mount_capabilities(id: u64) {
    capabilities().remove(&MountKey::Test(id));
}
