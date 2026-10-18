// SPDX-License-Identifier: MIT

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ffi::OsString,
    time::{Duration, Instant, UNIX_EPOCH},
};

use super::*;
use crate::{
    app::BrowserEvent,
    model::{EntryKind, FileEntry, MetadataValue},
    services::{
        DirectoryChange, DirectoryEvent, DirectoryRequest, Evidence, FileSource, IdentityTraits,
        InodeNumbers, ItemIdentity, LocationValidationError, Watching,
        file_activity::OperationScope,
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
type Notify = Rc<dyn Fn(DirectoryChange)>;
type Validation = (
    Location,
    Rc<dyn Fn(Result<(), LocationValidationError>)>,
    Rc<Cell<bool>>,
);

#[derive(Default)]
struct Folders {
    folders: RefCell<HashMap<Location, Folder>>,
    loads: RefCell<Vec<Location>>,
    checks: RefCell<Vec<Location>>,
    hold_loads: Cell<bool>,
    held_loads: RefCell<Vec<(DirectoryRequest, Emit)>>,
    fail_loads: Cell<bool>,
    truncate_loads: Cell<bool>,
    responsiveness: Cell<Responsiveness>,
    followers: RefCell<Vec<Rc<dyn Fn()>>>,
    probes_run_out_of_time: Cell<bool>,
    hold_checks: Cell<bool>,
    held_checks: RefCell<Vec<Check>>,
    monitors: RefCell<HashMap<Location, Notify>>,
    watch_requests: RefCell<Vec<Location>>,
    validations: RefCell<Vec<Location>>,
    hold_validations: Cell<bool>,
    held_validations: RefCell<Vec<Validation>>,
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

    fn begin_load(&self, request: &DirectoryRequest, emit: &Emit) -> bool {
        let folder = self.folders.borrow().get(&request.location).cloned();
        let request_id = request.id;
        let Some(Folder {
            answer: StampAnswer::Stamp(stamp),
            names,
            ..
        }) = folder.filter(|_| self.answering())
        else {
            emit(DirectoryEvent::Failed {
                request_id,
                message: "The operation timed out".to_owned(),
            });
            return false;
        };
        emit(DirectoryEvent::Stamp { request_id, stamp });
        emit(DirectoryEvent::Batch {
            request_id,
            entries: names
                .iter()
                .map(|name| entry(&request.location, name))
                .collect(),
        });
        true
    }

    fn finish_load(&self, request: &DirectoryRequest, emit: &Emit) {
        let request_id = request.id;
        emit(match self.fail_loads.get() {
            true => DirectoryEvent::Failed {
                request_id,
                message: "A device input/output error occurred".to_owned(),
            },
            false => DirectoryEvent::Finished {
                request_id,
                truncated: self.truncate_loads.get(),
                can_trash: None,
                can_delete: None,
            },
        });
    }

    fn finish_held_loads(&self) {
        let held = self.held_loads.take();
        for (request, emit) in held {
            self.finish_load(&request, &emit);
        }
    }

    fn answer(&self, location: &Location, answer: StampAnswer) {
        let mut folders = self.folders.borrow_mut();
        folders.get_mut(location).expect("a known folder").answer = answer;
    }

    fn remove(&self, location: &Location) {
        self.folders.borrow_mut().remove(location);
    }

    fn replace(&self, location: &Location, inode: u64, names: &[&str]) {
        let mut folders = self.folders.borrow_mut();
        let folder = folders.get_mut(location).expect("a known folder");
        if let StampAnswer::Stamp(stamp) = &mut folder.answer
            && let Some(identity) = &mut stamp.identity
        {
            identity.evidence.inode = inode;
        }
        folder.names = names.iter().map(|name| (*name).to_owned()).collect();
    }

    fn answer_held_checks_for(&self, folder: &Location, answer: &StampAnswer) {
        let held = self.held_checks.take();
        for (location, emit) in held {
            let answer = match location == *folder {
                true => answer.clone(),
                false => self.answer_of(&location),
            };
            emit(location, answer);
        }
    }

    fn monitors_requested(&self, location: &Location) -> usize {
        self.watch_requests
            .borrow()
            .iter()
            .filter(|watched| *watched == location)
            .count()
    }

    fn notify(&self, location: &Location, change: DirectoryChange) {
        let notify = self.monitors.borrow().get(location).cloned();
        notify.expect("a requested monitor")(change);
    }

    fn answer_held_checks(&self) {
        let held = self.held_checks.take();
        for (location, emit) in held {
            emit(location.clone(), self.answer_of(&location));
        }
    }

    fn answer_held_check(&self, folder: &Location) {
        let held = std::mem::take(&mut *self.held_checks.borrow_mut());
        let (answered, waiting): (Vec<_>, Vec<_>) = held
            .into_iter()
            .partition(|(location, _)| location == folder);
        self.held_checks.replace(waiting);
        for (location, emit) in answered {
            emit(location.clone(), self.answer_of(&location));
        }
    }

    fn answer_held_validations(&self) {
        let held = self.held_validations.take();
        for (location, emit, cancelled) in held {
            if !cancelled.get() {
                emit(self.validate_location(&location));
            }
        }
    }

    fn answering(&self) -> bool {
        self.responsiveness.get() == Responsiveness::Answering
    }

    fn mount_of(&self, location: &Location) -> Option<MountToken> {
        match self.answer_of(location) {
            StampAnswer::Stamp(stamp) => stamp.mount,
            _ => None,
        }
    }

    fn lane_busy(&self, location: &Location) -> bool {
        let mount = self.mount_of(location);
        mount.is_some()
            && (self.held_loads.borrow().iter().any(|(request, _)| {
                request.background && self.mount_of(&request.location) == mount
            }) || self
                .held_checks
                .borrow()
                .iter()
                .any(|(checked, _)| self.mount_of(checked) == mount))
    }
}

impl FileSource for Folders {
    fn validate_location(&self, location: &Location) -> Result<(), LocationValidationError> {
        match self.answer_of(location) {
            StampAnswer::Stamp(_) => Ok(()),
            _ => Err(LocationValidationError::Missing),
        }
    }

    fn validate_location_async(
        &self,
        location: Location,
        emit: Rc<dyn Fn(Result<(), LocationValidationError>)>,
    ) -> LoadHandle {
        self.validations.borrow_mut().push(location.clone());
        if !self.hold_validations.get() {
            emit(self.validate_location(&location));
            return LoadHandle::new(|| {});
        }
        let cancelled = Rc::new(Cell::new(false));
        self.held_validations
            .borrow_mut()
            .push((location, emit, cancelled.clone()));
        LoadHandle::new(move || cancelled.set(true))
    }

    fn enumerate(&self, request: DirectoryRequest, emit: Emit) -> LoadHandle {
        self.loads.borrow_mut().push(request.location.clone());
        if request.background && self.lane_busy(&request.location) {
            emit(DirectoryEvent::Failed {
                request_id: request.id,
                message: "The operation timed out".to_owned(),
            });
            return LoadHandle::new(|| {});
        }
        if !self.answering() && self.probes_run_out_of_time.get() {
            emit(DirectoryEvent::Finished {
                request_id: request.id,
                truncated: true,
                can_trash: None,
                can_delete: None,
            });
            return LoadHandle::new(|| {});
        }
        if !self.begin_load(&request, &emit) {
            return LoadHandle::new(|| {});
        }
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
        notify: Notify,
        report: Rc<dyn Fn(Watching)>,
    ) -> LoadHandle {
        self.monitors.borrow_mut().insert(location.clone(), notify);
        self.watch_requests.borrow_mut().push(location.clone());
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

    fn responsiveness(&self, _location: &Location) -> Responsiveness {
        self.responsiveness.get()
    }

    fn follow_responsiveness(&self, notify: Rc<dyn Fn()>) -> LoadHandle {
        self.followers.borrow_mut().push(notify);
        LoadHandle::new(|| {})
    }

    fn check_stamps(
        &self,
        locations: Vec<Location>,
        emit: Rc<dyn Fn(Location, StampAnswer)>,
    ) -> LoadHandle {
        let busy: Vec<bool> = locations
            .iter()
            .map(|location| self.lane_busy(location))
            .collect();
        for (location, busy) in locations.into_iter().zip(busy) {
            self.checks.borrow_mut().push(location.clone());
            if busy || !self.answering() {
                emit(location, StampAnswer::Unresponsive);
            } else if self.hold_checks.get() {
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
        daemon_cache: None,
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
    asleep: Cell<Duration>,
}

impl Clock {
    fn new(browser: &Rc<Browser>) -> Self {
        let clock = Self {
            browser: browser.clone(),
            start: Instant::now(),
            asleep: Cell::new(Duration::ZERO),
        };
        clock.at(0);
        clock
    }

    fn at(&self, seconds: u64) {
        let awake = Duration::from_secs(seconds);
        self.browser
            .freshness
            .clock
            .set(Some((self.start + awake, awake + self.asleep.get())));
    }

    fn sleep(&self, seconds: u64) {
        self.asleep
            .set(self.asleep.get() + Duration::from_secs(seconds));
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

fn recorded(browser: &Browser) -> Rc<RefCell<Vec<BrowserEvent>>> {
    let events = Rc::new(RefCell::new(Vec::new()));
    let observed = events.clone();
    browser.observe(move |event| observed.borrow_mut().push(event.clone()));
    events
}

fn count(events: &RefCell<Vec<BrowserEvent>>, wanted: impl Fn(&BrowserEvent) -> bool) -> usize {
    events.borrow().iter().filter(|event| wanted(event)).count()
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
fn folders_changed_on_one_mount_relist_one_after_the_other() {
    let source = Rc::new(Folders::default());
    let share = source.add("/net/share", network(1), Some(Watching::Live));
    let sub = source.add("/net/share/sub", network(2), Some(Watching::Live));
    let (browser, clock) = browser_on(&source);
    browser.navigate(share.clone());
    browser.show_child(0, sub.clone());
    source.change(&share, &["listed", "added elsewhere"]);
    source.change(&sub, &["listed", "added elsewhere"]);
    source.hold_loads.set(true);

    clock.tick_at(15);
    let while_the_share_lists = (source.loads_of(&share), source.loads_of(&sub));
    source.finish_held_loads();
    source.finish_held_loads();

    assert_eq!(while_the_share_lists, (2, 1));
    assert_eq!(names_at(&browser, 0), ["added elsewhere", "listed"]);
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
fn with_network_folders_unwatched_only_local_folders_without_a_monitor_are_checked() {
    let source = Rc::new(Folders::default());
    let docs = source.add("/home/me/docs", local(1), None);
    let nas = source.add("/home/me/docs/nas", network(2), None);
    let (browser, clock) = browser_on(&source);
    let watch = |watch_network_folders| {
        browser.set_freshness(FreshnessInput {
            presence: Presence::Focused,
            watch_network_folders,
            ..browser.freshness_input()
        });
    };
    watch(false);
    browser.navigate(docs.clone());
    browser.show_child(0, nas.clone());
    source.change(&nas, &["listed", "added elsewhere"]);

    clock.tick_at(3);
    let unwatched = (source.checks_of(&docs), source.checks_of(&nas));
    watch(true);
    clock.tick_at(6);

    assert_eq!(unwatched, (1, 0));
    assert_eq!((source.checks_of(&docs), source.checks_of(&nas)), (2, 1));
    assert_eq!(names_at(&browser, 1), ["added elsewhere", "listed"]);
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
    let checked_on_focus = source.checks_of(&share);
    clock.tick_at(17);
    let checked_before_the_focused_interval = source.checks_of(&share);
    clock.tick_at(18);

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
fn power_saver_slows_focused_checks_and_stops_unfocused_ones() {
    let source = Rc::new(Folders::default());
    let share = source.add("/net/share", network(1), Some(Watching::Live));
    let (browser, clock) = browser_on(&source);
    browser.set_freshness(FreshnessInput {
        presence: Presence::Focused,
        power_saver: true,
        ..browser.freshness_input()
    });
    browser.navigate(share.clone());
    let armed_focused = clock.armed();

    clock.tick_at(11);
    let before_four_intervals = source.checks_of(&share);
    clock.tick_at(12);
    let at_four_intervals = source.checks_of(&share);
    browser.set_freshness(FreshnessInput {
        presence: Presence::Visible,
        ..browser.freshness_input()
    });
    let armed_unfocused = clock.armed();
    clock.tick_at(60);
    let unfocused = source.checks_of(&share);
    browser.set_freshness(FreshnessInput {
        power_saver: false,
        ..browser.freshness_input()
    });
    clock.tick_at(61);

    assert_eq!(armed_focused, Some(Duration::from_secs(12)));
    assert_eq!((before_four_intervals, at_four_intervals), (0, 1));
    assert_eq!((armed_unfocused, unfocused), (None, 1));
    assert_eq!(source.checks_of(&share), 2);
}

#[derive(Clone, Copy)]
enum Blocker {
    InlineEdit,
    Operation,
}

#[test]
fn focus_after_thirty_seconds_relists_visible_checked_folders() {
    let mut failures = Vec::new();
    for (row, away, asleep, blocker, expected) in [
        ("away 29 s", 29, 0, None, (1, 1, 1)),
        ("away 30 s", 30, 0, None, (0, 2, 2)),
        (
            "away 20 s around 8 h asleep",
            20,
            8 * 3_600,
            None,
            (0, 2, 2),
        ),
        (
            "an inline edit open at focus",
            40,
            0,
            Some(Blocker::InlineEdit),
            (1, 1, 2),
        ),
        (
            "an operation writing into the folder at focus",
            40,
            0,
            Some(Blocker::Operation),
            (0, 1, 2),
        ),
    ] {
        let source = Rc::new(Folders::default());
        let home = source.add("/freshness/focus", local(1), Some(Watching::Live));
        let nas = source.add("/freshness/focus/nas", network(2), Some(Watching::Live));
        let (browser, clock) = browser_on(&source);
        let editing = Rc::new(Cell::new(false));
        let held = editing.clone();
        browser.set_freshness_hold(move || held.get());
        let focus = |presence| {
            browser.set_freshness(FreshnessInput {
                presence,
                ..browser.freshness_input()
            });
        };
        focus(Presence::Focused);
        browser.navigate(home.clone());
        browser.show_child(0, nas.clone());
        clock.at(1);
        focus(Presence::Visible);
        clock.sleep(asleep);

        clock.at(1 + away);
        editing.set(matches!(blocker, Some(Blocker::InlineEdit)));
        let folder = nas.native_path().expect("a native folder").to_path_buf();
        let activity = matches!(blocker, Some(Blocker::Operation)).then(|| {
            file_activity::begin(OperationScope {
                roots: vec![folder.join("listed")],
                folders: vec![folder],
            })
        });
        focus(Presence::Focused);
        let at_focus = (source.checks_of(&nas), source.loads_of(&nas));
        editing.set(false);
        drop(activity);
        for second in 2 + away..=61 + away {
            clock.tick_at(second);
        }

        let got = (at_focus.0, at_focus.1, source.loads_of(&nas));
        if got != expected || source.loads_of(&home) != 1 {
            failures.push(format!(
                "{row}: got (checks at focus, network loads at focus, network loads a minute \
                 later) {got:?} and {} home loads, expected {expected:?} and 1",
                source.loads_of(&home)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn a_focus_relist_waits_for_the_relist_duty_cycle() {
    let mut failures = Vec::new();
    for (row, finished, left, back, changed, floor, checks) in [
        ("back after 1 s to a change", 13, 14, 15, true, 113, 2),
        ("back after 30 s", 13, 14, 44, false, 113, 2),
        (
            "back after 30 s while the re-list runs",
            36,
            4,
            34,
            false,
            366,
            1,
        ),
    ] {
        let source = Rc::new(Folders::default());
        let share = source.add("/freshness/duty", network(1), Some(Watching::Live));
        let (browser, clock) = browser_on(&source);
        let focus = |presence| {
            browser.set_freshness(FreshnessInput {
                presence,
                ..browser.freshness_input()
            });
        };
        let finish = || {
            source.finish_held_loads();
            source.hold_loads.set(false);
        };
        focus(Presence::Focused);
        browser.navigate(share.clone());
        source.change(&share, &["listed", "a"]);
        source.hold_loads.set(true);
        clock.tick_at(3);
        if finished < left {
            clock.at(finished);
            finish();
        }
        if changed {
            source.change(&share, &["listed", "a", "b"]);
        }
        clock.at(left);
        focus(Presence::Visible);

        clock.at(back);
        focus(Presence::Focused);
        let on_return = source.loads_of(&share);
        if finished > back {
            clock.at(finished);
            finish();
        }
        for second in back.max(finished) + 1..floor {
            clock.tick_at(second);
        }
        let before_the_floor = (source.loads_of(&share), source.checks_of(&share));
        clock.tick_at(floor);

        let got = (
            on_return,
            before_the_floor.0,
            source.loads_of(&share),
            before_the_floor.1,
        );
        let expected = (2, 2, 3, checks);
        if got != expected {
            failures.push(format!(
                "{row}: got (loads on return, before {floor} s, at {floor} s, checks before \
                 {floor} s) {got:?}, expected {expected:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
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
    let mut failures = Vec::new();
    for (row, truncated, failed, shown) in [
        ("shown", false, false, vec!["added elsewhere", "listed"]),
        ("cut short", true, false, vec!["listed"]),
        ("failed", false, true, vec!["listed"]),
    ] {
        let source = Rc::new(Folders::default());
        let share = source.add("/net/share", network(1), Some(Watching::Live));
        let (browser, clock) = browser_on(&source);
        browser.navigate(share.clone());
        source.hold_loads.set(true);
        source.truncate_loads.set(truncated);
        source.fail_loads.set(failed);
        source.change(&share, &["listed", "added elsewhere"]);

        clock.tick_at(15);
        clock.at(20);
        source.finish_held_loads();
        let relisted = (source.loads_of(&share), names_at(&browser, 0));
        clock.tick_at(69);
        let checks_within_ten_listings = source.checks_of(&share);
        clock.tick_at(70);

        let got = (
            relisted,
            checks_within_ten_listings,
            source.checks_of(&share),
        );
        let expected = (
            (2, shown.iter().map(|name| (*name).to_owned()).collect()),
            1,
            2,
        );
        if got != expected {
            failures.push(format!("{row}: got {got:?}, expected {expected:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
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

#[test]
fn a_background_relist_that_fails_keeps_the_previous_listing() {
    let source = Rc::new(Folders::default());
    let share = source.add("/net/share", network(1), Some(Watching::Live));
    let (browser, clock) = browser_on(&source);
    browser.navigate(share.clone());
    let events = recorded(&browser);
    source.change(&share, &["listed", "added elsewhere"]);
    source.hold_loads.set(true);
    source.fail_loads.set(true);

    clock.tick_at(15);
    source.notify(&share, DirectoryChange::Upsert(entry(&share, "made here")));
    source.finish_held_loads();
    let kept = (
        names_at(&browser, 0),
        browser.state.borrow().columns[0].load_state.clone(),
    );
    source.hold_loads.set(false);
    source.fail_loads.set(false);
    clock.tick_at(30);

    assert_eq!(
        kept,
        (
            vec!["listed".to_owned(), "made here".to_owned()],
            LoadState::Ready
        )
    );
    assert_eq!(
        count(&events, |event| matches!(
            event,
            BrowserEvent::LoadFailed { .. }
        )),
        0
    );
    assert_eq!(
        count(&events, |event| matches!(
            event,
            BrowserEvent::LoadFinished { depth: 0, .. }
        )),
        2
    );
    assert_eq!(source.loads_of(&share), 3);
    assert_eq!(names_at(&browser, 0), ["added elsewhere", "listed"]);
}

#[test]
fn a_time_truncated_relist_does_not_replace_a_complete_listing() {
    let _serial = crate::test_support::ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .expect("the async test lock should not be poisoned");
    let at_the_cap: Vec<String> = (0..super::super::MAX_DIRECTORY_ENTRIES)
        .map(|index| format!("entry {index}"))
        .collect();
    let mut failures = Vec::new();
    for (row, truncated_before, listed, kept) in [
        ("complete before", false, 2, true),
        ("truncated before", true, 2, false),
        (
            "complete before, cut at the entry cap",
            false,
            at_the_cap.len(),
            false,
        ),
    ] {
        let source = Rc::new(Folders::default());
        let share = source.add("/net/share", network(1), Some(Watching::Live));
        let (browser, clock) = browser_on(&source);
        source.truncate_loads.set(truncated_before);
        browser.navigate(share.clone());
        let names: Vec<&str> = match listed {
            2 => vec!["listed", "added elsewhere"],
            _ => at_the_cap.iter().map(String::as_str).collect(),
        };
        source.change(&share, &names);
        source.truncate_loads.set(true);

        clock.tick_at(15);
        let deadline = Instant::now() + Duration::from_secs(30);
        while !browser.settled(0) && Instant::now() < deadline {
            glib::MainContext::default().iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        }

        let got = (
            names_at(&browser, 0).len(),
            browser.state.borrow().columns[0].truncated,
            source.validations.borrow().len(),
        );
        let expected = match kept {
            true => (1, false, 0),
            false => (listed, true, 0),
        };
        if got != expected {
            failures.push(format!("{row}: got {got:?}, expected {expected:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn a_refresh_on_a_mount_that_stopped_answering_keeps_its_rows() {
    let mut failures = Vec::new();
    for (stalled, relisting_truncated, probe_runs_out_of_time) in [
        (false, false, false),
        (true, false, false),
        (true, true, false),
        (true, false, true),
    ] {
        let source = Rc::new(Folders::default());
        let share = source.add("/net/share", network(1), Some(Watching::Live));
        let (browser, clock) = browser_on(&source);
        source.truncate_loads.set(relisting_truncated);
        source.probes_run_out_of_time.set(probe_runs_out_of_time);
        browser.navigate(share.clone());
        let events = recorded(&browser);
        if relisting_truncated {
            source.change(&share, &["listed", "added elsewhere"]);
            source.hold_loads.set(true);
            clock.tick_at(15);
        }
        if stalled {
            source.responsiveness.set(Responsiveness::Stalled);
        }

        browser.refresh_all();

        let got = (
            names_at(&browser, 0),
            count(&events, |event| {
                matches!(event, BrowserEvent::ColumnReloaded { .. })
            }),
            count(&events, |event| {
                matches!(event, BrowserEvent::LoadFailed { .. })
            }),
            browser.state.borrow().columns[0].truncated,
        );
        let expected = (
            vec!["listed".to_owned()],
            usize::from(!stalled),
            0,
            relisting_truncated,
        );
        if got != expected {
            failures.push(format!(
                "stalled {stalled}, re-listing a truncated listing {relisting_truncated}, \
                 the refresh running out of time {probe_runs_out_of_time}: \
                 got {got:?}, expected {expected:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn notices(events: &RefCell<Vec<BrowserEvent>>) -> Vec<(usize, bool)> {
    events
        .borrow()
        .iter()
        .filter_map(|event| match event {
            BrowserEvent::ColumnNotResponding {
                depth,
                not_responding,
            } => Some((*depth, *not_responding)),
            _ => None,
        })
        .collect()
}

fn notice_shown(browser: &Browser, depth: usize) -> bool {
    browser
        .column_snapshot(depth)
        .is_some_and(|column| column.not_responding)
}

#[test]
fn a_mount_that_stops_answering_shows_the_notice_until_it_answers() {
    let source = Rc::new(Folders::default());
    let share = source.add("/net/share", network(1), Some(Watching::Live));
    let (browser, clock) = browser_on(&source);
    browser.navigate(share.clone());
    let events = recorded(&browser);

    source.responsiveness.set(Responsiveness::Stalled);
    clock.tick_at(15);
    let stalled = (notices(&events), browser.freshness.following.get());
    source.responsiveness.set(Responsiveness::NotResponding);
    browser.follow_responsiveness();
    let not_responding = (
        notices(&events),
        notice_shown(&browser, 0),
        names_at(&browser, 0),
        source.loads_of(&share),
    );
    source.responsiveness.set(Responsiveness::Answering);
    browser.follow_responsiveness();

    assert_eq!(stalled, (vec![], true));
    assert_eq!(
        not_responding,
        (vec![(0, true)], true, vec!["listed".to_owned()], 1)
    );
    assert_eq!(notices(&events), [(0, true), (0, false)]);
    assert!(!notice_shown(&browser, 0));
    assert!(!browser.freshness.following.get());
}

#[test]
fn a_mount_that_stops_answering_a_request_from_elsewhere_shows_the_notice() {
    let source = Rc::new(Folders::default());
    let share = source.add("/net/share", network(1), Some(Watching::Live));
    let (browser, _clock) = browser_on(&source);
    browser.set_freshness(FreshnessInput {
        presence: Presence::Focused,
        watch_network_folders: false,
        ..browser.freshness_input()
    });
    browser.navigate(share);
    let idle = (
        browser.freshness.armed_for.get(),
        browser.freshness.following.get(),
    );

    source.responsiveness.set(Responsiveness::NotResponding);
    let followers = source.followers.borrow().clone();
    for notify in followers {
        notify();
    }

    assert_eq!(idle, (None, false));
    assert!(notice_shown(&browser, 0));
}

#[test]
fn the_notice_stays_off_a_folder_whose_load_failed() {
    let source = Rc::new(Folders::default());
    let share = source.add("/net/share", network(1), Some(Watching::Live));
    let sub = source.add("/net/share/sub", network(2), Some(Watching::Live));
    let (browser, _clock) = browser_on(&source);
    browser.navigate(share);
    source.fail_loads.set(true);
    browser.show_child(0, sub);
    let events = recorded(&browser);

    source.responsiveness.set(Responsiveness::NotResponding);
    browser.follow_responsiveness();

    assert_eq!(notices(&events), [(0, true)]);
    assert!(!notice_shown(&browser, 1));
}

#[test]
fn responsiveness_is_followed_only_while_visible_and_a_mount_is_busy_or_stalled() {
    let source = Rc::new(Folders::default());
    let share = source.add("/net/share", network(1), Some(Watching::Live));
    let (browser, clock) = browser_on(&source);
    browser.navigate(share.clone());
    let following = || browser.freshness.following.get();
    let idle = following();
    source.hold_checks.set(true);
    clock.tick_at(15);
    let checking = following();
    source.answer_held_checks();
    let answered = following();
    source.responsiveness.set(Responsiveness::Stalled);
    browser.follow_responsiveness();
    let stalled = following();
    let input = browser.freshness_input();
    browser.set_freshness(FreshnessInput {
        presence: Presence::Hidden,
        ..input
    });
    let hidden = following();
    browser.set_freshness(input);

    assert_eq!(
        (idle, checking, answered, stalled, hidden),
        (false, true, false, true, false)
    );
    assert!(following());
}

#[test]
fn a_folder_found_inaccessible_shows_the_failure_and_is_no_longer_checked() {
    let source = Rc::new(Folders::default());
    let home = source.add("/home/me", local(1), Some(Watching::Live));
    let private = source.add("/home/me/nas", network(2), Some(Watching::Live));
    let (browser, clock) = browser_on(&source);
    browser.navigate(home.clone());
    browser.show_child(0, private.clone());
    let events = recorded(&browser);
    source.answer(&private, StampAnswer::Inaccessible);

    clock.tick_at(15);
    let armed_after_the_failure = clock.armed();
    clock.tick_at(30);

    assert_eq!(
        count(&events, |event| matches!(
            event,
            BrowserEvent::LoadFailed { depth: 1, .. }
        )),
        1
    );
    assert_eq!(armed_after_the_failure, None);
    assert_eq!(source.checks_of(&private), 1);
}

#[test]
fn a_check_finding_the_folder_gone_runs_the_departure_check_under_a_watching_parent() {
    let mut failures = Vec::new();
    for (parent, found_by_a_relist) in [
        (Some(Watching::Live), false),
        (None, false),
        (Some(Watching::Live), true),
        (None, true),
    ] {
        let source = Rc::new(Folders::default());
        let share = source.add("/net/share", network(1), parent);
        let sub = source.add("/net/share/sub", network(2), Some(Watching::Live));
        let (browser, clock) = browser_on(&source);
        browser.navigate(share.clone());
        browser.show_child(0, sub.clone());
        source.hold_checks.set(true);

        clock.tick_at(15);
        let moved = match source.answer_of(&sub) {
            StampAnswer::Stamp(mut stamp) => {
                stamp.modified.0 += 1;
                StampAnswer::Stamp(stamp)
            }
            other => other,
        };
        source.remove(&sub);
        match found_by_a_relist {
            true => source.answer_held_checks_for(&sub, &moved),
            false => source.answer_held_checks(),
        }

        let got = (browser.location_at(0), browser.location_at(1));
        if got != (Some(share.clone()), None) {
            failures.push(format!(
                "parent monitor {parent:?}, found by a re-list {found_by_a_relist}: {got:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn a_folder_removed_with_its_open_subfolder_leaves_for_the_folder_above_both() {
    let source = Rc::new(Folders::default());
    let share = source.add("/net/share", network(1), Some(Watching::Live));
    let outer = source.add("/net/share/outer", network(2), Some(Watching::Live));
    let inner = source.add("/net/share/outer/inner", network(3), Some(Watching::Live));
    let (browser, clock) = browser_on(&source);
    browser.navigate(share.clone());
    browser.show_child(0, outer.clone());
    browser.show_child(1, inner.clone());
    source.remove(&outer);
    source.remove(&inner);
    source.hold_validations.set(true);

    clock.tick_at(15);
    source.hold_validations.set(false);
    source.answer_held_validations();

    assert_eq!(
        (browser.location_at(0), browser.location_at(1)),
        (Some(share), None)
    );
}

#[test]
fn a_failed_relist_leaves_a_departure_check_in_flight_alone() {
    let source = Rc::new(Folders::default());
    let share = source.add("/net/share", network(1), Some(Watching::Live));
    let sub = source.add(
        "/net/share/sub",
        stamp(NFS + 1, 2, true),
        Some(Watching::Live),
    );
    let (browser, clock) = browser_on(&source);
    browser.navigate(share.clone());
    browser.show_child(0, sub.clone());
    source.change(&share, &["listed", "added elsewhere"]);
    source.hold_checks.set(true);
    source.hold_validations.set(true);
    source.fail_loads.set(true);

    clock.tick_at(15);
    source.remove(&sub);
    source.answer_held_check(&sub);
    source.answer_held_check(&share);
    source.hold_validations.set(false);
    source.answer_held_validations();

    assert_eq!(
        (browser.location_at(0), browser.location_at(1)),
        (Some(share), None)
    );
}

#[test]
fn a_check_finding_the_folder_replaced_reloads_it_in_place() {
    let source = Rc::new(Folders::default());
    let share = source.add("/net/share", network(1), Some(Watching::Live));
    let (browser, clock) = browser_on(&source);
    browser.navigate(share.clone());
    source.replace(&share, 9, &["made anew"]);

    clock.tick_at(15);

    assert_eq!(browser.location_at(0), Some(share.clone()));
    assert_eq!(names_at(&browser, 0), ["made anew"]);
    assert_eq!(
        (source.loads_of(&share), source.monitors_requested(&share)),
        (2, 2)
    );
}

#[test]
fn an_unreachable_folder_keeps_its_listing_and_is_retried_later() {
    let source = Rc::new(Folders::default());
    let share = source.add("/net/share", network(1), Some(Watching::Live));
    let (browser, clock) = browser_on(&source);
    browser.navigate(share.clone());
    source.hold_checks.set(true);

    clock.tick_at(15);
    source.answer_held_checks_for(&share, &StampAnswer::Unreachable);
    clock.tick_at(30);
    let checked_at_the_usual_interval = source.checks_of(&share);
    clock.tick_at(45);

    assert_eq!(checked_at_the_usual_interval, 1);
    assert_eq!(source.checks_of(&share), 2);
    assert_eq!(browser.location_at(0), Some(share));
    assert_eq!(names_at(&browser, 0), ["listed"]);
    assert_eq!(
        browser.state.borrow().columns[0].load_state,
        LoadState::Ready
    );
}

fn coarse(
    modified: (i64, u32),
    read_at_millis: u64,
    coarse_window: Option<u64>,
    daemon_cache: Option<u64>,
) -> DirectoryStamp {
    DirectoryStamp {
        modified,
        changed: Some(modified),
        coarse_window: coarse_window.map(Duration::from_secs),
        daemon_cache: daemon_cache.map(Duration::from_secs),
        read_at: UNIX_EPOCH + Duration::from_millis(read_at_millis),
        ..network(1)
    }
}

#[test]
fn a_coarse_baseline_is_confirmed_once_after_its_window() {
    let mut failures = Vec::new();
    for (row, stamp, fails_once, confirmed_at) in [
        (
            "whole seconds",
            coarse((1_000, 0), 1_000_500, Some(2), None),
            false,
            Some(2),
        ),
        (
            "whole seconds whose confirmation fails once",
            coarse((1_000, 0), 1_000_500, Some(2), None),
            true,
            Some(2),
        ),
        (
            "whole seconds behind a daemon's cache",
            coarse((1_000, 0), 1_000_500, Some(3), Some(21)),
            false,
            Some(21),
        ),
        (
            "whole seconds read after their window",
            coarse((1_000, 0), 1_005_000, Some(2), None),
            false,
            None,
        ),
        (
            "coarse ticks with fractions",
            coarse((1_000, 10_000_000), 1_000_500, Some(2), None),
            false,
            None,
        ),
        (
            "fine-grained",
            coarse((1_000, 5), 1_000_500, None, None),
            false,
            None,
        ),
    ] {
        let source = Rc::new(Folders::default());
        let share = source.add("/net/share", stamp, Some(Watching::Live));
        let (browser, clock) = browser_on(&source);
        browser.navigate(share.clone());
        source.fail_loads.set(fails_once);

        let loads: Vec<usize> = (1..=40)
            .map(|second| {
                clock.tick_at(second);
                if source.loads_of(&share) > 1 {
                    source.fail_loads.set(false);
                }
                source.loads_of(&share)
            })
            .collect();

        let first_relist = loads
            .iter()
            .position(|loads| *loads > 1)
            .map(|index| index as u64 + 1);
        let got = (first_relist, loads.last().copied());
        let expected = (
            confirmed_at,
            Some(1 + usize::from(confirmed_at.is_some()) + usize::from(fails_once)),
        );
        if got != expected {
            failures.push(format!("{row}: got {got:?}, expected {expected:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn a_confirmation_and_a_check_due_together_on_one_mount_both_go_through() {
    let source = Rc::new(Folders::default());
    let share = source.add(
        "/net/share",
        coarse((1_000, 0), 1_000_500, Some(3), Some(15)),
        Some(Watching::Live),
    );
    let sub = source.add("/net/share/sub", network(2), Some(Watching::Live));
    let (browser, clock) = browser_on(&source);
    browser.navigate(share.clone());
    browser.show_child(0, sub.clone());
    source.change(&sub, &["listed", "added elsewhere"]);
    source.hold_checks.set(true);
    source.hold_loads.set(true);

    clock.tick_at(15);
    source.answer_held_checks();
    source.finish_held_loads();
    source.finish_held_loads();

    assert_eq!(source.loads_of(&share), 2);
    assert_eq!(names_at(&browser, 1), ["added elsewhere", "listed"]);
}
