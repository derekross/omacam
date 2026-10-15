//! The Pro panel: shutter, ISO, white balance and manual focus, each with
//! an Auto ("A") state, over the bottom of the picture.
//!
//! A chip per control shows its value: the manual one in the accent
//! color, or auto's current choice (from the frame metadata) dimmed. A
//! chip whose control the camera doesn't offer is grayed out, with a
//! tooltip saying so (libcamera 0.7.2-4.8 offers none of them; they light
//! up as libcamera does). Tapping a chip opens its editor: an "A" button
//! and a slider over the values the camera allows, or the white-balance
//! presets.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;
use gtk4 as gtk;

use omacam_core::backend::{Awb, AwbPreset, Setting};
use omacam_core::controller::ExposureView;
use omacam_core::exposure_control::{format_shutter, iso, iso_stops, shutter_stops};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Shutter,
    Iso,
    WhiteBalance,
    Focus,
}

/// What the user did in the panel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Shutter(Setting<i32>),
    Iso(Setting<u32>),
    WhiteBalance(Setting<Awb>),
    Focus(Setting<f32>),
    Histogram(bool),
    AllAuto,
}

const NOT_OFFERED: &str = "This camera doesn't offer it (yet)";

pub struct ProPanel {
    pub root: gtk::Box,
    chips: Vec<(Kind, gtk::Button, gtk::Label, gtk::Label)>,
    histogram: gtk::ToggleButton,
    editor: gtk::Box,
    selected: Cell<Option<Kind>>,
    /// The view the open editor was built for.
    built_for: RefCell<Option<ExposureView>>,
    on_action: Rc<dyn Fn(Action)>,
    /// True while one of the panel's own controls reports a change: the
    /// update that follows mustn't rebuild the editor under the finger.
    emitting: Rc<Cell<bool>>,
}

fn chip(name: &str) -> (gtk::Button, gtk::Label, gtk::Label) {
    let title = gtk::Label::new(Some(name));
    title.add_css_class("pro-chip-name");
    let value = gtk::Label::new(Some("A"));
    value.add_css_class("pro-chip-value");
    let b = gtk::Box::new(gtk::Orientation::Vertical, 0);
    b.append(&title);
    b.append(&value);
    let button = gtk::Button::new();
    button.set_child(Some(&b));
    button.add_css_class("pro-chip");
    (button, title, value)
}

impl ProPanel {
    pub fn new(on_action: impl Fn(Action) + 'static) -> Rc<ProPanel> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
        root.add_css_class("pro-panel");
        root.set_hexpand(true);
        let editor = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        editor.add_css_class("pro-editor");
        editor.set_visible(false);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        row.set_homogeneous(true);
        let mut chips = Vec::new();
        for (kind, name) in [
            (Kind::Shutter, "Shutter"),
            (Kind::Iso, "ISO"),
            (Kind::WhiteBalance, "WB"),
            (Kind::Focus, "Focus"),
        ] {
            let (button, title, value) = chip(name);
            row.append(&button);
            chips.push((kind, button, title, value));
        }
        let histogram = gtk::ToggleButton::with_label("Hist");
        histogram.set_tooltip_text(Some("Histogram"));
        histogram.add_css_class("pro-chip");
        row.append(&histogram);
        root.append(&editor);
        root.append(&row);
        let emitting = Rc::new(Cell::new(false));
        let flag = emitting.clone();
        let panel = Rc::new(ProPanel {
            root,
            chips,
            histogram,
            editor,
            selected: Cell::new(None),
            built_for: RefCell::new(None),
            on_action: Rc::new(move |a| {
                flag.set(true);
                on_action(a);
                flag.set(false);
            }),
            emitting,
        });
        for (kind, button, _, _) in &panel.chips {
            let (p, kind) = (Rc::downgrade(&panel), *kind);
            button.connect_clicked(move |_| {
                if let Some(p) = p.upgrade() {
                    let now = if p.selected.get() == Some(kind) {
                        None
                    } else {
                        Some(kind)
                    };
                    p.selected.set(now);
                    p.built_for.borrow_mut().take();
                    p.rebuild_editor();
                }
            });
        }
        let p = Rc::downgrade(&panel);
        panel.histogram.connect_toggled(move |b| {
            if let Some(p) = p.upgrade() {
                (p.on_action)(Action::Histogram(b.is_active()));
            }
        });
        panel
    }

    /// Open the editor of one control (the test script uses this).
    pub fn select(&self, kind: Kind) {
        self.selected.set(Some(kind));
        self.built_for.borrow_mut().take();
    }

