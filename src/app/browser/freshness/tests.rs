// SPDX-License-Identifier: MIT

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ffi::OsString,
    time::{Duration, Instant, UNIX_EPOCH},
};

use super::*;
use crate::{
    model::{EntryKind, FileEntry, MetadataValue},
    services::{
        DirectoryEvent, DirectoryRequest, Evidence, FileSource, IdentityTraits, InodeNumbers,
        ItemIdentity, LocationValidationError, Watching, file_activity::OperationScope,
    },
};

const NFS: u64 = 40;
const DISK: u64 = 7;

#[derive(Clone)]
struct Folder {
    answer: StampAnswer,
    names: Vec<String>,
    watching: Option<Watching>,
}

type Emit = Rc<dyn Fn(DirectoryEvent)>;
type Check = (Location, Rc<dyn Fn(Location, StampAnswer)>);

#[derive(Default)]
struct Folders {
    folders: RefCell<HashMap<Location, Folder>>,
    loads: RefCell<Vec<Location>>,
    checks: RefCell<Vec<Location>>,
    hold_loads: Cell<bool>,
    held_loads: RefCell<Vec<(DirectoryRequest, Emit)>>,
    hold_checks: Cell<bool>,
    held_checks: RefCell<Vec<Check>>,
}

impl Folders {
    fn add(&self, path: &str, stamp: DirectoryStamp, watching: Option<Watching>) -> Location {
        let location = Location::local(path);
        self.folders.borrow_mut().insert(
            location.clone(),
            Folder {
                answer: StampAnswer::Stamp(Box::new(stamp)),
                names: vec!["listed".to_owned()],
                watching,
            },
        );
        location
    }

    fn change(&self, location: &Location, names: &[&str]) {
        let mut folders = self.folders.borrow_mut();
        let folder = folders.get_mut(location).expect("a known folder");
        if let StampAnswer::Stamp(stamp) = &mut folder.answer {
            stamp.modified.0 += 1;
            stamp.changed = stamp.changed.map(|(seconds, nanos)| (seconds + 1, nanos));
        }
        folder.names = names.iter().map(|name| (*name).to_owned()).collect();
    }

    fn loads_of(&self, location: &Location) -> usize {
        self.loads
            .borrow()
            .iter()
            .filter(|loaded| *loaded == location)
            .count()
    }

    fn checks_of(&self, location: &Location) -> usize {
        self.checks
            .borrow()
            .iter()
            .filter(|checked| *checked == location)
            .count()
    }

    fn answer_of(&self, location: &Location) -> StampAnswer {
        self.folders
            .borrow()
            .get(location)
            .map_or(StampAnswer::Missing, |folder| folder.answer.clone())
    }

    fn finish_load(&self, request: &DirectoryRequest, emit: &Emit) {
        let folder = self.folders.borrow().get(&request.location).cloned();
        let request_id = request.id;
        let Some(Folder {
            answer: StampAnswer::Stamp(stamp),
            names,
            ..
        }) = folder
        else {
            emit(DirectoryEvent::Failed {
                request_id,
                message: "No such file or folder".to_owned(),
            });
            return;
        };
        emit(DirectoryEvent::Stamp {
            request_id,
            stamp: *stamp,
        });
        emit(DirectoryEvent::Batch {
            request_id,
            entries: names
                .iter()
                .map(|name| entry(&request.location, name))
                .collect(),
        });
        emit(DirectoryEvent::Finished {
            request_id,
            truncated: false,
            can_trash: None,
            can_delete: None,
        });
    }

    fn finish_held_loads(&self) {
        let held = self.held_loads.take();
        for (request, emit) in held {
            self.finish_load(&request, &emit);
        }
    }

    fn answer_held_checks(&self) {
        let held = self.held_checks.take();
        for (location, emit) in held {
            emit(location.clone(), self.answer_of(&location));
        }
    }
}

impl FileSource for Folders {
    fn validate_location(&self, location: &Location) -> Result<(), LocationValidationError> {
        match self.answer_of(location) {
            StampAnswer::Stamp(_) => Ok(()),
            _ => Err(LocationValidationError::Missing),
        }
    }

