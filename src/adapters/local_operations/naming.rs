// SPDX-License-Identifier: MIT

use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    os::unix::ffi::OsStrExt,
    path::Path,
};

use super::{
    copy_engine::stage,
    placement::{self, PlaceError},
};
use crate::{
    adapters::volume::{NameLimit, NameProfile, StrictRule, Strictness},
    services::RenameCause,
};

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

/// The names a mount that refused `name` may take for it, in a folder whose
/// names follow `rules`, with the Windows rules each needs beyond those and
/// why: the name the folder's rules give, then those each rule that changes
/// that gives alone, then those such rules give together, fewest first, each
/// where it is a new name.
pub(super) fn stricter_names(
    name: &OsStr,
    rules: &NameProfile,
    is_directory: bool,
) -> Vec<(Strictness, OsString, RenameCause)> {
    let fixed = |strictness| fix::fix_name(name, &rules.stricter(strictness), is_directory);
    let own = fixed(Strictness::default()).name;
    let changing = StrictRule::ALL
        .into_iter()
        .filter(|rule| fixed(Strictness::of([*rule])).name != own)
        .collect::<Vec<_>>();
    let mut sets = (0..1_u32 << changing.len()).collect::<Vec<_>>();
    sets.sort_by_key(|set| set.count_ones());
    let mut names = Vec::<(Strictness, OsString, RenameCause)>::new();
    for set in sets {
        let strictness = Strictness::of(
            changing
                .iter()
                .enumerate()
                .filter(|(index, _)| set & 1 << index != 0)
                .map(|(_, rule)| *rule),
        );
        let fixed = fixed(strictness);
        let Some(cause) = fixed.cause else {
            continue;
        };
        if names.iter().all(|(_, tried, _)| **tried != *fixed.name) {
            names.push((strictness, fixed.name.into_owned(), cause));
        }
    }
    names
}

/// Whether a mount may refuse `name` for a reason the Windows rules know.
pub(super) fn may_be_refused(name: &OsStr) -> bool {
    let strictest = NameProfile::posix(None).stricter(Strictness::all());
    fix::fix_name(name, &strictest, false).cause.is_some()
}

/// Keep Both's names: "name (1)", or one past the number it has, with a
/// file's last extension kept, wrapping past the largest number, and each
/// cut to fit `limit`.
pub(super) fn numbered_names(
    name: &OsStr,
    is_directory: bool,
    limit: NameLimit,
) -> impl Iterator<Item = OsString> + '_ {
    numbered_names_after(name, is_directory, limit, 0)
}

/// [`numbered_names`] past the first `skipped`, which are never made.
fn numbered_names_after(
    name: &OsStr,
    is_directory: bool,
    limit: NameLimit,
    skipped: usize,
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
        .skip(skipped)
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Planned {
    pub target: OsString,
    /// Why `target` differs from the name: the fix's cause, or
    /// [`RenameCause::Alias`] where the folder takes a name that needs no
    /// fix for a sibling's that keeps it.
    pub cause: Option<RenameCause>,
}

/// The names of a folder's entries, in their order. A name that needs no
/// fix is kept unless a mount that folds names takes it for another kept
/// name, which sorts first. Fixed names, and the kept names that sort later,
/// are numbered past the kept names and each other in the byte order of the
/// names they come from, so the plan depends only on the entries. Fails
/// when a name has no free number left.
pub(super) fn plan_names(
    entries: &[PlanEntry<'_>],
    profile: &NameProfile,
    key: Key,
) -> Result<Vec<Planned>, PlaceError> {
    let mut planned = entries
        .iter()
        .map(|entry| Planned {
            target: entry.name.to_os_string(),
            cause: None,
        })
        .collect::<Vec<_>>();
    let mut numbered = Vec::new();
    let mut kept = Vec::new();
    for (index, entry) in entries.iter().enumerate().filter(|(_, entry)| entry.fixing) {
        let fixed = fix::fix_name(entry.name, profile, entry.is_directory);
        match fixed.cause {
            Some(cause) => {
                planned[index].cause = Some(cause);
                numbered.push((index, fixed.name.into_owned()));
            }
            None => kept.push(index),
        }
    }
    // Distinct names never meet under exact keys, and kept names meet only
    // where the mount is known to fold them; elsewhere it decides.
    let folds = key == Key::Folded && profile.folds;
    if numbered.is_empty() && (!folds || kept.is_empty()) {
        return Ok(planned);
    }
    let mut taken = entries
        .iter()
        .filter(|entry| !entry.fixing)
        .map(|entry| key.of(entry.name))
        .collect::<HashSet<_>>();
    kept.sort_by_key(|index| entries[*index].name);
    for index in kept {
        if !taken.insert(key.of(entries[index].name)) && folds {
            planned[index].cause = Some(RenameCause::Alias);
            numbered.push((index, entries[index].name.to_os_string()));
        }
    }
    numbered.sort_by(|(left, _), (right, _)| entries[*left].name.cmp(entries[*right].name));
    // Every name an equal one tried is taken, so the next resumes after it.
    let mut tried = HashMap::<(OsString, bool), usize>::new();
    for (index, name) in numbered {
        let is_directory = entries[index].is_directory;
        let tried = tried.entry((name.clone(), is_directory)).or_default();
        let plain = (*tried == 0).then(|| name.clone());
        let skipped = tried.saturating_sub(1);
        let candidates = numbered_names_after(&name, is_directory, profile.limit, skipped);
        let mut free = None;
        for candidate in plain.into_iter().chain(candidates) {
            *tried += 1;
            if taken.insert(key.of(&candidate)) {
                free = Some(candidate);
                break;
            }
        }
        planned[index].target = free.ok_or(PlaceError::Exists)?;
    }
    Ok(planned)
}
