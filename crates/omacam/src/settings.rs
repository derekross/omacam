//! The user's settings, kept in `~/.config/omacam/settings.toml`.
//!
//! Every field has a default, and unknown or missing keys fall back to it,
//! so a settings file from an older or newer omacam still loads. A file
//! that doesn't parse is left alone (renamed aside) rather than lost.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Aspect {
    /// The whole frame of the lens's mode.
    #[default]
    Full,
    #[serde(rename = "4:3")]
    FourThree,
    #[serde(rename = "16:9")]
    SixteenNine,
    #[serde(rename = "1:1")]
    Square,
}

impl Aspect {
    pub const ALL: [Aspect; 4] = [
        Aspect::Full,
        Aspect::FourThree,
        Aspect::SixteenNine,
        Aspect::Square,
    ];

    /// Width over height of an upright (portrait) photo, None for full.
    pub fn portrait_ratio(self) -> Option<f64> {
        match self {
            Aspect::Full => None,
            Aspect::FourThree => Some(3.0 / 4.0),
            Aspect::SixteenNine => Some(9.0 / 16.0),
            Aspect::Square => Some(1.0),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Aspect::Full => "Full frame",
            Aspect::FourThree => "4:3",
            Aspect::SixteenNine => "16:9",
            Aspect::Square => "1:1",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Resolution {
    /// The lens's full-resolution still mode.
    #[default]
    Full,
    /// Half the width and height: a quarter of the pixels.
    Medium,
}

impl Resolution {
    pub const ALL: [Resolution; 2] = [Resolution::Full, Resolution::Medium];

    pub fn label(self) -> &'static str {
        match self {
            Resolution::Full => "Full",
            Resolution::Medium => "Medium (a quarter of the pixels)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Timer {
    #[default]
    Off,
    #[serde(rename = "3s")]
    Three,
    #[serde(rename = "10s")]
    Ten,
}

impl Timer {
    pub const ALL: [Timer; 3] = [Timer::Off, Timer::Three, Timer::Ten];

    pub fn seconds(self) -> u32 {
        match self {
            Timer::Off => 0,
            Timer::Three => 3,
            Timer::Ten => 10,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Timer::Off => "Off",
            Timer::Three => "3 seconds",
            Timer::Ten => "10 seconds",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Grid {
    #[default]
    Off,
    #[serde(rename = "3x3")]
    Thirds,
    #[serde(rename = "4x4")]
    Quarters,
}

impl Grid {
    pub const ALL: [Grid; 3] = [Grid::Off, Grid::Thirds, Grid::Quarters];

    pub fn lines(self) -> u32 {
        match self {
            Grid::Off => 0,
            Grid::Thirds => 3,
            Grid::Quarters => 4,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Grid::Off => "Off",
            Grid::Thirds => "3 × 3",
            Grid::Quarters => "4 × 4",
        }
    }
}

pub const DEFAULT_QUALITY: u8 = 92;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub aspect: Aspect,
    pub resolution: Resolution,
    /// JPEG quality, 50-100.
    pub jpeg_quality: u8,
    pub timer: Timer,
    pub grid: Grid,
    /// Save front-camera photos mirrored, as the preview shows them.
    pub mirror_front: bool,
    /// Location tags (not built yet; off by default, CAMERA-APP-PLAN Q5).
    pub location: bool,
    /// The shutter sound (on by default, Q7).
    pub shutter_sound: bool,
    /// Where photos go; None for `$XDG_PICTURES_DIR/Camera`.
    pub save_dir: Option<PathBuf>,
    /// Volume keys take a photo while the camera has focus.
    pub volume_shutter: bool,
    pub tap_to_focus: bool,
    pub debug_overlay: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            aspect: Aspect::Full,
            resolution: Resolution::Full,
            jpeg_quality: DEFAULT_QUALITY,
            timer: Timer::Off,
            grid: Grid::Off,
            mirror_front: true,
            location: false,
            shutter_sound: true,
            save_dir: None,
            volume_shutter: true,
            tap_to_focus: true,
            debug_overlay: false,
        }
    }
}

impl Settings {
    /// `~/.config/omacam/settings.toml`.
    pub fn default_path() -> PathBuf {
        gtk4::glib::user_config_dir().join("omacam/settings.toml")
    }

    /// Load from `path`; defaults when it's missing. A file that doesn't
    /// parse is moved to `settings.toml.bad` and defaults are used.
    pub fn load(path: &Path) -> Settings {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => return Settings::default(),
        };
        match toml::from_str::<Settings>(&text) {
            Ok(s) => s.sanitised(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "settings unreadable; using defaults");
                let _ = std::fs::rename(path, path.with_extension("toml.bad"));
                Settings::default()
            }
        }
    }

    /// Write to `path` atomically.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text =
            toml::to_string_pretty(&self.clone().sanitised()).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("toml.part");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)
    }

