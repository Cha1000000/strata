// SPDX-License-Identifier: MIT

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
};

use gtk::{gdk, gio, glib, prelude::*};

use super::{page_content, scrollable_page, search, settings_group};
use crate::{
    assets::{self, icons},
    services::{
        integration_catalog::{self, Category, Entry, Installation, RequirementResult, Status},
        package_manager::PackageManager,
    },
    ui::{accessibility, controls::copyable_command},
};

#[cfg(test)]
mod tests;

const TITLE: &str = "Integrations";

type Checker = Arc<dyn Fn(&Entry) -> Vec<RequirementResult> + Send + Sync>;

/// Where the page reads the catalog and the registry from; tests substitute both.
pub(super) struct Source {
    pub catalog: Box<dyn Fn() -> Vec<Entry>>,
    pub root: Option<PathBuf>,
    pub check: Checker,
    /// Shows the registration directory in the browser, when a window hosts the page.
    pub open_folder: Option<super::OpenFolder>,
    pub reload: Rc<dyn Fn()>,
}

impl Default for Source {
    fn default() -> Self {
        Self {
            catalog: Box::new(integration_catalog::catalog),
            root: crate::services::file_providers::config_root(),
            check: Arc::new(integration_catalog::check_requirements),
            open_folder: None,
            reload: Rc::new(crate::ui::file_providers::reload),
        }
    }
}

pub(super) fn integrations_page(open_folder: Option<super::OpenFolder>) -> gtk::Widget {
    integrations_page_with(Source {
        open_folder,
        ..Source::default()
    })
}

pub(super) fn integrations_page_with(source: Source) -> gtk::Widget {
    let content = page_content();
    content.add_css_class("settings-integrations-page");
    let row = gtk::Box::new(gtk::Orientation::Vertical, 2);
    row.add_css_class("settings-option");
    search::tag(&row, TITLE);
    // One child keeps the copy stacked when the responsive layout widens rows.
    let copy = gtk::Box::new(gtk::Orientation::Vertical, 2);
    copy.set_hexpand(true);
    let title = gtk::Label::new(Some(&crate::i18n::tr(TITLE)));
    title.set_xalign(0.0);
    title.add_css_class("settings-option-title");
    let description = gtk::Label::new(Some(&crate::i18n::tr(
        "Optional integrations bundled with Strata connect the context menu to other apps and services, such as sending files to a phone. They run with your permissions, like any program you install.",
    )));
    description.set_xalign(0.0);
    description.set_wrap(true);
    description.add_css_class("settings-option-description");
    copy.append(&title);
    copy.append(&description);
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.add_css_class("settings-integration-actions");
    actions.set_halign(gtk::Align::End);
    actions.set_margin_top(6);
    copy.append(&actions);
    row.append(&copy);
    settings_group(&content, "").append(&row);

    // A direct child of the page so its group headings get the page spacing.
    let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
    search::tag(&list, TITLE);
    content.append(&list);

    let page = Rc::new(Page {
        source,
        list,
        requirements: RefCell::default(),
        manager: PackageManager::detect(),
        generation: Cell::new(0),
    });
    let refresh = icon_label_button(icons::REFRESH, "Refresh");
    refresh.set_widget_name("integrations-refresh");
    let refreshing = Rc::downgrade(&page);
    refresh.connect_clicked(move |_| {
        if let Some(page) = refreshing.upgrade() {
            (page.source.reload)();
            page.requirements.borrow_mut().clear();
            page.rebuild();
        }
    });
    actions.append(&refresh);
    if let (Some(root), Some(open_folder)) = (&page.source.root, page.source.open_folder.clone()) {
        let open = icon_label_button(icons::FOLDER_OPEN, "Open folder");
        open.set_widget_name("integrations-open-folder");
        let root = root.clone();
        open.connect_clicked(move |_| open_folder(root.clone()));
        actions.append(&open);
    }
    // The registry can change outside this page, so every visit rescans it.
    content.connect_map(move |_| page.rebuild());
    scrollable_page(&content, None)
}

struct Page {
    source: Source,
    list: gtk::Box,
    /// Requirement results by entry id, filled by a worker after the first build.
    requirements: RefCell<HashMap<String, Vec<RequirementResult>>>,
    manager: Option<PackageManager>,
    generation: Cell<u64>,
}

