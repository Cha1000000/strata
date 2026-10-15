// SPDX-License-Identifier: MIT

use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::Path,
};

use super::placement;
use crate::adapters::volume::{NameEncoding, NameLimit, NameProfile, Reserved};

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "transfers fix names with it next")
)]
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

const FAT_INVALID_BYTES: &[u8] = b"\"*/:<>?\\|";

pub(super) fn fat_sanitized_name(name: &OsStr) -> OsString {
    let mut bytes: Vec<u8> = name
        .as_bytes()
        .iter()
        .map(|&byte| {
            if FAT_INVALID_BYTES.contains(&byte) || byte < 0x20 {
                b'_'
            } else {
                byte
            }
        })
        .collect();
    // FAT drivers strip trailing dots and spaces.
    while matches!(bytes.last(), Some(b'.' | b' ')) {
        bytes.pop();
    }
    if bytes.is_empty() {
        bytes.push(b'_');
    }
    OsString::from_vec(bytes)
}

fn fat_name_key(name: &OsStr) -> OsString {
    match name.to_str() {
        Some(name) => OsString::from(name.to_uppercase()),
        None => OsString::from_vec(name.as_bytes().to_ascii_uppercase()),
    }
}

pub(super) fn unique_fat_sibling_name(
    candidate: OsString,
    used: &mut HashSet<OsString>,
) -> OsString {
    if used.insert(fat_name_key(&candidate)) {
        return candidate;
    }
    // vfat, msdos and exFAT all take the Windows formats' length in UTF-16 units.
    let limit = NameProfile::windows(Reserved::Bare, NameEncoding::Utf8).limit;
    let numbered =
        numbered_names(&candidate, false, limit).find(|name| used.insert(fat_name_key(name)));
    numbered.unwrap_or(candidate)
}

pub(super) fn fat_family_child_name(
    name: &OsStr,
    fat_family: bool,
    used: &mut HashSet<OsString>,
) -> OsString {
    if fat_family {
        unique_fat_sibling_name(fat_sanitized_name(name), used)
    } else {
        name.to_os_string()
    }
}
