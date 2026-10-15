//! The settings sheet: a libadwaita preferences dialog opened from the gear
//! in the viewfinder's top bar. On a phone it opens as a bottom sheet.
//!
//! Every change is applied at once through `on_change` (which saves the
//! settings file). Settings that need features not built yet show as
//! disabled rows with the reason.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::settings::{
    default_photo_dir, default_video_dir, Aspect, Grid, Resolution, Settings, Timer,
    VideoResolution,
};

type OnChange = Rc<dyn Fn(&Settings)>;

#[allow(clippy::too_many_arguments)]
fn combo<T: Copy + PartialEq + 'static>(
    title: &str,
    subtitle: Option<&str>,
    all: &'static [T],
    label: fn(T) -> &'static str,
    current: T,
    state: &Rc<RefCell<Settings>>,
    on_change: &OnChange,
    set: fn(&mut Settings, T),
) -> adw::ComboRow {
    let labels: Vec<&str> = all.iter().map(|v| label(*v)).collect();
    let row = adw::ComboRow::builder()
        .title(title)
        .model(&gtk::StringList::new(&labels))
        .selected(all.iter().position(|v| *v == current).unwrap_or(0) as u32)
        .build();
    if let Some(s) = subtitle {
        row.set_subtitle(s);
    }
    let (state, on_change) = (state.clone(), on_change.clone());
    row.connect_selected_notify(move |r| {
        if let Some(v) = all.get(r.selected() as usize) {
            set(&mut state.borrow_mut(), *v);
            on_change(&state.borrow());
        }
    });
    row
}

fn switch(
    title: &str,
    subtitle: &str,
    active: bool,
    state: &Rc<RefCell<Settings>>,
    on_change: &OnChange,
    set: fn(&mut Settings, bool),
) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder()
        .title(title)
        .subtitle(subtitle)
        .active(active)
        .build();
    let (state, on_change) = (state.clone(), on_change.clone());
    row.connect_active_notify(move |r| {
        set(&mut state.borrow_mut(), r.is_active());
        on_change(&state.borrow());
    });
    row
}

/// A row for something not built yet.
fn later(title: &str, reason: &str) -> adw::ActionRow {
    adw::ActionRow::builder()
        .title(title)
        .subtitle(reason)
        .sensitive(false)
        .build()
}

fn info(title: &str, value: &str) -> adw::ActionRow {
    adw::ActionRow::builder()
        .title(title)
        .subtitle(value)
        .subtitle_selectable(true)
        .build()
}