struct Row {
    entry: Entry,
    status: Status,
    enabled: bool,
}

impl Page {
    fn rebuild(self: &Rc<Self>) {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        let catalog = (self.source.catalog)();
        let root = self.source.root.clone();
        let page = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let scanned = gio::spawn_blocking(move || {
                let rows: Vec<Row> = catalog
                    .into_iter()
                    .map(|entry| Row {
                        status: root.as_deref().map_or(Status::NotInstalled, |root| {
                            integration_catalog::status(&entry, root)
                        }),
                        enabled: root
                            .as_deref()
                            .is_none_or(|root| integration_catalog::enabled(root, &entry.id)),
                        entry,
                    })
                    .collect();
                let manual = root.as_deref().map_or_else(Vec::new, |root| {
                    let catalog: Vec<Entry> = rows.iter().map(|row| row.entry.clone()).collect();
                    integration_catalog::manual_installations(root, &catalog)
                });
                (rows, manual)
            })
            .await;
            let Some(page) = page.upgrade() else {
                return;
            };
            if page.generation.get() != generation {
                return;
            }
            if let Ok((rows, manual)) = scanned {
                page.show(rows, manual);
            }
        });
    }

    fn show(self: &Rc<Self>, rows: Vec<Row>, manual: Vec<Installation>) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        if rows.is_empty() && manual.is_empty() {
            let empty = gtk::Label::new(Some(&crate::i18n::tr(
                "No integrations are bundled with this build.",
            )));
            empty.set_xalign(0.0);
            empty.set_wrap(true);
            empty.add_css_class("settings-section-description");
            self.list.append(&empty);
            return;
        }
        let mut unchecked = Vec::new();
        for category in Category::ALL {
            let entries: Vec<&Row> = rows
                .iter()
                .filter(|row| row.entry.category == category)
                .collect();
            if entries.is_empty() {
                continue;
            }
            let group = settings_group(&self.list, heading(category));
            for row in entries {
                if !self.requirements.borrow().contains_key(&row.entry.id) {
                    unchecked.push(row.entry.clone());
                }
                group.append(&self.catalog_row(row));
            }
        }
        if !manual.is_empty() {
            let group = settings_group(&self.list, "INSTALLED MANUALLY");
            for installation in manual {
                group.append(&self.manual_row(installation));
            }
        }
        if !unchecked.is_empty() {
            self.check(unchecked);
        }
    }

    fn check(self: &Rc<Self>, entries: Vec<Entry>) {
        let page = Rc::downgrade(self);
        let generation = self.generation.get();
        let check = self.source.check.clone();
        glib::spawn_future_local(async move {
            let results = gio::spawn_blocking(move || {
                entries
                    .into_iter()
                    .map(|entry| {
                        let results = check(&entry);
                        (entry.id, results)
                    })
                    .collect::<Vec<_>>()
            })
            .await;
            let Some(page) = page.upgrade() else {
                return;
            };
            if let Ok(results) = results {
                page.requirements.borrow_mut().extend(results);
                if page.generation.get() == generation {
                    page.rebuild();
                }
            }
        });
    }

    fn catalog_row(self: &Rc<Self>, row: &Row) -> gtk::Box {
        let entry = &row.entry;
        let (widget, copy) = row_frame(&entry.id);
        let heading = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        heading.append(&logo(entry.icon_bytes(), entry.category));
        let name = crate::i18n::tr(&entry.name);
        let title = gtk::Label::new(Some(&name));
        title.set_xalign(0.0);
        title.set_wrap(true);
        title.set_hexpand(true);
        title.add_css_class("settings-option-title");
        heading.append(&title);
        let status = gtk::Label::new(Some(&status_label(&row.status, row.enabled)));
        status.add_css_class("integration-status");
        status.add_css_class(status_class(&row.status, row.enabled));
        status.set_valign(gtk::Align::Center);
        heading.append(&status);
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        actions.add_css_class("integration-actions");
        actions.set_valign(gtk::Align::Center);
        heading.append(&actions);
        copy.append(&heading);
        copy.append(&note(&crate::i18n::tr(&entry.description)));
        copy.append(&note(&rust_i18n::t!(
            "Maintained by %{maintainer}",
            maintainer = format!("@{}", entry.maintainer)
        )));
        copy.append(&self.requirements_row(entry));
        let message = note("");
        message.add_css_class("integration-message");
        message.set_visible(false);
        copy.append(&message);
        let confirmation = gtk::Box::new(gtk::Orientation::Vertical, 8);
        confirmation.add_css_class("integration-confirmation");
        confirmation.set_visible(false);
        copy.append(&confirmation);
        let context = Rc::new(RowContext {
            page: Rc::downgrade(self),
            id: entry.id.clone(),
            name: name.clone(),
            message,
            confirmation,
            actions,
        });
        if row.status != Status::NotInstalled {
            context.toggle(row.enabled);
        }
        match &row.status {
            Status::NotInstalled => {
                let install = context.button(
                    "Install",
                    icons::DOWNLOADS,
                    &rust_i18n::t!("Install %{name}", name = name),
                    false,
                );
                let entry = entry.clone();
                let context = context.clone();
                install.connect_clicked(move |_| context.install(entry.clone()));
            }
            Status::Installed { update, modified } => {
                let remove = context.button(
                    "Remove",
                    icons::TRASH,
                    &rust_i18n::t!("Remove %{name}", name = name),
                    true,
                );
                let modified = *modified;
                let removing = context.clone();
                remove.connect_clicked(move |_| {
                    if modified {
                        removing.confirm(
                            &crate::i18n::tr(
                                "The installed copy has local changes that are deleted with it.",
                            ),
                            "Remove",
                            rust_i18n::t!("Confirm removing %{name}", name = removing.name)
                                .into_owned(),
                            Box::new({
                                let context = removing.clone();
                                move || context.remove()
                            }),
                        );
                    } else {
                        removing.remove();
                    }
                });
                if *update {
                    let update = context.button(
                        "Update",
                        icons::REFRESH,
                        &rust_i18n::t!("Update %{name}", name = name),
                        false,
                    );
                    let entry = entry.clone();
                    let updating = context.clone();
                    update.connect_clicked(move |_| {
                        if modified {
                            let entry = entry.clone();
                            let context = updating.clone();
                            updating.confirm(
                                &crate::i18n::tr("The installed copy has local changes. Updating replaces them with the bundled files."),
                                "Replace",
                                rust_i18n::t!("Confirm replacing %{name}", name = updating.name).into_owned(),
                                Box::new(move || context.install(entry.clone())),
                            );
                        } else {
                            updating.install(entry.clone());
                        }
                    });
                }
            }
            Status::Manual => {
                let remove = context.button(
                    "Remove",
                    icons::TRASH,
                    &rust_i18n::t!("Remove %{name}", name = name),
                    true,
                );
                let removing = context.clone();
                remove.connect_clicked(move |_| {
                    removing.confirm(
                        &crate::i18n::tr(
                            "This copy was not installed from the catalog. Its folder is deleted.",
                        ),
                        "Remove",
                        rust_i18n::t!("Confirm removing %{name}", name = removing.name)
                            .into_owned(),
                        Box::new({
                            let context = removing.clone();
                            move || context.remove()
                        }),
                    );
                });
            }
        }
        widget
    }

    fn manual_row(self: &Rc<Self>, installation: Installation) -> gtk::Box {
        let (widget, copy) = row_frame(&installation.id);
        let heading = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        heading.append(&logo(installation.icon.as_deref(), Category::Other));
        let title = gtk::Label::new(Some(&installation.name));
        title.set_xalign(0.0);
        title.set_wrap(true);
        title.set_hexpand(true);
        title.add_css_class("settings-option-title");
        heading.append(&title);
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        actions.add_css_class("integration-actions");
        actions.set_valign(gtk::Align::Center);
        heading.append(&actions);
        copy.append(&heading);
        let message = note("");
        message.add_css_class("integration-message");
        message.set_visible(false);
        copy.append(&message);
        let confirmation = gtk::Box::new(gtk::Orientation::Vertical, 8);
        confirmation.add_css_class("integration-confirmation");
        confirmation.set_visible(false);
        copy.append(&confirmation);
        let context = Rc::new(RowContext {
            page: Rc::downgrade(self),
            id: installation.id.clone(),
            name: installation.name.clone(),
            message,
            confirmation,
            actions,
        });
        context.toggle(installation.enabled);
        let remove = context.button(
            "Remove",
            icons::TRASH,
            &rust_i18n::t!("Remove %{name}", name = installation.name),
            true,
        );
        let removing = context.clone();
        remove.connect_clicked(move |_| {
            removing.confirm(
                &crate::i18n::tr(
                    "This copy was not installed from the catalog. Its folder is deleted.",
                ),
                "Remove",
                rust_i18n::t!("Confirm removing %{name}", name = removing.name).into_owned(),
                Box::new({
                    let context = removing.clone();
                    move || context.remove()
                }),
            );
        });
        widget
    }

    fn requirements_row(&self, entry: &Entry) -> gtk::Box {
        let container = gtk::Box::new(gtk::Orientation::Vertical, 6);
        container.add_css_class("integration-requirements");
        let chips = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        chips.add_css_class("integration-requirement-list");
        let label = gtk::Label::new(Some(&crate::i18n::tr("Requires:")));
        label.add_css_class("settings-option-description");
        chips.append(&label);
        container.append(&chips);
        let results = self.requirements.borrow();
        let Some(results) = results.get(&entry.id) else {
            let checking = gtk::Label::new(Some(&crate::i18n::tr("Checking…")));
            checking.add_css_class("settings-option-description");
            chips.append(&checking);
            return container;
        };
        let mut hints = Vec::new();
        for result in results {
            let chip = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            chip.add_css_class("integration-requirement");
            let icon = if result.satisfied {
                assets::primary_icon(icons::CIRCLE_CHECK, 14)
            } else {
                assets::danger_icon(icons::CIRCLE_X, 14)
            };
            chip.append(&icon);
            let name = result.requirement.label();
            let text = gtk::Label::new(Some(name));
            text.add_css_class("settings-option-description");
            chip.append(&text);
            chip.add_css_class(if result.satisfied {
                "available"
            } else {
                "missing"
            });
            accessibility::set_label(
                &text,
                &rust_i18n::t!(
                    "%{label}: %{value}",
                    label = name,
                    value = crate::i18n::tr(if result.satisfied {
                        "Available"
                    } else {
                        "Missing"
                    })
                ),
            );
            chips.append(&chip);
            if !result.satisfied
                && let Some(command) = result.requirement.install_command(self.manager)
                && !hints.contains(&command)
            {
                hints.push(command);
            }
        }
        for command in hints {
            let hint = note(&crate::i18n::tr("Install with:"));
            container.append(&hint);
            let field = copyable_command(&command);
            field.add_css_class("diagnostics-command");
            container.append(&field);
        }
        container
    }
}