    /// Bring the chips (and, when the camera changed, the editor) up to
    /// date.
    pub fn update(&self, view: &ExposureView) {
        let caps = &view.caps;
        for (kind, button, _, value) in &self.chips {
            let (offered, manual, text) = match kind {
                Kind::Shutter => (
                    caps.manual_shutter(),
                    view.pro.shutter.manual().map(format_shutter),
                    view.shutter_us.map(format_shutter),
                ),
                Kind::Iso => (
                    caps.manual_gain(),
                    view.pro
                        .gain
                        .manual()
                        .map(|g| iso(g as f64, view.base_iso).to_string()),
                    view.iso.map(|i| i.to_string()),
                ),
                Kind::WhiteBalance => (
                    caps.white_balance(),
                    view.pro.white_balance.manual().map(|w| match w {
                        Awb::Preset(p) => match p {
                            AwbPreset::Daylight => "Sun",
                            AwbPreset::Cloudy => "Cloud",
                            AwbPreset::Incandescent => "Bulb",
                            AwbPreset::Fluorescent => "Tube",
                        }
                        .to_string(),
                        Awb::Kelvin(k) => format!("{k} K"),
                        Awb::Gains(_) => "Manual".into(),
                    }),
                    None,
                ),
                Kind::Focus => (
                    caps.lens_position.is_some(),
                    view.pro.lens.manual().map(|p| format!("{p:.1}")),
                    view.lens_position.map(|p| format!("{p:.1}")),
                ),
            };
            button.set_sensitive(offered);
            button.set_tooltip_text((!offered).then_some(NOT_OFFERED));
            match (offered, manual) {
                (true, Some(m)) => {
                    value.set_label(&m);
                    button.add_css_class("manual");
                }
                _ => {
                    // Auto: show what auto chose, when the camera says.
                    value.set_label(&match text {
                        Some(t) if offered => format!("A {t}"),
                        _ => "A".into(),
                    });
                    button.remove_css_class("manual");
                }
            }
        }
        // Rebuild for another camera, or when the values changed from
        // outside the editor ("All auto", a lens switch, a script).
        let changed = self.built_for.borrow().as_ref().is_none_or(|b| {
            b.caps != view.caps
                || b.base_iso != view.base_iso
                || (b.pro != view.pro && !self.emitting.get())
        });
        if changed {
            *self.built_for.borrow_mut() = Some(view.clone());
            self.rebuild_editor();
        } else if let Some(b) = self.built_for.borrow_mut().as_mut() {
            b.pro = view.pro;
        }
    }