    fn enumerate(&self, request: DirectoryRequest, emit: Emit) -> LoadHandle {
        self.loads.borrow_mut().push(request.location.clone());
        if self.hold_loads.get() {
            self.held_loads.borrow_mut().push((request, emit));
        } else {
            self.finish_load(&request, &emit);
        }
        LoadHandle::new(|| {})
    }

    fn start_watch(
        &self,
        location: Location,
        _include_hidden: bool,
        _notify: Rc<dyn Fn(crate::services::DirectoryChange)>,
        report: Rc<dyn Fn(Watching)>,
    ) -> LoadHandle {
        let watching = self
            .folders
            .borrow()
            .get(&location)
            .and_then(|folder| folder.watching);
        if let Some(watching) = watching {
            report(watching);
        }
        LoadHandle::new(|| {})
    }

    fn check_stamps(
        &self,
        locations: Vec<Location>,
        emit: Rc<dyn Fn(Location, StampAnswer)>,
    ) -> LoadHandle {
        for location in locations {
            self.checks.borrow_mut().push(location.clone());
            if self.hold_checks.get() {
                self.held_checks.borrow_mut().push((location, emit.clone()));
            } else {
                emit(location.clone(), self.answer_of(&location));
            }
        }
        LoadHandle::new(|| {})
    }
}

fn entry(folder: &Location, name: &str) -> FileEntry {
    FileEntry {
        location: folder
            .child(std::ffi::OsStr::new(name))
            .expect("a child location"),
        native_name: OsString::from(name),
        thumbnail_path: None,
        display_name: name.to_owned(),
        kind: EntryKind::File,
        size: MetadataValue::Unknown,
        modified_unix_seconds: MetadataValue::Unknown,
        recent_unix_seconds: MetadataValue::Unknown,
        is_hidden: false,
        mode: MetadataValue::Unknown,
        image_dimensions: MetadataValue::Unknown,
        child_count: MetadataValue::Unknown,
        duration_seconds: MetadataValue::Unknown,
        recent_uri: None,
    }
}

fn stamp(mount: u64, inode: u64, outside_writers: bool) -> DirectoryStamp {
    DirectoryStamp {
        modified: (100, 5),
        changed: Some((100, 5)),
        identity: Some(ItemIdentity {
            evidence: Evidence {
                device: mount,
                inode,
                kind: rustix::fs::FileType::Directory,
                birth: Some((1, 0)),
                handle: None,
                attributes: None,
            },
            traits: IdentityTraits {
                numbers: InodeNumbers::Kept,
                folder_births: true,
                made_up_births: false,
            },
        }),
        mount: Some(MountToken::Native(mount)),
        outside_writers,
        coarse_window: None,
        read_at: UNIX_EPOCH + Duration::from_secs(1_000),
    }
}

fn network(inode: u64) -> DirectoryStamp {
    stamp(NFS, inode, true)
}

fn local(inode: u64) -> DirectoryStamp {
    stamp(DISK, inode, false)
}

struct Clock {
    browser: Rc<Browser>,
    start: Instant,
}

impl Clock {
    fn new(browser: &Rc<Browser>) -> Self {
        let clock = Self {
            browser: browser.clone(),
            start: Instant::now(),
        };
        clock.at(0);
        clock
    }

    fn at(&self, seconds: u64) {
        self.browser
            .freshness
            .clock
            .set(Some(self.start + Duration::from_secs(seconds)));
    }

    fn tick_at(&self, seconds: u64) {
        self.at(seconds);
        self.browser.freshness_tick();
    }

    fn armed(&self) -> Option<Duration> {
        self.browser
            .freshness
            .armed_for
            .get()
            .map(|due| due.saturating_duration_since(self.start))
    }
}

fn browser_on(source: &Rc<Folders>) -> (Rc<Browser>, Clock) {
    let browser = Browser::new(source.clone());
    let clock = Clock::new(&browser);
    (browser, clock)
}

fn names_at(browser: &Browser, depth: usize) -> Vec<String> {
    browser.state.borrow().columns[depth]
        .entries
        .iter()
        .map(|entry| entry.display_name.clone())
        .collect()
}

