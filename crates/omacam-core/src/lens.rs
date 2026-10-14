//! The lens table: what each camera is, how it relates to the others, and
//! which modes to ask of it.
//!
//! A table is a TOML file (`data/cheetah.toml` for the Pixel 7 Pro). It
//! matches cameras by the library's id. Cameras the table doesn't know
//! still work: the first back camera becomes the main one and the first
//! front camera the selfie one, at the library's default mode.

use std::path::Path;

use serde::Deserialize;

use crate::backend::{CameraInfo, Location, Size};

/// What a camera does in the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Ultrawide,
    Main,
    Tele,
    Front,
}

impl Role {
    pub fn faces_front(self) -> bool {
        self == Role::Front
    }
}

/// One camera.
#[derive(Debug, Clone, PartialEq)]
pub struct Lens {
    pub role: Role,
    pub name: String,
    pub id: String,
    /// Field of view relative to the main camera (the front camera's ladder
    /// is its own, so its zoom is 1.0).
    pub zoom: f64,
    pub f_number: Option<f64>,
    pub focal_length_35mm: Option<u32>,
    pub preview: Option<Size>,
    pub still: Option<Size>,
    pub autofocus: bool,
    pub mirror: bool,
    /// The camera's Rotation in libcamera's sense (correct by turning
    /// counter-clockwise). Filled from the camera's own Rotation property
    /// unless the table overrides it. See [`Lens::upright_clockwise`].
    pub rotation: u32,
}

/// Clockwise degrees that turn a frame upright, for a camera whose
/// libcamera `Rotation` is `rotation`. libcamera defines Rotation as the
/// counter-clockwise difference between the sensor's reference system and
/// the projected scene's, and says the image needs a correction of that
/// many degrees counter-clockwise: Rotation 90 means turning the frame
/// 90 degrees counter-clockwise, which is 270 clockwise.
pub fn upright_clockwise(rotation: u32) -> u32 {
    (360 - rotation % 360) % 360
}