    fn rebuild_editor(&self) {
        while let Some(c) = self.editor.first_child() {
            self.editor.remove(&c);
        }
        let Some(kind) = self.selected.get() else {
            self.editor.set_visible(false);
            return;
        };
        let Some(view) = self.built_for.borrow().clone() else {
            self.editor.set_visible(false);
            return;
        };
        let act = self.on_action.clone();
        let auto = gtk::ToggleButton::with_label("A");
        auto.add_css_class("pro-auto");
        auto.set_tooltip_text(Some("Automatic"));
        match kind {
            Kind::Shutter => {
                let stops = view
                    .caps
                    .exposure_time
                    .map(shutter_stops)
                    .unwrap_or_default();
                self.slider_editor(
                    &auto,
                    stops.clone(),
                    view.pro.shutter.manual(),
                    move |i| {
                        act(Action::Shutter(match i {
                            Some(i) => Setting::Manual(stops[i]),
                            None => Setting::Auto,
                        }))
                    },
                    format_shutter,
                );
            }
            Kind::Iso => {
                let stops = view
                    .caps
                    .gain
                    .map(|g| iso_stops(view.base_iso, g))
                    .unwrap_or_default();
                let current = view.pro.gain.manual().map(|g| iso(g as f64, view.base_iso));
                self.slider_editor(
                    &auto,
                    stops.clone(),
                    current,
                    move |i| {
                        act(Action::Iso(match i {
                            Some(i) => Setting::Manual(stops[i]),
                            None => Setting::Auto,
                        }))
                    },
                    |v| format!("ISO {v}"),
                );
            }
            Kind::Focus => {
                let (lo, hi) = view.caps.lens_position.unwrap_or((0.0, 1.0));
                let scale = gtk::Scale::with_range(
                    gtk::Orientation::Horizontal,
                    lo as f64,
                    hi as f64,
                    (hi - lo) as f64 / 100.0,
                );
                scale.set_hexpand(true);
                scale.set_draw_value(false);
                scale.set_value(view.pro.lens.manual().unwrap_or(lo) as f64);
                scale.set_tooltip_text(Some("Far ← → near"));
                auto.set_active(view.pro.lens.is_auto());
                let a2 = act.clone();
                let s2 = scale.clone();
                auto.connect_toggled(move |b| {
                    if b.is_active() {
                        a2(Action::Focus(Setting::Auto));
                    } else {
                        a2(Action::Focus(Setting::Manual(s2.value() as f32)));
                    }
                });
                let auto2 = auto.clone();
                scale.connect_value_changed(move |s| {
                    auto2.set_active(false);
                    act(Action::Focus(Setting::Manual(s.value() as f32)));
                });
                self.editor.append(&auto);
                self.editor.append(&scale);
            }
            Kind::WhiteBalance => {
                auto.set_active(view.pro.white_balance.is_auto());
                let group_leader = auto.clone();
                let a2 = act.clone();
                auto.connect_toggled(move |b| {
                    if b.is_active() {
                        a2(Action::WhiteBalance(Setting::Auto));
                    }
                });
                self.editor.append(&auto);
                // Four equal presets in their own homogeneous row, with
                // short labels: at the phone's 480 px logical width the
                // long names pushed the last chip off the right edge.
                let presets = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                presets.set_homogeneous(true);
                presets.set_hexpand(true);
                for preset in AwbPreset::ALL {
                    let short = match preset {
                        AwbPreset::Daylight => "Sun",
                        AwbPreset::Cloudy => "Cloud",
                        AwbPreset::Incandescent => "Bulb",
                        AwbPreset::Fluorescent => "Tube",
                    };
                    let b = gtk::ToggleButton::with_label(short);
                    b.set_tooltip_text(Some(preset.label()));
                    b.set_hexpand(true);
                    b.add_css_class("pro-preset");
                    b.set_group(Some(&group_leader));
                    b.set_active(view.pro.white_balance == Setting::Manual(Awb::Preset(preset)));
                    let a3 = act.clone();
                    b.connect_toggled(move |b| {
                        if b.is_active() {
                            a3(Action::WhiteBalance(Setting::Manual(Awb::Preset(preset))));
                        }
                    });
                    presets.append(&b);
                }
                self.editor.append(&presets);
            }
        }
        let reset = gtk::Button::with_label("All auto");
        reset.add_css_class("pro-preset");
        let a4 = self.on_action.clone();
        reset.connect_clicked(move |_| a4(Action::AllAuto));
        if kind != Kind::WhiteBalance {
            self.editor.append(&reset);
        }
        self.editor.set_visible(true);
    }

    /// An "A" button and a slider over `stops`.
    fn slider_editor<T: Copy + PartialEq + PartialOrd + 'static>(
        &self,
        auto: &gtk::ToggleButton,
        stops: Vec<T>,
        current: Option<T>,
        apply: impl Fn(Option<usize>) + 'static,
        show: impl Fn(T) -> String + 'static,
    ) {
        let n = stops.len().max(1);
        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, (n - 1) as f64, 1.0);
        scale.set_hexpand(true);
        scale.set_draw_value(false);
        scale.set_round_digits(0);
        let index = current
            .and_then(|c| stops.iter().position(|s| *s >= c))
            .unwrap_or(n / 2);
        scale.set_value(index as f64);
        let label = gtk::Label::new(None);
        label.add_css_class("pro-value");
        label.set_width_chars(7);
        let show = Rc::new(show);
        if let Some(v) = stops.get(index) {
            label.set_label(&if current.is_some() {
                show(*v)
            } else {
                "Auto".into()
            });
        }
        auto.set_active(current.is_none());
        let apply = Rc::new(apply);
        let (a2, s2, l2, st2, sh2) = (
            apply.clone(),
            scale.clone(),
            label.clone(),
            stops.clone(),
            show.clone(),
        );
        auto.connect_toggled(move |b| {
            if b.is_active() {
                l2.set_label("Auto");
                a2(None);
            } else {
                let i = (s2.value().round() as usize).min(st2.len().saturating_sub(1));
                if let Some(v) = st2.get(i) {
                    l2.set_label(&sh2(*v));
                }
                a2(Some(i));
            }
        });
        let auto2 = auto.clone();
        let l3 = label.clone();
        scale.connect_value_changed(move |s| {
            let i = (s.value().round() as usize).min(stops.len().saturating_sub(1));
            if let Some(v) = stops.get(i) {
                l3.set_label(&show(*v));
            }
            auto2.set_active(false);
            apply(Some(i));
        });
        self.editor.append(auto);
        self.editor.append(&scale);
        self.editor.append(&label);
    }
}
