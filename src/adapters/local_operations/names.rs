// SPDX-License-Identifier: MIT

use std::{ffi::{OsStr, OsString}, os::unix::ffi::{OsStrExt, OsStringExt}, path::Path};
use super::placement;

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

pub(super) fn duplicate_candidate_name(
    base_stem: &OsStr,
    extension: Option<&OsStr>,
    copy_number: u64,
) -> OsString {
    let mut candidate = base_stem.as_bytes().to_vec();
    candidate.extend_from_slice(b" (");
    candidate.extend_from_slice(copy_number.to_string().as_bytes());
    candidate.push(b')');
    if let Some(extension) = extension {
        candidate.push(b'.');
        candidate.extend_from_slice(extension.as_bytes());
    }
    OsString::from_vec(candidate)
}

/// Keep Both's names: "name (1)", or one past the number it has, with a
/// file's extension kept, wrapping past the largest number.
pub(super) fn numbered_names(name: &OsStr, is_directory: bool) -> impl Iterator<Item = OsString> + '_ {
    let (stem, extension) = if is_directory {
        (name, None)
    } else {
        let path = Path::new(name);
        let extension = path.extension().filter(|extension| !extension.is_empty());
        (path.file_stem().unwrap_or(name), extension)
    };
    let (base_stem, copy_num) = parse_copy_suffix(stem);
    let start = copy_num.map_or(1, |number| number + 1);
    (start..=u64::MAX)
        .chain(1..start)
        .take(placement::NUMBERED_NAME_ATTEMPTS as usize)
        .map(move |index| duplicate_candidate_name(base_stem, extension, index))
}

