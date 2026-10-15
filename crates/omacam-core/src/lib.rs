//! The camera logic of omacam, with no UI and no camera library.
//!
//! - [`backend`]: the interface a camera backend implements (libcamera on
//!   the phone, a fake one in tests and on the laptop).
//! - [`lens`]: the lens table, which says what each camera is.
//! - [`zoom`]: the lens ladder, zoom thresholds and the crop of a lens.
//! - [`framing`]: which part of each lens's frame a zoom shows, so the
//!   lenses frame the scene alike.
//! - [`switch`]: the stream state machine that stops one camera and starts
//!   the next.
//! - [`capture`]: the still-capture sequence.
//! - [`lowlight`]: the low-light fallback from the telephoto to the main.
//! - [`exposure`]: carrying exposure from one camera to the next.
//! - [`stabilize`]: video stabilization from the gyroscope.
//! - [`controller`]: ties them together; the UI feeds it input and backend
//!   events and sends the commands it returns to the backend.

pub mod backend;
pub mod capture;
pub mod controller;
pub mod exposure;
pub mod exposure_control;
pub mod focus;
pub mod framing;
pub mod lens;
pub mod lowlight;
pub mod orientation;
pub mod stabilize;
pub mod switch;
pub mod zoom;

pub use backend::{Backend, CameraInfo, Command, Event, Frame, FrameData, PixelLayout, Size};
pub use controller::{Controller, Output, ViewState};
pub use lens::{Lens, LensTable, Role};