/// A row and the copy box its content goes in.
fn row_frame(id: &str) -> (gtk::Box, gtk::Box) {
    // Vertical by design: the responsive layout stacks horizontal rows,
    // which would push the logo above the copy.
    let widget = gtk::Box::new(gtk::Orientation::Vertical, 3);
    widget.add_css_class("settings-option");
    widget.add_css_class("integration-row");
    // The copy expands to center within the shared row height; stop that
    // expansion from reaching the page.
    widget.set_vexpand(false);
    widget.set_widget_name(&format!("integration-{id}"));
    let copy = gtk::Box::new(gtk::Orientation::Vertical, 3);
    copy.set_hexpand(true);
    // Rows share a minimum height; keep short copy centered within it
    // whichever orientation the responsive layout gives the row.
    copy.set_vexpand(true);
    copy.set_valign(gtk::Align::Center);
    widget.append(&copy);
    (widget, copy)
}

struct RowContext {
    page: std::rc::Weak<Page>,
    id: String,
    name: String,
    message: gtk::Label,
    confirmation: gtk::Box,
    actions: gtk::Box,
}

impl RowContext {
    /// An icon button in the row heading, styled like the custom action rows.
    fn button(&self, label: &str, icon: &str, accessible: &str, danger: bool) -> gtk::Button {
        let image = if danger {
            assets::danger_icon(icon, assets::CHROME_ICON_PX)
        } else {
            assets::text_icon(icon, assets::CHROME_ICON_PX)
        };
        image.set_halign(gtk::Align::Center);
        image.set_valign(gtk::Align::Center);
        let button = gtk::Button::builder()
            .child(&image)
            .tooltip_text(crate::i18n::tr(label))
            .build();
        accessibility::set_label(&button, accessible);
        button.add_css_class("settings-action-button");
        button.add_css_class("settings-action-icon-button");
        if danger {
            button.add_css_class("danger");
            button.add_css_class("borderless");
        }
        button.set_widget_name(&format!("integration-{}-{}", self.id, label.to_lowercase()));
        self.actions.append(&button);
        button
    }

