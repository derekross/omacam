//! How the phone is held, and what that does to a photo's or a video's
//! orientation.
//!
//! The accelerometer (iio-sensor-proxy's `AccelerometerOrientation`) says
//! which edge of the phone is up. Frames are first turned upright for a
//! phone held in portrait ([`crate::lens::Lens::upright_clockwise`]);
//! held another way, the picture needs a further turn. A back camera turns
//! with the phone, so the scene turns the other way in its frames and the
//! fix turns with the phone: held with its left edge up (turned 90°
//! clockwise), the picture turns 90° clockwise more. The front camera
//! faces the other way, so the same hold turns its (unmirrored) picture
//! the opposite way.

/// Which edge of the phone is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeviceOrientation {
    /// Portrait, the top edge up (also when unknown or flat).
    #[default]
    Normal,
    /// Upside down.
    BottomUp,
    /// Landscape, turned 90° clockwise: the left edge is up.
    LeftUp,
    /// Landscape, turned 90° counter-clockwise: the right edge is up.
    RightUp,
}

impl DeviceOrientation {
    /// From iio-sensor-proxy's `AccelerometerOrientation` ("normal",
    /// "bottom-up", "left-up", "right-up"; "undefined" when flat).
    pub fn from_sensor(s: &str) -> Option<DeviceOrientation> {
        match s {
            "normal" => Some(DeviceOrientation::Normal),
            "bottom-up" => Some(DeviceOrientation::BottomUp),
            "left-up" => Some(DeviceOrientation::LeftUp),
            "right-up" => Some(DeviceOrientation::RightUp),
            _ => None,
        }
    }

    /// Clockwise degrees the phone is turned from portrait.
    pub fn degrees(self) -> u32 {
        match self {
            DeviceOrientation::Normal => 0,
            DeviceOrientation::LeftUp => 90,
            DeviceOrientation::BottomUp => 180,
            DeviceOrientation::RightUp => 270,
        }
    }

    pub fn is_landscape(self) -> bool {
        matches!(self, DeviceOrientation::LeftUp | DeviceOrientation::RightUp)
    }
}

/// The clockwise turn to add to a camera's portrait-upright turn for a
/// phone held as `held` (front: the selfie camera).
pub fn extra_turn(held: DeviceOrientation, front: bool) -> u32 {
    let d = held.degrees();
    if front {
        (360 - d) % 360
    } else {
        d
    }
}

/// The whole clockwise turn of a frame from `upright` (the camera's
/// portrait turn) for a phone held as `held`.
pub fn turn(upright: u32, held: DeviceOrientation, front: bool) -> u32 {
    (upright + extra_turn(held, front)) % 360
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_sensor_names() {
        assert_eq!(
            DeviceOrientation::from_sensor("left-up"),
            Some(DeviceOrientation::LeftUp)
        );
        assert_eq!(
            DeviceOrientation::from_sensor("right-up"),
            Some(DeviceOrientation::RightUp)
        );
        assert_eq!(
            DeviceOrientation::from_sensor("bottom-up"),
            Some(DeviceOrientation::BottomUp)
        );
        assert_eq!(
            DeviceOrientation::from_sensor("normal"),
            Some(DeviceOrientation::Normal)
        );
        assert_eq!(DeviceOrientation::from_sensor("undefined"), None);
    }

    #[test]
    fn back_cameras_turn_with_the_phone() {
        // The main camera: 270 clockwise for portrait.
        assert_eq!(turn(270, DeviceOrientation::Normal, false), 270);
        assert_eq!(turn(270, DeviceOrientation::LeftUp, false), 0);
        assert_eq!(turn(270, DeviceOrientation::RightUp, false), 180);
        assert_eq!(turn(270, DeviceOrientation::BottomUp, false), 90);
        // The ultrawide: 90 for portrait.
        assert_eq!(turn(90, DeviceOrientation::LeftUp, false), 180);
    }

    #[test]
    fn the_front_camera_turns_the_other_way() {
        assert_eq!(turn(270, DeviceOrientation::Normal, true), 270);
        assert_eq!(turn(270, DeviceOrientation::LeftUp, true), 180);
        assert_eq!(turn(270, DeviceOrientation::RightUp, true), 0);
        assert_eq!(turn(270, DeviceOrientation::BottomUp, true), 90);
    }

    #[test]
    fn landscape_is_left_or_right_up() {
        assert!(DeviceOrientation::LeftUp.is_landscape());
        assert!(DeviceOrientation::RightUp.is_landscape());
        assert!(!DeviceOrientation::BottomUp.is_landscape());
    }
}