#[test]
fn a_folder_other_writers_change_relists_when_its_stamp_moves() {
    let source = Rc::new(Folders::default());
    let share = source.add("/net/share", network(1), Some(Watching::Live));
    let (browser, clock) = browser_on(&source);
    browser.navigate(share.clone());

    clock.tick_at(15);
    let unchanged = (source.checks_of(&share), source.loads_of(&share));
    source.change(&share, &["listed", "added elsewhere"]);
    clock.tick_at(30);

    assert_eq!(unchanged, (1, 1));
    assert_eq!((source.checks_of(&share), source.loads_of(&share)), (2, 2));
    assert_eq!(names_at(&browser, 0), ["added elsewhere", "listed"]);
}

#[test]
fn an_unchanged_stamp_never_relists_while_a_changed_one_does() {
    let source = Rc::new(Folders::default());
    let share = source.add("/net/share", network(1), Some(Watching::Live));
    let sub = source.add("/net/share/sub", network(2), Some(Watching::Live));
    let (browser, clock) = browser_on(&source);
    browser.navigate(share.clone());
    browser.show_child(0, sub.clone());
    source.change(&sub, &["listed", "added elsewhere"]);

    clock.tick_at(15);

    assert_eq!((source.checks_of(&share), source.checks_of(&sub)), (1, 1));
    assert_eq!((source.loads_of(&share), source.loads_of(&sub)), (1, 2));
    assert_eq!(names_at(&browser, 1), ["added elsewhere", "listed"]);
}

#[test]
fn a_folder_only_this_kernel_writes_is_never_checked_while_a_network_folder_is() {
    let source = Rc::new(Folders::default());
    let home = source.add("/home/me", local(1), Some(Watching::Live));
    let nas = source.add("/home/me/nas", network(2), Some(Watching::Live));
    let (browser, clock) = browser_on(&source);
    browser.navigate(home.clone());
    let armed_for_home_alone = clock.armed();
    browser.show_child(0, nas.clone());
    let armed_with_the_share = clock.armed();

    clock.tick_at(15);

    assert_eq!(armed_for_home_alone, None);
    assert_eq!(armed_with_the_share, Some(Duration::from_secs(15)));
    assert_eq!((source.checks_of(&home), source.checks_of(&nas)), (0, 1));
}

