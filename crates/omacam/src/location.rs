//! Where the phone is, from GeoClue (`org.freedesktop.GeoClue2` on the
//! system bus; on the phone omamobile-gnss feeds it the GNSS receiver).
//!
//! Only while the "Location tags" setting is on: then omacam asks GeoClue
//! for a client as desktop id `omacam` at street accuracy or better, and
//! keeps the newest fix. GeoClue answers a user's apps only while an
//! authorization agent runs for that user (omamobile-gnss's Location
//! switch starts one), and asks the agent about apps it doesn't list;
//! omacam's package lists `omacam` in GeoClue's configuration
//! (`/etc/geoclue/conf.d/91-omacam.conf`), so it isn't asked about.
//! Without GeoClue, without an agent, or before a fix, photos and videos
//! carry no location; nothing waits for one.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gtk4::gio;
use gtk4::gio::prelude::*;
use gtk4::glib;

const NAME: &str = "org.freedesktop.GeoClue2";
const MANAGER: &str = "/org/freedesktop/GeoClue2/Manager";
/// GeoClue's accuracy levels: 6 is street, 8 exact.
const ACCURACY_EXACT: u32 = 8;
/// A fix older than this isn't written into a photo.
pub const MAX_AGE: Duration = Duration::from_secs(10 * 60);

/// One fix.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fix {
    pub latitude: f64,
    pub longitude: f64,
    /// Meters above sea level, when known.
    pub altitude: Option<f64>,
    /// Horizontal accuracy, meters.
    pub accuracy: f64,
    /// When it arrived here.
    pub at: Instant,
}

impl Fix {
    /// The fix, if it's recent enough to stand for where a photo is taken.
    pub fn fresh(self, now: Instant) -> Option<Fix> {
        (now.saturating_duration_since(self.at) <= MAX_AGE).then_some(self)
    }
}

/// From GeoClue's Location properties. GeoClue reports an unknown
/// altitude as -1.7976931348623157e308 (-G_MAXDOUBLE).
pub fn fix_from(
    latitude: f64,
    longitude: f64,
    altitude: f64,
    accuracy: f64,
    at: Instant,
) -> Option<Fix> {
    if !(latitude.is_finite()
        && longitude.is_finite()
        && latitude.abs() <= 90.0
        && longitude.abs() <= 180.0)
    {
        return None;
    }
    let altitude = (altitude.is_finite() && altitude > -1e10).then_some(altitude);
    Some(Fix {
        latitude,
        longitude,
        altitude,
        accuracy: if accuracy.is_finite() {
            accuracy
        } else {
            f64::INFINITY
        },
        at,
    })
}

/// A GeoClue client on a thread of its own (with its own main context),
/// so a missing or slow GeoClue never holds up the window: every D-Bus
/// call there has a timeout, and the UI only reads the newest fix.
/// Stopped when dropped.
pub struct Locator {
    latest: Arc<Mutex<Option<Fix>>>,
    stop: Arc<AtomicBool>,
}

/// The longest any one GeoClue call may take.
const CALL_TIMEOUT_MS: i32 = 3000;

fn read_location(path: &str) -> Option<Fix> {
    let loc = gio::DBusProxy::for_bus_sync(
        gio::BusType::System,
        gio::DBusProxyFlags::NONE,
        None,
        NAME,
        path,
        "org.freedesktop.GeoClue2.Location",
        gio::Cancellable::NONE,
    )
    .ok()?;
    let get = |p: &str| loc.cached_property(p).and_then(|v| v.get::<f64>());
    fix_from(
        get("Latitude")?,
        get("Longitude")?,
        get("Altitude").unwrap_or(f64::MIN),
        get("Accuracy").unwrap_or(f64::INFINITY),
        Instant::now(),
    )
}

