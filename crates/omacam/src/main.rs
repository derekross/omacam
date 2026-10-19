//! omacam: a phone camera for Omarchy Mobile, shown as "Camera".

mod app;
mod exif;
mod journal;
mod location;
mod photo;
mod prefs;
mod pro;
mod script;
mod sensors;
mod settings;
mod theme;
mod viewfinder;

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use gtk4 as gtk;
use omacam_core::backend::{Backend, Event, EventSink};
use omacam_core::lens::{Facing, TableSpec};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Display {
    /// Copy each frame into memory (works around the panthor dmabuf
    /// export bug until the Mesa fix lands).
    Memory,
    /// Show the camera's own dmabufs, with no copy.
    Dmabuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CaptureMode {
    /// Reconfigure into the camera's full-resolution mode for each photo.
    Full,
    /// Keep a preview frame (fast, preview resolution).
    Quick,
    /// Night: a burst at full resolution, merged to cut the noise
    /// (omacam-merge).
    Night,
    /// HDR: a burst under the settled exposure, merged and tone mapped.
    Hdr,
}

#[derive(Debug, Clone, Parser)]
#[command(name = "omacam", version, about = "Camera for Omarchy Mobile")]
pub struct Cli {
    /// List the cameras and their roles, then exit (opens no stream).
    #[arg(long)]
    list: bool,
    /// Use synthetic frames instead of the cameras.
    #[arg(long)]
    fake: bool,
    /// How frames reach the screen.
    #[arg(long, value_enum, env = "OMACAM_DISPLAY", default_value = "memory")]
    display: Display,
    #[arg(long, value_enum, env = "OMACAM_CAPTURE", default_value = "full")]
    capture: CaptureMode,
    /// A lens table to use instead of the built-in one (also read from
    /// ~/.config/omacam/lenses.toml).
    #[arg(long)]
    lens_table: Option<PathBuf>,
    /// Pass the old camera's exposure to the new one at a lens switch.
    #[arg(long, env = "OMACAM_EXPOSURE_HINTS")]
    exposure_hints: bool,
    /// Show frame rate, lens and switch time over the viewfinder.
    #[arg(long, env = "OMACAM_DEBUG")]
    debug: bool,
    /// Run a test script (see `script.rs`), e.g.
    /// "wait 3; zoom 5; wait 2; shot /tmp/tele.png; quit".
    #[arg(long)]
    script: Option<String>,
    /// Save photos here instead of the Pictures folder's Camera directory.
    #[arg(long)]
    output_dir: Option<PathBuf>,
    /// Send a command to the running omacam and exit (`shutter`).
    #[arg(long, value_name = "COMMAND")]
    remote: Option<String>,
    /// Start on the front camera.
    #[arg(long)]
    front: bool,
    /// Start at this zoom.
    #[arg(long)]
    zoom: Option<f64>,
    /// With --fake: how bright the scene is (1 daylight indoors; 0.05 a
    /// dim room, where the telephoto gives way to the main camera).
    #[arg(long, default_value_t = 1.0)]
    fake_light: f64,
    /// With --fake: the frame rate every camera's mode reports (60 offers
    /// 60 fps video).
    #[arg(long)]
    fake_fps: Option<f64>,
}

impl Cli {
    pub fn lens_table(&self) -> TableSpec {
        let user = gtk::glib::user_config_dir().join("omacam/lenses.toml");
        let path = self
            .lens_table
            .clone()
            .or_else(|| user.exists().then_some(user));
        match path {
            Some(p) => match TableSpec::load(&p) {
                Ok(t) => {
                    tracing::info!(path = %p.display(), "lens table");
                    t
                }
                Err(e) => {
                    tracing::warn!(error = %e, "using the built-in lens table");
                    TableSpec::cheetah()
                }
            },
            None => TableSpec::cheetah(),
        }
    }

    pub fn start_facing(&self) -> Facing {
        if self.front {
            Facing::Front
        } else {
            Facing::Back
        }
    }
}

/// Start the backend the options ask for.
pub fn spawn_backend(cli: &Cli, sink: EventSink) -> anyhow::Result<Box<dyn Backend>> {
    if cli.fake {
        return Ok(Box::new(omacam_fake::FakeBackend::spawn(
            sink,
            omacam_fake::Options {
                scene_light: cli.fake_light,
                max_fps: cli.fake_fps,
                ..omacam_fake::Options::default()
            },
        )));
    }
    #[cfg(feature = "libcamera")]
    {
        let opts = omacam_libcamera::Options {
            display: match cli.display {
                Display::Memory => omacam_libcamera::DisplayMode::Memory,
                Display::Dmabuf => omacam_libcamera::DisplayMode::Dmabuf,
            },
            exposure_hints: cli.exposure_hints,
        };
        Ok(Box::new(omacam_libcamera::LibcameraBackend::spawn(
            sink, opts,
        )?))
    }
    #[cfg(not(feature = "libcamera"))]
    {
        anyhow::bail!("built without libcamera: run with --fake")
    }
}

fn list(cli: &Cli) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel();
    let _backend = spawn_backend(
        cli,
        Box::new(move |e| {
            let _ = tx.send(e);
        }),
    )?;
    let cameras = loop {
        match rx.recv_timeout(Duration::from_secs(10))? {
            Event::Cameras(c) => break c,
            Event::Error { message, .. } => eprintln!("{message}"),
            _ => {}
        }
    };
    let table = cli.lens_table().resolve(&cameras);
    for cam in &cameras {
        let lens = table.index_of(&cam.id).map(|i| table.get(i));
        match lens {
            Some(l) => println!(
                "{:<10} {:>5.2}x  rotation {:>3} (turned {:>3} clockwise)  {}  ({})",
                format!("{:?}", l.role).to_lowercase(),
                l.zoom,
                l.rotation,
                l.upright_clockwise(),
                cam.id,
                cam.model
            ),
            None => println!(
                "{:<10} {:>6}  rotation {:>3}  {}  ({})",
                "unused", "", cam.rotation, cam.id, cam.model
            ),
        }
    }
    Ok(())
}