    /// A labelled switch that keeps the registration installed but stops it
    /// loading, separated from the row's buttons.
    fn toggle(self: &Rc<Self>, enabled: bool) {
        let group = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        group.add_css_class("integration-toggle");
        group.set_valign(gtk::Align::Center);
        let state = gtk::Label::new(Some(&toggle_label(enabled)));
        state.add_css_class("settings-option-description");
        group.append(&state);
        let switch = gtk::Switch::builder()
            .active(enabled)
            .valign(gtk::Align::Center)
            .build();
        accessibility::set_label(&switch, &rust_i18n::t!("Enable %{name}", name = self.name));
        switch.set_widget_name(&format!("integration-{}-enabled", self.id));
        let context = self.clone();
        switch.connect_state_set(move |_, enabled| {
            state.set_text(&toggle_label(enabled));
            context.set_enabled(enabled);
            glib::Propagation::Proceed
        });
        group.append(&switch);
        self.actions.append(&group);
        let separator = gtk::Separator::new(gtk::Orientation::Vertical);
        separator.add_css_class("integration-separator");
        self.actions.append(&separator);
    }

    fn busy(&self, busy: bool) {
        let mut child = self.actions.first_child();
        while let Some(button) = child {
            child = button.next_sibling();
            button.set_sensitive(!busy);
        }
    }

