// SPDX-License-Identifier: MIT

#[cfg(test)]
mod tests;

use rustix::fs::FileType;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdentityTraits {
    pub numbers: InodeNumbers,
    /// Whether a folder keeps its birth time when an entry is renamed into or within it.
    pub folder_births: bool,
    /// Samba makes up file birth times, which a replacement keeping the mtime repeats.
    pub made_up_births: bool,
}

impl Default for IdentityTraits {
    fn default() -> Self {
        Self {
            numbers: InodeNumbers::Unknown,
            folder_births: true,
            made_up_births: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InodeNumbers {
    Kept,
    /// Numbered afresh after eviction, or kept by a name for whatever a rename puts there.
    Renewed,
    /// Compared as kept, but no removal rests on an equal number alone.
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Proof {
    Same,
    Different,
    Unproven,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Attributes {
    pub size: u64,
    pub mtime: (i64, u32),
    pub ctime: Option<(i64, u32)>,
}

impl Attributes {
    fn matches(&self, now: &Self, traits: IdentityTraits) -> bool {
        let changed = traits.made_up_births
            && self
                .ctime
                .zip(now.ctime)
                .is_some_and(|(recorded, now)| recorded != now);
        self.size == now.size && self.mtime == now.mtime && !changed
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Evidence {
    pub device: u64,
    pub inode: u64,
    pub kind: FileType,
    pub birth: Option<(i64, u32)>,
    /// A file's or link's; a folder's own size and times change with its entries.
    pub attributes: Option<Attributes>,
}

impl Evidence {
    pub fn compare(&self, traits: IdentityTraits, current: &Self) -> Proof {
        if current.device != self.device || current.kind != self.kind {
            return Proof::Different;
        }
        let folder = self.kind == FileType::Directory;
        // SMB and NFSv4 servers without one, and some FUSE daemons, report a birth time of zero.
        let reported = |birth: Option<(i64, u32)>| birth.filter(|&birth| birth != (0, 0));
        let births = reported(self.birth)
            .zip(reported(current.birth))
            .filter(|_| !folder || traits.folder_births);
        if births.is_some_and(|(recorded, now)| recorded != now) {
            return Proof::Different;
        }
        if traits.numbers != InodeNumbers::Renewed {
            return if current.inode == self.inode {
                Proof::Same
            } else {
                Proof::Different
            };
        }
        let attributes = self.attributes.zip(current.attributes).filter(|_| !folder);
        if births.is_some() && !(traits.made_up_births && attributes.is_some()) {
            return Proof::Same;
        }
        match attributes {
            Some((recorded, now)) if recorded.matches(&now, traits) => Proof::Same,
            Some(_) => Proof::Different,
            None => Proof::Unproven,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ItemIdentity {
    pub evidence: Evidence,
    pub traits: IdentityTraits,
}

impl ItemIdentity {
    pub fn same(&self, other: &Self) -> bool {
        self.proof(other) == Proof::Same
    }

    /// Where the rule proves nothing either way, the exact number and any birth time decide.
    pub fn unchanged(&self, other: &Self) -> bool {
        match self.proof(other) {
            Proof::Same => true,
            Proof::Different => false,
            Proof::Unproven => {
                let (recorded, now) = (self.evidence, other.evidence);
                recorded.inode == now.inode
                    && recorded
                        .birth
                        .zip(now.birth)
                        .is_none_or(|(recorded, now)| recorded == now)
            }
        }
    }

    pub fn proof(&self, other: &Self) -> Proof {
        let numbers = match (self.traits.numbers, other.traits.numbers) {
            (InodeNumbers::Kept, InodeNumbers::Kept) => InodeNumbers::Kept,
            (InodeNumbers::Renewed, _) | (_, InodeNumbers::Renewed) => InodeNumbers::Renewed,
            _ => InodeNumbers::Unknown,
        };
        let traits = IdentityTraits {
            numbers,
            folder_births: self.traits.folder_births && other.traits.folder_births,
            made_up_births: self.traits.made_up_births || other.traits.made_up_births,
        };
        self.evidence.compare(traits, &other.evidence)
    }
}
