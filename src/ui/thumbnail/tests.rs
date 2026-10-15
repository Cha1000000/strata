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