impl Lens {
    /// Clockwise degrees that turn this camera's frames upright.
    pub fn upright_clockwise(&self) -> u32 {
        upright_clockwise(self.rotation)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TableFile {
    #[allow(dead_code)]
    device: Option<String>,
    max_zoom: Option<f64>,
    #[serde(default, rename = "lens")]
    lenses: Vec<LensEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct LensEntry {
    role: Role,
    name: Option<String>,
    id: String,
    zoom: f64,
    f_number: Option<f64>,
    focal_length_35mm: Option<u32>,
    preview: Option<[u32; 2]>,
    still: Option<[u32; 2]>,
    #[serde(default)]
    autofocus: bool,
    #[serde(default)]
    mirror: bool,
    rotation: Option<u32>,
}

#[derive(Debug, thiserror::Error)]
pub enum TableError {
    #[error("lens table: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("lens table: {0}")]
    Invalid(String),
    #[error("lens table {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
}

/// The lens table as written, before it meets the cameras.
#[derive(Debug, Clone)]
pub struct TableSpec {
    max_zoom: f64,
    entries: Vec<LensEntry>,
}

/// The built-in table for the Pixel 7 Pro.
pub const CHEETAH: &str = include_str!("../../../data/cheetah.toml");

/// Highest zoom when the table doesn't say.
pub const DEFAULT_MAX_ZOOM: f64 = 8.0;
/// Highest digital zoom of the front camera.
pub const FRONT_MAX_ZOOM: f64 = 4.0;

impl TableSpec {
    pub fn parse(text: &str) -> Result<TableSpec, TableError> {
        let file: TableFile = toml::from_str(text)?;
        for e in &file.lenses {
            if !(e.zoom.is_finite() && e.zoom > 0.0) {
                return Err(TableError::Invalid(format!("{}: zoom must be > 0", e.id)));
            }
            if let Some(r) = e.rotation {
                if r % 90 != 0 || r >= 360 {
                    return Err(TableError::Invalid(format!(
                        "{}: rotation must be 0, 90, 180 or 270",
                        e.id
                    )));
                }
            }
        }
        let max_zoom = file.max_zoom.unwrap_or(DEFAULT_MAX_ZOOM);
        if !(max_zoom.is_finite() && max_zoom >= 1.0) {
            return Err(TableError::Invalid("max_zoom must be >= 1".into()));
        }
        Ok(TableSpec {
            max_zoom,
            entries: file.lenses,
        })
    }

    pub fn load(path: &Path) -> Result<TableSpec, TableError> {
        let text = std::fs::read_to_string(path).map_err(|source| TableError::Read {
            path: path.display().to_string(),
            source,
        })?;
        TableSpec::parse(&text)
    }

    pub fn cheetah() -> TableSpec {
        TableSpec::parse(CHEETAH).expect("built-in lens table parses")
    }

    /// An empty table: every camera gets a generic entry.
    pub fn empty() -> TableSpec {
        TableSpec {
            max_zoom: DEFAULT_MAX_ZOOM,
            entries: Vec::new(),
        }
    }

    /// Match the table against the cameras present.
    pub fn resolve(&self, cameras: &[CameraInfo]) -> LensTable {
        let mut lenses = Vec::new();
        for cam in cameras {
            if let Some(e) = self.entries.iter().find(|e| e.id == cam.id) {
                lenses.push(Lens {
                    role: e.role,
                    name: e.name.clone().unwrap_or_else(|| cam.model.clone()),
                    id: cam.id.clone(),
                    zoom: e.zoom,
                    f_number: e.f_number,
                    focal_length_35mm: e.focal_length_35mm,
                    preview: e.preview.map(|[w, h]| Size::new(w, h)),
                    still: e.still.map(|[w, h]| Size::new(w, h)),
                    autofocus: e.autofocus,
                    mirror: e.mirror,
                    rotation: e.rotation.unwrap_or(cam.rotation % 360),
                });
            }
        }
        // Cameras the table doesn't know: one main and one front, at most.
        for cam in cameras {
            if lenses.iter().any(|l| l.id == cam.id) {
                continue;
            }
            let role = match cam.location {
                Location::Front => Role::Front,
                Location::Back | Location::External => Role::Main,
            };
            if lenses.iter().any(|l| l.role == role) {
                continue;
            }
            lenses.push(Lens {
                role,
                name: cam.model.clone(),
                id: cam.id.clone(),
                zoom: 1.0,
                f_number: None,
                focal_length_35mm: None,
                preview: None,
                still: None,
                autofocus: false,
                mirror: role == Role::Front,
                rotation: cam.rotation % 360,
            });
        }
        LensTable {
            lenses,
            max_zoom: self.max_zoom,
        }
    }
}

/// The lenses present on this device.
#[derive(Debug, Clone, PartialEq)]
pub struct LensTable {
    lenses: Vec<Lens>,
    max_zoom: f64,
}

/// Which way the app looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Facing {
    Back,
    Front,
}

impl LensTable {
    pub fn lenses(&self) -> &[Lens] {
        &self.lenses
    }

    pub fn get(&self, index: usize) -> &Lens {
        &self.lenses[index]
    }

    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.lenses.iter().position(|l| l.id == id)
    }

    pub fn by_role(&self, role: Role) -> Option<usize> {
        self.lenses.iter().position(|l| l.role == role)
    }

    pub fn has(&self, facing: Facing) -> bool {
        !self.ladder(facing).is_empty()
    }

    /// The lenses of one side, ordered by zoom: the indices into
    /// [`LensTable::lenses`].
    pub fn ladder(&self, facing: Facing) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..self.lenses.len())
            .filter(|&i| (self.lenses[i].role == Role::Front) == (facing == Facing::Front))
            .collect();
        idx.sort_by(|&a, &b| self.lenses[a].zoom.total_cmp(&self.lenses[b].zoom));
        idx
    }

    pub fn max_zoom(&self, facing: Facing) -> f64 {
        match facing {
            Facing::Back => self.max_zoom,
            Facing::Front => FRONT_MAX_ZOOM,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn cheetah_cameras() -> Vec<CameraInfo> {
        let cam = |id: &str, location, rotation| CameraInfo {
            id: id.into(),
            location,
            rotation,
            model: "sensor".into(),
        };
        vec![
            cam(
                "/base/soc@0/usi@109100c0/i2c@10910000/camera-sensor@10",
                Location::Front,
                90,
            ),
            cam(
                "/base/soc@0/usi@109000c0/i2c@10900000/camera-sensor@3d",
                Location::Back,
                90,
            ),
            cam(
                "/base/soc@0/usi@109200c0/i2c@10920000/camera-sensor@1a",
                Location::Back,
                270,
            ),
            cam(
                "/base/soc@0/usi@109300c0/i2c@10930000/camera-sensor@2d",
                Location::Back,
                270,
            ),
        ]
    }

    #[test]
    fn builtin_table_matches_all_four_cameras() {
        let t = TableSpec::cheetah().resolve(&cheetah_cameras());
        assert_eq!(t.lenses().len(), 4);
        let roles: Vec<Role> = t
            .ladder(Facing::Back)
            .iter()
            .map(|&i| t.get(i).role)
            .collect();
        assert_eq!(roles, [Role::Ultrawide, Role::Main, Role::Tele]);
        assert_eq!(t.ladder(Facing::Front).len(), 1);
        let uw = t.get(t.by_role(Role::Ultrawide).unwrap());
        assert_eq!(uw.zoom, 0.45);
        assert_eq!(uw.rotation, 270, "rotation comes from the camera");
        let tele = t.get(t.by_role(Role::Tele).unwrap());
        assert_eq!(tele.zoom, 4.78);
        assert!(!tele.autofocus);
        assert!(t.get(t.by_role(Role::Front).unwrap()).mirror);
        assert_eq!(t.max_zoom(Facing::Back), 15.0);
    }

    /// What the Pixel 7 Pro's cameras report (`omacam --list`, 2026-10-14,
    /// kernel 00545) and how far each frame must turn clockwise to be
    /// upright: main and front Rotation 90, ultrawide and telephoto 270.
    #[test]
    fn cheetah_frames_turn_upright_counter_clockwise_by_rotation() {
        let t = TableSpec::cheetah().resolve(&cheetah_cameras());
        let cw = |role| t.get(t.by_role(role).unwrap()).upright_clockwise();
        assert_eq!(cw(Role::Main), 270);
        assert_eq!(cw(Role::Ultrawide), 90);
        assert_eq!(cw(Role::Tele), 90);
        assert_eq!(cw(Role::Front), 270);
    }

    #[test]
    fn upright_clockwise_inverts_libcamera_rotation() {
        assert_eq!(upright_clockwise(0), 0);
        assert_eq!(upright_clockwise(90), 270);
        assert_eq!(upright_clockwise(180), 180);
        assert_eq!(upright_clockwise(270), 90);
        assert_eq!(upright_clockwise(360), 0);
    }

    #[test]
    fn missing_cameras_drop_out_of_the_ladder() {
        let mut cams = cheetah_cameras();
        cams.retain(|c| !c.id.ends_with("@2d"));
        let t = TableSpec::cheetah().resolve(&cams);
        assert_eq!(t.ladder(Facing::Back).len(), 2);
        assert!(t.by_role(Role::Tele).is_none());
    }

    #[test]
    fn unknown_cameras_get_generic_roles() {
        let cams = vec![
            CameraInfo {
                id: "usb-1".into(),
                location: Location::External,
                rotation: 0,
                model: "Webcam".into(),
            },
            CameraInfo {
                id: "usb-2".into(),
                location: Location::External,
                rotation: 0,
                model: "Second".into(),
            },
        ];
        let t = TableSpec::cheetah().resolve(&cams);
        assert_eq!(t.lenses().len(), 1, "only one generic main camera");
        assert_eq!(t.get(0).role, Role::Main);
        assert_eq!(t.get(0).name, "Webcam");
        assert!(!t.has(Facing::Front));
    }

    #[test]
    fn table_override_of_rotation_and_validation() {
        let spec =
            TableSpec::parse("[[lens]]\nrole = \"main\"\nid = \"a\"\nzoom = 1.0\nrotation = 180\n")
                .unwrap();
        let cams = vec![CameraInfo {
            id: "a".into(),
            location: Location::Back,
            rotation: 90,
            model: "m".into(),
        }];
        assert_eq!(spec.resolve(&cams).get(0).rotation, 180);
        assert!(TableSpec::parse("[[lens]]\nrole = \"main\"\nid = \"a\"\nzoom = 0\n").is_err());
        assert!(TableSpec::parse(
            "[[lens]]\nrole = \"main\"\nid = \"a\"\nzoom = 1\nrotation = 45\n"
        )
        .is_err());
        assert!(TableSpec::parse("[[lens]]\nrole = \"side\"\nid = \"a\"\nzoom = 1\n").is_err());
    }
}