    fn confirm(
        self: &Rc<Self>,
        question: &str,
        accept: &str,
        accessible: String,
        run: Box<dyn Fn()>,
    ) {
        while let Some(child) = self.confirmation.first_child() {
            self.confirmation.remove(&child);
        }
        let text = note(question);
        self.confirmation.append(&text);
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.add_css_class("settings-integration-actions");
        buttons.set_halign(gtk::Align::End);
        let cancel = gtk::Button::with_label(&crate::i18n::tr("Cancel"));
        cancel.add_css_class("action-dialog-cancel");
        let proceed = named_button(accept, &accessible);
        proceed.add_css_class("action-dialog-confirm");
        proceed.set_widget_name(&format!("integration-{}-confirm", self.id));
        buttons.append(&cancel);
        buttons.append(&proceed);
        self.confirmation.append(&buttons);
        self.confirmation.set_visible(true);
        self.busy(true);
        let context = self.clone();
        cancel.connect_clicked(move |_| {
            context.confirmation.set_visible(false);
            context.busy(false);
        });
        let context = self.clone();
        proceed.connect_clicked(move |_| {
            context.confirmation.set_visible(false);
            run();
        });
    }

    fn install(self: &Rc<Self>, entry: Entry) {
        let Some(page) = self.page.upgrade() else {
            return;
        };
        let Some(root) = page.source.root.clone() else {
            self.fail(&crate::i18n::tr("No configuration folder is available."));
            return;
        };
        self.run(move || integration_catalog::install(&entry, &root).map(|_| ()));
    }

    fn set_enabled(self: &Rc<Self>, enabled: bool) {
        let Some(page) = self.page.upgrade() else {
            return;
        };
        let Some(root) = page.source.root.clone() else {
            return;
        };
        let id = self.id.clone();
        self.run(move || integration_catalog::set_enabled(&root, &id, enabled));
    }

    fn remove(self: &Rc<Self>) {
        let Some(page) = self.page.upgrade() else {
            return;
        };
        let Some(root) = page.source.root.clone() else {
            return;
        };
        let id = self.id.clone();
        self.run(move || integration_catalog::remove(&root, &id));
    }

