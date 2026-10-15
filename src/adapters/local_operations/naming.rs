// SPDX-License-Identifier: MIT

use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    os::unix::ffi::OsStrExt,
    path::Path,
};

use super::{copy_engine::stage, placement};
use crate::adapters::volume::{NameEncoding, NameLimit, NameProfile, Reserved};

pub(super) mod fix;

pub(super) fn parse_copy_suffix(stem: &OsStr) -> (&OsStr, Option<u64>) {
    let bytes = stem.as_bytes();
    if let Some(without_closing_parenthesis) = bytes.strip_suffix(b")")
        && let Some(separator) = without_closing_parenthesis
            .windows(b" (".len())
            .rposition(|window| window == b" (")
    {
        let suffix = &without_closing_parenthesis[separator + b" (".len()..];
        if !suffix.is_empty()
            && suffix[0] != b'0'
            && suffix.iter().all(u8::is_ascii_digit)
            && let Ok(suffix) = std::str::from_utf8(suffix)
            && let Ok(number) = suffix.parse::<u64>()
            && number < u64::MAX
        {
            return (OsStr::from_bytes(&bytes[..separator]), Some(number));
        }
    }
    (stem, None)
}

/// Keep Both's names: "name (1)", or one past the number it has, with a
/// file's last extension kept, wrapping past the largest number, and each
/// cut to fit `limit`.
pub(super) fn numbered_names(
    name: &OsStr,
    is_directory: bool,
    limit: NameLimit,
) -> impl Iterator<Item = OsString> + '_ {
    let path = Path::new(name);
    let stem = match path.extension() {
        Some(extension) if !is_directory && !extension.is_empty() => {
            path.file_stem().unwrap_or(name)
        }
        _ => name,
    };
    let extension = OsStr::from_bytes(&name.as_bytes()[stem.len()..]);
    let (base_stem, copy_num) = parse_copy_suffix(stem);
    let start = copy_num.map_or(1, |number| number + 1);
    (start..=u64::MAX)
        .chain(1..start)
        .take(placement::NUMBERED_NAME_ATTEMPTS as usize)
        .map(move |index| numbered_name(base_stem, index, extension, limit, is_directory))
}

/// `stem (number)` and `extension`, cut to fit `limit`.
pub(super) fn numbered_name(
    stem: &OsStr,
    number: u64,
    extension: &OsStr,
    limit: NameLimit,
    is_directory: bool,
) -> OsString {
    fix::fit(
        stem,
        &format!(" ({number})"),
        extension,
        limit,
        is_directory,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Taken {
    Fail,
    Number,
}

impl Taken {
    /// `limit` is read only to number names.
    pub(super) fn names(
        self,
        name: &OsStr,
        is_directory: bool,
        limit: impl FnOnce() -> NameLimit,
    ) -> impl Iterator<Item = OsString> + '_ {
        let (plain, numbered) = match self {
            Self::Fail => (Some(name.to_os_string()), None),
            Self::Number => (None, Some(numbered_names(name, is_directory, limit()))),
        };
        plain.into_iter().chain(numbered.into_iter().flatten())
    }
}

/// The rules FAT-family destinations get until transfers read their
/// destination's profile. Device names and malformed bytes are kept, which a
/// move within the mount must not change.
pub(super) fn fat_family_profile() -> NameProfile {
    NameProfile::windows(Reserved::None, NameEncoding::AnyBytes)
}

/// How a folder tells its names apart.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Key {
    Exact,
    /// As a filesystem that may take two names for one does, and where a
    /// folder's casefolding is unknown.
    Folded,
}

impl Key {
    fn of(self, name: &OsStr) -> OsString {
        match self {
            Self::Exact => name.to_os_string(),
            Self::Folded => stage::collision_key(name),
        }
    }
}

pub(super) struct PlanEntry<'a> {
    pub name: &'a OsStr,
    pub is_directory: bool,
    /// Whether `profile` fixes the name, rather than it being kept.
    pub fixing: bool,
}

/// The names of a folder's entries, in their order. A name that needs no
/// fix is kept, even where another entry's fixed name would take it, and
/// fixed names are numbered past it and each other in the byte order of
/// the names they were fixed from, so the plan depends only on the entries.
pub(super) fn plan_names(
    entries: &[PlanEntry<'_>],
    profile: &NameProfile,
    key: Key,
) -> Vec<OsString> {
    let mut fixed = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.fixing)
        .filter_map(|(index, entry)| {
            let fixed = fix::fix_name(entry.name, profile, entry.is_directory);
            fixed.cause.map(|_| (index, fixed.name.into_owned()))
        })
        .collect::<Vec<_>>();
    let mut planned = entries
        .iter()
        .map(|entry| entry.name.to_os_string())
        .collect::<Vec<_>>();
    if fixed.is_empty() {
        return planned;
    }
    let mut changed = vec![false; entries.len()];
    for (index, _) in &fixed {
        changed[*index] = true;
    }
    let mut taken = entries
        .iter()
        .zip(&changed)
        .filter(|(_, changed)| !**changed)
        .map(|(entry, _)| key.of(entry.name))
        .collect::<HashSet<_>>();
    fixed.sort_by(|(left, _), (right, _)| entries[*left].name.cmp(entries[*right].name));
    for (index, name) in fixed {
        let numbered = numbered_names(&name, entries[index].is_directory, profile.limit);
        planned[index] = std::iter::once(name.clone())
            .chain(numbered)
            .find(|candidate| taken.insert(key.of(candidate)))
            .unwrap_or_else(|| name.clone());
    }
    planned
}

/// A folder's names planned as its children arrive: a child that keeps its
/// name is named at once, and one whose name is fixed waits until all its
/// siblings are known.
pub(super) struct Arrivals {
    fixing: bool,
    profile: NameProfile,
    key: Key,
    kept: Vec<OsString>,
    held: Vec<(OsString, bool)>,
}

pub(super) struct Arrived {
    pub name: OsString,
    pub is_directory: bool,
    pub target: OsString,
}

impl Arrivals {
    pub(super) fn new(fixing: bool, profile: NameProfile, key: Key) -> Self {
        Self {
            fixing,
            profile,
            key,
            kept: Vec::new(),
            held: Vec::new(),
        }
    }

    /// Whether `name` is kept; a name that is not waits for [`Self::planned`].
    pub(super) fn keeps(&mut self, name: &OsStr, is_directory: bool) -> bool {
        if !self.fixing {
            return true;
        }
        if fix::fix_name(name, &self.profile, is_directory)
            .cause
            .is_none()
        {
            self.kept.push(name.to_os_string());
            return true;
        }
        self.held.push((name.to_os_string(), is_directory));
        false
    }

    /// The children that waited, with the names planned for them.
    pub(super) fn planned(self) -> Vec<Arrived> {
        if self.held.is_empty() {
            return Vec::new();
        }
        let kept = self.kept.iter().map(|name| PlanEntry {
            name,
            is_directory: false,
            fixing: false,
        });
        let held = self.held.iter().map(|(name, is_directory)| PlanEntry {
            name,
            is_directory: *is_directory,
            fixing: true,
        });
        let entries = kept.chain(held).collect::<Vec<_>>();
        let targets = plan_names(&entries, &self.profile, self.key);
        self.held
            .into_iter()
            .zip(targets.into_iter().skip(self.kept.len()))
            .map(|((name, is_directory), target)| Arrived {
                name,
                is_directory,
                target,
            })
            .collect()
    }
}