#[test]
fn a_folder_without_a_monitor_is_checked() {
    let mut failures = Vec::new();
    for (watching, checked) in [
        (None, true),
        (Some(Watching::Unavailable), true),
        (Some(Watching::Live), false),
    ] {
        let source = Rc::new(Folders::default());
        let folder = source.add("/home/me/docs", local(1), watching);
        let (browser, clock) = browser_on(&source);
        browser.navigate(folder.clone());
        let armed = clock.armed().is_some();

        clock.tick_at(15);

        let got = (armed, source.checks_of(&folder));
        let expected = (checked, usize::from(checked));
        if got != expected {
            failures.push(format!("{watching:?}: got {got:?}, expected {expected:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn checks_stop_while_hidden_and_resume_on_focus() {
    let source = Rc::new(Folders::default());
    let share = source.add("/net/share", network(1), Some(Watching::Live));
    let (browser, clock) = browser_on(&source);
    browser.navigate(share.clone());
    browser.set_freshness(FreshnessInput {
        presence: Presence::Hidden,
        ..browser.freshness_input()
    });
    let armed_while_hidden = clock.armed();

    clock.tick_at(15);
    let checked_while_hidden = source.checks_of(&share);
    browser.set_freshness(FreshnessInput {
        presence: Presence::Focused,
        ..browser.freshness_input()
    });
    clock.tick_at(16);
    let checked_on_focus = source.checks_of(&share);
    clock.tick_at(18);
    let checked_before_the_focused_interval = source.checks_of(&share);
    clock.tick_at(19);

    assert_eq!(armed_while_hidden, None);
    assert_eq!(
        (
            checked_while_hidden,
            checked_on_focus,
            checked_before_the_focused_interval
        ),
        (0, 1, 1)
    );
    assert_eq!(source.checks_of(&share), 2);
}

#[test]
fn a_stale_folder_waits_for_an_open_inline_edit() {
    let source = Rc::new(Folders::default());
    let share = source.add("/net/share", network(1), Some(Watching::Live));
    let (browser, clock) = browser_on(&source);
    let editing = Rc::new(Cell::new(true));
    let held = editing.clone();
    browser.set_freshness_hold(move || held.get());
    browser.navigate(share.clone());
    source.change(&share, &["listed", "added elsewhere"]);

    clock.tick_at(15);
    clock.tick_at(30);
    let while_editing = (source.checks_of(&share), source.loads_of(&share));
    editing.set(false);
    clock.tick_at(45);

    assert_eq!(while_editing, (2, 1));
    assert_eq!((source.checks_of(&share), source.loads_of(&share)), (3, 2));
}

#[test]
fn a_relist_waits_ten_times_its_own_duration() {
    let source = Rc::new(Folders::default());
    let share = source.add("/net/share", network(1), Some(Watching::Live));
    let (browser, clock) = browser_on(&source);
    browser.navigate(share.clone());
    source.hold_loads.set(true);
    source.change(&share, &["listed", "added elsewhere"]);

    clock.tick_at(15);
    clock.at(20);
    source.finish_held_loads();
    let relisted = source.loads_of(&share);
    clock.tick_at(69);
    let checks_within_ten_listings = source.checks_of(&share);
    clock.tick_at(70);

    assert_eq!(relisted, 2);
    assert_eq!(names_at(&browser, 0), ["added elsewhere", "listed"]);
    assert_eq!(checks_within_ten_listings, 1);
    assert_eq!(source.checks_of(&share), 2);
}

#[test]
fn an_unchanged_folder_is_checked_less_often_until_it_changes() {
    let source = Rc::new(Folders::default());
    let share = source.add("/net/share", network(1), Some(Watching::Live));
    let (browser, clock) = browser_on(&source);
    browser.set_freshness(FreshnessInput {
        presence: Presence::Focused,
        ..browser.freshness_input()
    });
    browser.navigate(share.clone());

    for second in (3..=60).step_by(3) {
        clock.tick_at(second);
    }
    clock.tick_at(63);
    let after_twenty_unchanged = source.checks_of(&share);
    clock.tick_at(66);
    let at_twice_the_interval = source.checks_of(&share);
    source.change(&share, &["listed", "added elsewhere"]);
    clock.tick_at(72);
    clock.tick_at(75);

    assert_eq!((after_twenty_unchanged, at_twice_the_interval), (20, 21));
    assert_eq!(source.loads_of(&share), 2);
    assert_eq!(source.checks_of(&share), 23);
}

#[test]
fn an_answer_after_navigation_is_ignored_while_the_current_column_s_answer_applies() {
    let source = Rc::new(Folders::default());
    let share = source.add("/net/share", network(1), Some(Watching::Live));
    let sub = source.add("/net/share/sub", network(2), Some(Watching::Live));
    let other = source.add("/net/share/other", network(3), Some(Watching::Live));
    let (browser, clock) = browser_on(&source);
    browser.navigate(share.clone());
    browser.show_child(0, sub.clone());
    source.hold_checks.set(true);

    clock.tick_at(15);
    browser.show_child(0, other.clone());
    source.change(&share, &["listed", "added elsewhere"]);
    source.change(&sub, &["listed", "added elsewhere"]);
    source.answer_held_checks();

    assert_eq!(
        (
            source.loads_of(&share),
            source.loads_of(&sub),
            source.loads_of(&other)
        ),
        (2, 1, 1)
    );
    assert_eq!(names_at(&browser, 0), ["added elsewhere", "listed"]);
}

#[test]
fn checks_skip_folders_an_operation_changes_and_resume_after_it() {
    let mut failures = Vec::new();
    for (row, path) in [
        ("renaming the folder", "/freshness/renamed"),
        ("renaming inside the folder", "/freshness/written"),
    ] {
        let source = Rc::new(Folders::default());
        let share = source.add(path, network(1), Some(Watching::Live));
        let (browser, clock) = browser_on(&source);
        browser.navigate(share.clone());
        source.change(&share, &["listed", "added elsewhere"]);
        let folder = share.native_path().expect("a native folder").to_path_buf();
        let scope = match row {
            "renaming the folder" => OperationScope {
                roots: vec![folder],
                folders: Vec::new(),
            },
            _ => OperationScope {
                roots: vec![folder.join("listed")],
                folders: vec![folder],
            },
        };
        let activity = file_activity::begin(scope);

        clock.tick_at(15);
        let during = (source.checks_of(&share), source.loads_of(&share));
        drop(activity);
        clock.tick_at(30);

        let got = (during, source.checks_of(&share), source.loads_of(&share));
        if got != ((0, 1), 1, 2) {
            failures.push(format!("{row}: got {got:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
