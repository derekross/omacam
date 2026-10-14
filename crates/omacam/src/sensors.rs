//! How the phone is held: iio-sensor-proxy's accelerometer orientation
//! (`net.hadess.SensorProxy` on the system bus, which omamobile-sensors
//! provides on the phone). Claimed while omacam runs; released when it's
//! dropped. Without the service the phone counts as held upright.

use std::cell::Cell;
use std::rc::Rc;

use gtk4::gio;
use gtk4::gio::prelude::*;
use gtk4::glib;
use omacam_core::orientation::DeviceOrientation;

const NAME: &str = "net.hadess.SensorProxy";
const PATH: &str = "/net/hadess/SensorProxy";

pub struct Accelerometer {
    proxy: gio::DBusProxy,
}

fn read(proxy: &gio::DBusProxy) -> Option<DeviceOrientation> {
    proxy
        .cached_property("AccelerometerOrientation")
        .and_then(|v| v.get::<String>())
        .and_then(|s| DeviceOrientation::from_sensor(&s))
}

impl Accelerometer {
    /// Claim the accelerometer and keep `held` up to date. A flat phone
    /// ("undefined") keeps the last known hold.
    pub fn claim(held: Rc<Cell<DeviceOrientation>>) -> Option<Accelerometer> {
        let proxy = gio::DBusProxy::for_bus_sync(
            gio::BusType::System,
            gio::DBusProxyFlags::NONE,
            None,
            NAME,
            PATH,
            NAME,
            gio::Cancellable::NONE,
        )
        .map_err(|e| tracing::info!(error = %e, "no accelerometer service"))
        .ok()?;
        let has = proxy
            .cached_property("HasAccelerometer")
            .and_then(|v| v.get::<bool>())
            .unwrap_or(false);
        if !has {
            tracing::info!("no accelerometer");
            return None;
        }
        if let Err(e) = proxy.call_sync(
            "ClaimAccelerometer",
            None,
            gio::DBusCallFlags::NONE,
            1000,
            gio::Cancellable::NONE,
        ) {
            tracing::warn!(error = %e, "accelerometer not claimed");
            return None;
        }
        if let Some(o) = read(&proxy) {
            held.set(o);
        }
        proxy.connect_local("g-properties-changed", false, move |args| {
            let proxy = args[0].get::<gio::DBusProxy>().ok()?;
            if let Some(o) = read(&proxy) {
                if held.get() != o {
                    tracing::debug!(?o, "phone held");
                }
                held.set(o);
            }
            None::<glib::Value>
        });
        tracing::info!("accelerometer claimed");
        Some(Accelerometer { proxy })
    }
}

impl Drop for Accelerometer {
    fn drop(&mut self) {
        let _ = self.proxy.call_sync(
            "ReleaseAccelerometer",
            None,
            gio::DBusCallFlags::NONE,
            500,
            gio::Cancellable::NONE,
        );
    }
}