pub fn dialog(
    settings: &Settings,
    on_change: impl Fn(&Settings) + 'static,
) -> adw::PreferencesDialog {
    let state = Rc::new(RefCell::new(settings.clone()));
    let on_change: OnChange = Rc::new(on_change);
    let s = settings;

    // Photo.
    let photo = adw::PreferencesPage::builder()
        .title("Photo")
        .name("photo")
        .icon_name("camera-photo-symbolic")
        .build();
    let shape = adw::PreferencesGroup::builder().title("Picture").build();
    shape.add(&combo(
        "Aspect ratio",
        Some("Cropped from the center of the frame"),
        &Aspect::ALL,
        Aspect::label,
        s.aspect,
        &state,
        &on_change,
        |s, v| s.aspect = v,
    ));
    shape.add(&combo(
        "Resolution",
        Some("Full is each lens's largest mode"),
        &Resolution::ALL,
        Resolution::label,
        s.resolution,
        &state,
        &on_change,
        |s, v| s.resolution = v,
    ));
    let quality = adw::SpinRow::with_range(50.0, 100.0, 1.0);
    quality.set_title("JPEG quality");
    quality.set_subtitle("Higher keeps more detail in bigger files");
    quality.set_value(s.jpeg_quality as f64);
    {
        let (state, on_change) = (state.clone(), on_change.clone());
        quality.connect_value_notify(move |r| {
            state.borrow_mut().jpeg_quality = r.value().round() as u8;
            on_change(&state.borrow());
        });
    }
    shape.add(&quality);
    photo.add(&shape);

    let taking = adw::PreferencesGroup::builder()
        .title("Taking photos")
        .build();
    taking.add(&combo(
        "Timer",
        None,
        &Timer::ALL,
        Timer::label,
        s.timer,
        &state,
        &on_change,
        |s, v| s.timer = v,
    ));
    taking.add(&combo(
        "Grid",
        None,
        &Grid::ALL,
        Grid::label,
        s.grid,
        &state,
        &on_change,
        |s, v| s.grid = v,
    ));
    taking.add(&later("Level", "Coming later"));
    taking.add(&switch(
        "Shutter sound",
        "Play a sound when a photo is taken",
        s.shutter_sound,
        &state,
        &on_change,
        |s, v| s.shutter_sound = v,
    ));
    taking.add(&switch(
        "Save selfies as previewed",
        "Mirror front camera photos, as the preview shows them",
        s.mirror_front,
        &state,
        &on_change,
        |s, v| s.mirror_front = v,
    ));
    photo.add(&taking);

    let saving = adw::PreferencesGroup::builder().title("Saving").build();
    saving.add(&switch(
        "Location tags",
        "Record where photos and videos are taken (GeoClue)",
        s.location,
        &state,
        &on_change,
        |s, v| s.location = v,
    ));
    let dir = adw::EntryRow::builder()
        .title("Save photos in")
        .text(s.photo_dir().display().to_string())
        .show_apply_button(true)
        .build();
    {
        let (state, on_change) = (state.clone(), on_change.clone());
        dir.connect_apply(move |r| {
            let text = r.text().trim().to_string();
            let path = PathBuf::from(&text);
            state.borrow_mut().save_dir =
                (!text.is_empty() && path != default_photo_dir()).then_some(path);
            on_change(&state.borrow());
        });
    }
    saving.add(&dir);
    photo.add(&saving);

    // Video.
    let video = adw::PreferencesPage::builder()
        .title("Video")
        .name("video")
        .icon_name("camera-video-symbolic")
        .build();
    let vgroup = adw::PreferencesGroup::builder()
        .title("Video")
        .description(format!(
            "Videos are saved in {} as MP4: H.264 video, Opus sound.",
            default_video_dir().display()
        ))
        .build();
    vgroup.add(&combo(
        "Resolution",
        Some("Landscape frames, turned upright by the player"),
        &VideoResolution::ALL,
        VideoResolution::label,
        s.video_resolution,
        &state,
        &on_change,
        |s, v| s.video_resolution = v,
    ));
    vgroup.add(&combo(
        "Frame rate",
        Some("60 fps where the lens's mode allows it, else 30"),
        &[30u32, 60],
        |f| if f >= 60 { "60 fps" } else { "30 fps" },
        s.video_fps,
        &state,
        &on_change,
        |s, v| s.video_fps = v,
    ));
    vgroup.add(&switch(
        "Stabilization",
        "Steadies videos with the gyroscope; crops 10 % more",
        s.stabilization,
        &state,
        &on_change,
        |s, v| s.stabilization = v,
    ));
    video.add(&vgroup);

    // Advanced and about.
    let advanced = adw::PreferencesPage::builder()
        .title("Advanced")
        .name("advanced")
        .icon_name("preferences-other-symbolic")
        .build();
    let adv = adw::PreferencesGroup::builder().title("Advanced").build();
    adv.add(&switch(
        "Tap to focus",
        "Tap the picture to focus there",
        s.tap_to_focus,
        &state,
        &on_change,
        |s, v| s.tap_to_focus = v,
    ));
    adv.add(&switch(
        "Volume keys take photos",
        "While the camera is in front",
        s.volume_shutter,
        &state,
        &on_change,
        |s, v| s.volume_shutter = v,
    ));
    adv.add(&switch(
        "Keep Pro settings",
        "Off: every launch starts fully automatic",
        s.keep_pro,
        &state,
        &on_change,
        |s, v| s.keep_pro = v,
    ));
    adv.add(&switch(
        "Low-light lens",
        "In dim light, the main camera shows the telephoto's zoom (not in Pro)",
        s.low_light,
        &state,
        &on_change,
        |s, v| s.low_light = v,
    ));
    adv.add(&later("RAW (DNG) with JPEG", "Coming later"));
    adv.add(&switch(
        "Debug overlay",
        "Frame rate, lens and switch time over the picture",
        s.debug_overlay,
        &state,
        &on_change,
        |s, v| s.debug_overlay = v,
    ));
    advanced.add(&adv);

    let about = adw::PreferencesGroup::builder().title("About").build();
    about.add(&info(
        "Camera",
        &format!("omacam {}", env!("CARGO_PKG_VERSION")),
    ));
    about.add(&info("License", "GPL-3.0-or-later"));
    about.add(&info(
        "Credits",
        "Theme support adapted from gliff by Kevin McConnell (MIT)",
    ));
    advanced.add(&about);

    let dialog = adw::PreferencesDialog::builder()
        .title("Camera settings")
        .search_enabled(false)
        .build();
    dialog.add(&photo);
    dialog.add(&video);
    dialog.add(&advanced);
    dialog
}
