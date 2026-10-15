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
    /// Claim the accelerometer and keep `held` up to date, calling
    /// `changed` when the hold changes. A flat phone ("undefined") keeps
    /// the last known hold.
    pub fn claim(
        held: Rc<Cell<DeviceOrientation>>,
        changed: impl Fn(DeviceOrientation) + 'static,
    ) -> Option<Accelerometer> {
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
                    held.set(o);
                    changed(o);
                }
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

/// The gyroscope stream of the phone's sensor proxy (omamobile-sensors
/// 0.2.0 and later): `org.omamobile.SensorProxy.Gyro1` at
/// `/net/hadess/SensorProxy/Gyro` on the system bus, next to the
/// accelerometer. `ClaimGyroscope(u rate_hz) -> (h stream, u rate)` hands
/// over a SOCK_SEQPACKET socket whose packets hold whole records of
/// [`GYRO_RECORD`] bytes, little-endian: the sample time (u64 nanoseconds,
/// CLOCK_MONOTONIC, the clock of libcamera's SensorTimestamp on gs201),
/// the turn rates about x, y and z (f32 radians per second, Android's
/// device axes, counter-clockwise positive), and a sequence number (u32).
/// The claim ends with `ReleaseGyroscope()`, or when the socket closes.
/// No proxy, no gyroscope or an older proxy means no stabilization.
pub mod gyro {
    use std::io::Read;
    use std::sync::{Arc, Mutex};

    use super::{gio, NAME};
    use gtk4::gio::prelude::*;
    use gtk4::glib;
    use omacam_core::stabilize::{GyroSample, Stabilizer};

    pub const PATH: &str = "/net/hadess/SensorProxy/Gyro";
    pub const IFACE: &str = "org.omamobile.SensorProxy.Gyro1";
    /// Bytes per record: u64 timestamp, three f32 rates, u32 sequence.
    pub const GYRO_RECORD: usize = 24;
    /// The rate asked for: 240 Hz, several samples per frame even at
    /// 60 fps.
    pub const RATE_HZ: u32 = 240;

    /// One record from its bytes.
    pub fn parse(b: &[u8]) -> Option<GyroSample> {
        let b: &[u8; GYRO_RECORD] = b.get(..GYRO_RECORD)?.try_into().ok()?;
        let f = |i: usize| f32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        Some(GyroSample {
            t_ns: u64::from_le_bytes(b[0..8].try_into().unwrap()) as i64,
            rate: [f(8), f(12), f(16)],
        })
    }

    /// Every whole record in a packet.
    pub fn parse_packet(packet: &[u8]) -> impl Iterator<Item = GyroSample> + '_ {
        packet.chunks_exact(GYRO_RECORD).filter_map(parse)
    }

    fn proxy() -> Option<gio::DBusProxy> {
        gio::DBusProxy::for_bus_sync(
            gio::BusType::System,
            gio::DBusProxyFlags::NONE,
            None,
            NAME,
            PATH,
            IFACE,
            gio::Cancellable::NONE,
        )
        .ok()
    }

    fn has(proxy: &gio::DBusProxy) -> bool {
        let record_size = proxy
            .cached_property("RecordSize")
            .and_then(|v| v.get::<u32>())
            .unwrap_or(GYRO_RECORD as u32);
        let monotonic = proxy
            .cached_property("TimestampClock")
            .and_then(|v| v.get::<String>())
            .is_none_or(|c| c == "monotonic");
        proxy
            .cached_property("HasGyroscope")
            .and_then(|v| v.get::<bool>())
            .unwrap_or(false)
            && record_size as usize == GYRO_RECORD
            && monotonic
    }

    /// The phone has a gyroscope stream this client understands.
    pub fn available() -> bool {
        proxy().is_some_and(|p| has(&p))
    }

    /// A claimed gyroscope feeding a stabilizer. Released when dropped.
    pub struct Gyroscope {
        proxy: gio::DBusProxy,
    }

    impl Gyroscope {
        /// Claim the gyroscope and feed `stabilizer` from its records
        /// until released.
        pub fn claim(stabilizer: Arc<Mutex<Stabilizer>>) -> Option<Gyroscope> {
            let proxy = proxy()?;
            if !has(&proxy) {
                return None;
            }
            let (reply, fds) = proxy
                .call_with_unix_fd_list_sync(
                    "ClaimGyroscope",
                    Some(&(RATE_HZ,).to_variant()),
                    gio::DBusCallFlags::NONE,
                    5000,
                    None::<&gio::UnixFDList>,
                    gio::Cancellable::NONE,
                )
                .map_err(|e| tracing::warn!(error = %e, "gyroscope not claimed"))
                .ok()?;
            let index = reply.child_value(0).get::<glib::variant::Handle>()?.0;
            let rate = reply.child_value(1).get::<u32>().unwrap_or(0);
            let fd = fds?.get(index).ok()?;
            let mut socket = std::fs::File::from(fd);
            std::thread::Builder::new()
                .name("omacam-gyro".into())
                .spawn(move || {
                    // One packet per read: whole records.
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = socket.read(&mut buf) {
                        if n == 0 {
                            break; // released, or the proxy went away
                        }
                        let mut s = stabilizer.lock().unwrap();
                        for sample in parse_packet(&buf[..n]) {
                            s.push(sample);
                        }
                    }
                })
                .ok()?;
            tracing::info!(rate, "gyroscope claimed");
            Some(Gyroscope { proxy })
        }
    }

    impl Drop for Gyroscope {
        fn drop(&mut self) {
            let _ = self.proxy.call_sync(
                "ReleaseGyroscope",
                None,
                gio::DBusCallFlags::NONE,
                500,
                gio::Cancellable::NONE,
            );
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn record(t: u64, r: [f32; 3], seq: u32) -> Vec<u8> {
            let mut b = t.to_le_bytes().to_vec();
            for v in r {
                b.extend_from_slice(&v.to_le_bytes());
            }
            b.extend_from_slice(&seq.to_le_bytes());
            b
        }

        #[test]
        fn packets_parse_into_whole_records() {
            let mut p = record(123_456_789, [0.5, -1.25, 2.0], 7);
            p.extend(record(127_000_000, [0.0, 0.0, -0.004], 8));
            p.extend_from_slice(&[1, 2, 3]); // a torn tail is ignored
            let got: Vec<GyroSample> = parse_packet(&p).collect();
            assert_eq!(
                got,
                vec![
                    GyroSample {
                        t_ns: 123_456_789,
                        rate: [0.5, -1.25, 2.0]
                    },
                    GyroSample {
                        t_ns: 127_000_000,
                        rate: [0.0, 0.0, -0.004]
                    }
                ]
            );
        }
    }
}
