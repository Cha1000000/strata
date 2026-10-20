// SPDX-License-Identifier: MIT

use super::*;
use crate::{test_support::gtk_test, ui::preferences::PreferenceManager};

fn texture() -> gdk::Texture {
    gdk::MemoryTexture::new(
        1,
        1,
        gdk::MemoryFormat::R8g8b8a8,
        &glib::Bytes::from_owned(vec![120, 80, 40, 255]),
        4,
    )
    .upcast()
}

#[test]
fn show_thumbnails_saved_setting_bypasses_cached_and_uncached_media() {
    gtk_test(
        "ui::thumbnail::tests::show_thumbnails_saved_setting_bypasses_cached_and_uncached_media",
        || {
            let config = crate::ui::preferences::config_directory();
            std::fs::create_dir_all(&config).expect("config directory");
            std::fs::write(config.join("settings.toml"), "show_thumbnails = false\n")
                .expect("saved thumbnail setting");
            let manager = PreferenceManager::shared();
            let directory = tempfile::tempdir().expect("media folder");
            let cached = directory.path().join("cached.png");
            let pending = directory.path().join("uncached.pdf");
            let preview = texture();
            THUMBNAIL_CACHE.with_borrow_mut(|cache| {
                cache.insert(
                    ThumbnailKey {
                        path: cached.clone(),
                        modified: None,
                        file_size: None,
                        thumbnail_size: super::super::thumbnail_cache::CANONICAL_MAX_EDGE,
                    },
                    preview,
                );
            });
            for (path, icon) in [
                (&cached, crate::assets::icons::PICTURES),
                (&pending, crate::assets::icons::DOCUMENTS),
            ] {
                let slot = ThumbnailSlot::new(18);
                set_thumbnail_or_icon_for_path(&slot, path, icon, 18, 32);
                assert!(slot.texture().is_none());
                assert_eq!(slot.fallback_icon().as_deref(), Some(icon));
                ACTIVE_REQUESTS.with_borrow(|requests| {
                    assert!(!requests.contains_key(&(slot.as_ptr() as usize)));
                });
            }
            assert!(!manager.show_thumbnails());
        },
    );
}

#[test]
fn show_thumbnails_cancels_late_results_and_forgets_recycled_targets() {
    gtk_test(
        "ui::thumbnail::tests::show_thumbnails_cancels_late_results_and_forgets_recycled_targets",
        || {
            let manager = PreferenceManager::shared();
            let directory = tempfile::tempdir().expect("media folder");
            let old_path = directory.path().join("old.png");
            let slot = ThumbnailSlot::new(18);
            set_thumbnail_or_icon_for_path(
                &slot,
                &old_path,
                crate::assets::icons::PICTURES,
                18,
                32,
            );
            let target = ACTIVE_REQUESTS.with_borrow(|requests| {
                let image_id = slot.as_ptr() as usize;
                let active = requests.get(&image_id).expect("pending thumbnail");
                PendingTarget {
                    image_id,
                    request: active.id,
                    image: active.image.clone(),
                }
            });
            manager.set_show_thumbnails(false);
            assert!(!request_is_live(&target));
            finish_thumbnail_targets(vec![target.clone()], Some(&texture()), &old_path);
            assert!(slot.texture().is_none());

            let new_path = directory.path().join("new.png");
            let new_texture = texture();
            THUMBNAIL_CACHE.with_borrow_mut(|cache| {
                cache.insert(
                    ThumbnailKey {
                        path: new_path.clone(),
                        modified: None,
                        file_size: None,
                        thumbnail_size: super::super::thumbnail_cache::CANONICAL_MAX_EDGE,
                    },
                    new_texture.clone(),
                );
            });
            set_thumbnail_or_icon_for_path(
                &slot,
                &new_path,
                crate::assets::icons::PICTURES,
                18,
                32,
            );
            manager.set_show_thumbnails(true);
            assert_eq!(slot.texture(), Some(new_texture.clone()));
            finish_thumbnail_targets(vec![target], Some(&texture()), &old_path);
            assert_eq!(slot.texture(), Some(new_texture));

            manager.set_show_thumbnails(false);
            cancel_thumbnails_in(slot.upcast_ref());
            manager.set_show_thumbnails(true);
            assert!(slot.texture().is_none());
            ACTIVE_REQUESTS.with_borrow(|requests| assert!(requests.is_empty()));
        },
    );
}

#[test]
fn a_thumbnail_released_by_an_operation_is_not_remembered_as_failed() {
    gtk_test(
        "ui::thumbnail::tests::a_thumbnail_released_by_an_operation_is_not_remembered_as_failed",
        || {
            let directory = tempfile::tempdir().expect("media folder");
            let links = tempfile::tempdir().expect("link folder");
            let link = links.path().join("media");
            std::os::unix::fs::symlink(directory.path(), &link).expect("linked folder");
            for (row, shown_in, name, refused_after_end) in [
                (
                    "refused while it runs",
                    directory.path(),
                    "running.png",
                    false,
                ),
                (
                    "refused through a linked folder while it runs",
                    link.as_path(),
                    "linked.png",
                    false,
                ),
                (
                    "refused after it ended",
                    directory.path(),
                    "ended.png",
                    true,
                ),
            ] {
                let key = ThumbnailKey {
                    path: shown_in.join(name),
                    modified: None,
                    file_size: None,
                    thumbnail_size: super::super::thumbnail_cache::CANONICAL_MAX_EDGE,
                };
                let slot = ThumbnailSlot::new(18);
                let request = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
                let target =
                    register_active_request(&slot, slot.as_ptr() as usize, request, key.clone());
                let job = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
                PENDING_THUMBNAILS.with_borrow_mut(|pending| {
                    pending.insert(
                        key.clone(),
                        PendingThumbnail {
                            id: job,
                            kind: ThumbnailKind::Image,
                            cancellation: Cancellation::default(),
                            targets: vec![target.clone()],
                        },
                    );
                });
                let operation = crate::adapters::ActiveOperation::begin([]);
                glib::MainContext::default().block_on(
                    operation.release(vec![directory.path().join(name)], &gio::Cancellable::new()),
                );
                let operation = if refused_after_end {
                    drop(operation);
                    None
                } else {
                    Some(operation)
                };

                glib::MainContext::default().block_on(finish_thumbnail_job(
                    ThumbnailJob {
                        id: job,
                        key: key.clone(),
                        resolved: key.clone(),
                        kind: ThumbnailKind::Image,
                        cancellation: Cancellation::default(),
                    },
                    Err(crate::services::file_activity::HELD_FOR_OPERATION.to_owned()),
                ));

                assert!(
                    THUMBNAIL_CACHE
                        .with_borrow_mut(|cache| cache.get(&key))
                        .is_none(),
                    "{row}: an operation's refusal is no failure to remember"
                );
                assert!(request_is_live(&target), "{row}");
                assert_eq!(
                    PENDING_THUMBNAILS.with_borrow(|pending| pending.contains_key(&key)),
                    refused_after_end,
                    "{row}: asked for again at once only when no operation will end later"
                );
                drop(operation);
                assert!(
                    PENDING_THUMBNAILS.with_borrow(|pending| pending.contains_key(&key)),
                    "{row}: the thumbnail is asked for again once the operation ends"
                );
            }
        },
    );
}