/// Ask GeoClue for a client, configure it and start it. Runs on the
/// locator's thread.
fn client(latest: Arc<Mutex<Option<Fix>>>) -> Result<gio::DBusProxy, String> {
    let manager = gio::DBusProxy::for_bus_sync(
        gio::BusType::System,
        gio::DBusProxyFlags::DO_NOT_LOAD_PROPERTIES,
        None,
        NAME,
        MANAGER,
        "org.freedesktop.GeoClue2.Manager",
        gio::Cancellable::NONE,
    )
    .map_err(|e| format!("no GeoClue: {e}"))?;
    let path = manager
        .call_sync(
            "GetClient",
            None,
            gio::DBusCallFlags::NONE,
            CALL_TIMEOUT_MS,
            gio::Cancellable::NONE,
        )
        .map_err(|e| format!("GeoClue gave no client: {e}"))?
        .child_value(0)
        .get::<glib::variant::ObjectPath>()
        .ok_or("GeoClue's client path")?;
    let client = gio::DBusProxy::for_bus_sync(
        gio::BusType::System,
        gio::DBusProxyFlags::NONE,
        None,
        NAME,
        path.as_str(),
        "org.freedesktop.GeoClue2.Client",
        gio::Cancellable::NONE,
    )
    .map_err(|e| format!("GeoClue client: {e}"))?;
    let set = |prop: &str, value: glib::Variant| {
        client
            .call_sync(
                "org.freedesktop.DBus.Properties.Set",
                Some(&("org.freedesktop.GeoClue2.Client", prop, value).to_variant()),
                gio::DBusCallFlags::NONE,
                CALL_TIMEOUT_MS,
                gio::Cancellable::NONE,
            )
            .map(|_| ())
            .map_err(|e| format!("GeoClue client {prop}: {e}"))
    };
    set("DesktopId", "omacam".to_variant())?;
    set("RequestedAccuracyLevel", ACCURACY_EXACT.to_variant())?;
    // Every new fix, however small the move: a photo should have the
    // newest one.
    let _ = set("DistanceThreshold", 0u32.to_variant());
    client.connect_local("g-signal", false, move |args| {
        let signal = args[2].get::<String>().ok()?;
        if signal != "LocationUpdated" {
            return None;
        }
        let params = args[3].get::<glib::Variant>().ok()?;
        let new = params.child_value(1).get::<glib::variant::ObjectPath>()?;
        if let Some(fix) = read_location(new.as_str()) {
            tracing::info!(accuracy_m = fix.accuracy.round(), "location fix");
            *latest.lock().unwrap() = Some(fix);
        }
        None
    });
    // Most often this fails because no agent runs for the user (the
    // Location switch is off).
    client
        .call_sync(
            "Start",
            None,
            gio::DBusCallFlags::NONE,
            CALL_TIMEOUT_MS,
            gio::Cancellable::NONE,
        )
        .map_err(|e| format!("GeoClue didn't start: {e}"))?;
    Ok(client)
}

impl Locator {
    /// Start asking GeoClue for fixes, on a thread of its own; returns at
    /// once. `failed` is called (on that thread) with the reason if
    /// GeoClue can't be had.
    pub fn start(failed: impl FnOnce(String) + Send + 'static) -> Locator {
        let latest = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (l, st) = (latest.clone(), stop.clone());
        let spawned = std::thread::Builder::new()
            .name("omacam-geoclue".into())
            .spawn(move || {
                let ctx = glib::MainContext::new();
                let _ = ctx.with_thread_default(|| {
                    let client = match client(l) {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::info!(error = %e, "no location");
                            failed(e);
                            return;
                        }
                    };
                    tracing::info!("location tags: GeoClue started");
                    let main_loop = glib::MainLoop::new(Some(&ctx), false);
                    let ml = main_loop.clone();
                    glib::timeout_source_new(
                        Duration::from_millis(250),
                        None,
                        glib::Priority::DEFAULT,
                        move || {
                            if st.load(Ordering::Relaxed) {
                                ml.quit();
                                glib::ControlFlow::Break
                            } else {
                                glib::ControlFlow::Continue
                            }
                        },
                    )
                    .attach(Some(&ctx));
                    main_loop.run();
                    let _ = client.call_sync(
                        "Stop",
                        None,
                        gio::DBusCallFlags::NONE,
                        1000,
                        gio::Cancellable::NONE,
                    );
                    tracing::info!("location tags: GeoClue stopped");
                });
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "no location thread");
        }
        Locator { latest, stop }
    }

    /// The newest fix, if recent.
    pub fn fix(&self) -> Option<Fix> {
        self.latest
            .lock()
            .unwrap()
            .and_then(|f| f.fresh(Instant::now()))
    }
}

impl Drop for Locator {
    fn drop(&mut self) {
        // The thread stops GeoClue's client and ends; nothing waits here.
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixes_need_real_coordinates_and_drop_unknown_altitudes() {
        let now = Instant::now();
        // The Royal Observatory, Greenwich: neutral test coordinates.
        let f = fix_from(51.4779, -0.0015, 46.0, 12.0, now).unwrap();
        assert_eq!(
            (f.latitude, f.longitude, f.altitude),
            (51.4779, -0.0015, Some(46.0))
        );
        let f = fix_from(0.0, 0.0, -f64::MAX, 30.0, now).unwrap();
        assert_eq!(f.altitude, None, "GeoClue's unknown altitude");
        assert!(fix_from(f64::NAN, 0.0, 0.0, 1.0, now).is_none());
        assert!(fix_from(0.0, 181.0, 0.0, 1.0, now).is_none());
    }

    #[test]
    fn old_fixes_are_not_used() {
        let then = Instant::now();
        let f = fix_from(0.0, 0.0, 0.0, 5.0, then).unwrap();
        assert!(f.fresh(then + Duration::from_secs(60)).is_some());
        assert!(f.fresh(then + MAX_AGE + Duration::from_secs(1)).is_none());
    }
}