    fn run(self: &Rc<Self>, work: impl FnOnce() -> Result<(), String> + Send + 'static) {
        self.busy(true);
        self.message.set_visible(false);
        let context = self.clone();
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(work).await;
            let Some(page) = context.page.upgrade() else {
                return;
            };
            match result {
                Ok(Ok(())) => {
                    (page.source.reload)();
                    page.rebuild();
                }
                Ok(Err(error)) => {
                    context.fail(&rust_i18n::t!("The change failed: %{error}", error = error))
                }
                Err(_) => context.fail(&crate::i18n::tr("The change failed.")),
            }
        });
    }

    fn fail(&self, text: &str) {
        self.message.set_text(text);
        self.message.add_css_class("error");
        self.message.set_visible(true);
        self.busy(false);
    }
}

/// A text button whose accessible name says which entry it acts on. A label
/// child would otherwise name the button after its text alone.
fn named_button(label: &str, accessible: &str) -> gtk::Button {
    let button = gtk::Button::new();
    button.set_child(Some(&gtk::Label::new(Some(&crate::i18n::tr(label)))));
    button.add_css_class("text-button");
    accessibility::set_label(&button, accessible);
    button
}

fn toggle_label(enabled: bool) -> String {
    crate::i18n::tr(if enabled { "Enabled" } else { "Disabled" })
}

/// A text button with a leading icon; the accessible name is the label.
fn icon_label_button(icon: &str, label: &str) -> gtk::Button {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    content.set_halign(gtk::Align::Center);
    let image = assets::text_icon(icon, assets::CHROME_ICON_PX);
    image.set_valign(gtk::Align::Center);
    content.append(&image);
    let text = crate::i18n::tr(label);
    content.append(&gtk::Label::new(Some(&text)));
    let button = gtk::Button::builder().child(&content).build();
    button.add_css_class("settings-action-button");
    accessibility::set_label(&button, &text);
    button
}

fn logo(icon: Option<&[u8]>, category: Category) -> gtk::Widget {
    let texture = icon.and_then(|bytes| gdk::Texture::from_bytes(&glib::Bytes::from(bytes)).ok());
    let image = match texture {
        Some(texture) => gtk::Image::from_paintable(Some(&texture)),
        None => assets::primary_icon(category_icon(category), 24),
    };
    image.set_pixel_size(20);
    image.set_valign(gtk::Align::Center);
    image.add_css_class("integration-logo");
    image.upcast()
}

fn category_icon(category: Category) -> &'static str {
    match category {
        Category::Cloud => icons::CLOUD,
        Category::Devices => icons::SMARTPHONE,
        Category::VersionControl => icons::GIT_BRANCH,
        Category::Other => icons::BLOCKS,
    }
}

fn heading(category: Category) -> &'static str {
    match category {
        Category::Cloud => "CLOUD",
        Category::Devices => "DEVICES",
        Category::VersionControl => "VERSION CONTROL",
        Category::Other => "OTHER",
    }
}

fn status_label(status: &Status, enabled: bool) -> String {
    let tr = crate::i18n::tr;
    match status {
        Status::NotInstalled => tr("Not installed"),
        Status::Installed { update: true, .. } if !enabled => tr("Update available · Disabled"),
        _ if !enabled => tr("Disabled"),
        Status::Installed {
            update: true,
            modified: true,
        } => tr("Update available · Modified locally"),
        Status::Installed {
            update: true,
            modified: false,
        } => tr("Update available"),
        Status::Installed {
            update: false,
            modified: true,
        } => tr("Installed · Modified locally"),
        Status::Installed { .. } => tr("Installed"),
        Status::Manual => tr("Installed manually"),
    }
}

fn status_class(status: &Status, enabled: bool) -> &'static str {
    match status {
        Status::NotInstalled => "absent",
        Status::Installed { update: false, .. } | Status::Manual if !enabled => "absent",
        Status::Installed { update: true, .. } => "update",
        Status::Installed { .. } | Status::Manual => "installed",
    }
}

fn note(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    crate::ui::controls::keep_words_whole(&label);
    label.add_css_class("settings-option-description");
    label
}