    fn sanitised(mut self) -> Settings {
        self.jpeg_quality = self.jpeg_quality.clamp(50, 100);
        if self
            .save_dir
            .as_ref()
            .is_some_and(|p| p.as_os_str().is_empty())
        {
            self.save_dir = None;
        }
        self
    }

    /// The directory photos are saved in.
    pub fn photo_dir(&self) -> PathBuf {
        self.save_dir.clone().unwrap_or_else(default_photo_dir)
    }
}

pub fn default_photo_dir() -> PathBuf {
    gtk4::glib::user_special_dir(gtk4::glib::UserDirectory::Pictures)
        .unwrap_or_else(|| gtk4::glib::home_dir().join("Pictures"))
        .join("Camera")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("omacam-settings-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("settings.toml")
    }

    #[test]
    fn defaults_follow_the_owners_answers() {
        let s = Settings::default();
        assert!(s.shutter_sound, "Q7: shutter sound on");
        assert!(!s.location, "Q5: location off");
        assert!(s.mirror_front, "front photos saved as previewed");
        assert!(s.tap_to_focus);
        assert!(!s.debug_overlay);
        assert_eq!(s.aspect, Aspect::Full);
        assert_eq!(s.resolution, Resolution::Full);
        assert_eq!(s.jpeg_quality, 92);
        assert_eq!(s.timer, Timer::Off);
        assert_eq!(s.grid, Grid::Off);
        assert_eq!(s.save_dir, None);
    }

    #[test]
    fn a_missing_file_gives_defaults() {
        let p = tmp("missing");
        assert_eq!(Settings::load(&p), Settings::default());
    }

    #[test]
    fn save_and_load_round_trip() {
        let p = tmp("roundtrip");
        let s = Settings {
            aspect: Aspect::Square,
            resolution: Resolution::Medium,
            jpeg_quality: 80,
            timer: Timer::Ten,
            grid: Grid::Thirds,
            mirror_front: false,
            location: false,
            shutter_sound: false,
            save_dir: Some("/tmp/x".into()),
            volume_shutter: false,
            tap_to_focus: false,
            debug_overlay: true,
        };
        s.save(&p).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("aspect = \"1:1\""), "{text}");
        assert!(text.contains("timer = \"10s\""), "{text}");
        assert!(text.contains("grid = \"3x3\""), "{text}");
        assert_eq!(Settings::load(&p), s);
    }

    #[test]
    fn partial_and_unknown_keys_keep_defaults() {
        let p = tmp("partial");
        std::fs::write(&p, "shutter_sound = false\nsomething_new = 3\n").unwrap();
        let s = Settings::load(&p);
        assert!(!s.shutter_sound);
        assert_eq!(s.jpeg_quality, DEFAULT_QUALITY);
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let p = tmp("clamp");
        std::fs::write(&p, "jpeg_quality = 10\nsave_dir = \"\"\n").unwrap();
        let s = Settings::load(&p);
        assert_eq!(s.jpeg_quality, 50);
        assert_eq!(s.save_dir, None);
    }

    #[test]
    fn a_broken_file_is_moved_aside_not_lost() {
        let p = tmp("broken");
        std::fs::write(&p, "aspect = = nonsense").unwrap();
        assert_eq!(Settings::load(&p), Settings::default());
        assert!(p.with_extension("toml.bad").exists());
        assert!(!p.exists());
    }

    #[test]
    fn aspect_ratios_are_portrait() {
        assert_eq!(Aspect::FourThree.portrait_ratio(), Some(0.75));
        assert_eq!(Aspect::SixteenNine.portrait_ratio(), Some(9.0 / 16.0));
        assert_eq!(Aspect::Square.portrait_ratio(), Some(1.0));
        assert_eq!(Aspect::Full.portrait_ratio(), None);
    }
}