/// Log to the terminal when there is one; otherwise (started from the app
/// launcher, where stderr goes nowhere) to the journal as `omacam`, and
/// have libcamera log to syslog (the journal) too, warnings and errors
/// only. `journalctl -t omacam` shows omacam's lines, each with its
/// fields (`journal.rs`); libcamera's arrive under its own identifier,
/// `journalctl -t libcamera` or `_PID=`.
fn init_logging() {
    use std::io::IsTerminal;
    let filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            "omacam=info,omacam_core=info,omacam_record=info,omacam_libcamera=info".into()
        })
    };
    let journal = if std::io::stderr().is_terminal() {
        None
    } else {
        journal::Journal::open("omacam")
    };
    match journal {
        Some(journal) => {
            // SAFETY: called first thing in main, before any thread starts.
            unsafe {
                if std::env::var_os("LIBCAMERA_LOG_FILE").is_none() {
                    std::env::set_var("LIBCAMERA_LOG_FILE", "syslog");
                }
                if std::env::var_os("LIBCAMERA_LOG_LEVELS").is_none() {
                    std::env::set_var("LIBCAMERA_LOG_LEVELS", "*:WARN");
                }
            }
            tracing_subscriber::fmt()
                .with_env_filter(filter())
                .with_writer(journal)
                .with_ansi(false)
                .without_time()
                .with_target(false)
                .init();
        }
        None => tracing_subscriber::fmt()
            .with_env_filter(filter())
            .with_writer(std::io::stderr)
            .init(),
    }
}

fn main() -> gtk::glib::ExitCode {
    init_logging();
    let cli = Cli::parse();
    if let Some(text) = &cli.script {
        if let Err(e) = script::parse(text) {
            eprintln!("omacam: --script: {e}");
            return gtk::glib::ExitCode::FAILURE;
        }
    }
    if let Some(cmd) = &cli.remote {
        use std::io::Write;
        let path = app::control_socket_path();
        return match std::os::unix::net::UnixStream::connect(&path)
            .and_then(|mut s| s.write_all(format!("{cmd}\n").as_bytes()))
        {
            Ok(()) => gtk::glib::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("omacam: {}: {e}", path.display());
                gtk::glib::ExitCode::FAILURE
            }
        };
    }
    if cli.list {
        return match list(&cli) {
            Ok(()) => gtk::glib::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("omacam: {e}");
                gtk::glib::ExitCode::FAILURE
            }
        };
    }
    app::run(cli)
}
