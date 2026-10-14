//! A tiny test script for driving the app on a phone without touching it:
//! `--script "wait 3; zoom 5; wait 2; shot /tmp/tele.png; quit"`.
//!
//! Steps: `wait SECONDS`, `zoom Z` (as a preset button), `pinch Z` (as a
//! pinch that ends at Z), `flip`, `shutter`, `tap U V` (a tap on the
//! picture), `settings PAGE` and `close-settings`, `shot PATH` (the window
//! as a PNG, rendered by the app itself, so it works with the panel off),
//! and `quit`.

use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Wait(Duration),
    Zoom(f64),
    Pinch(f64),
    Flip,
    Shutter,
    Shot(PathBuf),
    /// Open the settings sheet on a page (photo, video, advanced).
    Settings(String),
    CloseSettings,
    /// A tap on the picture at (u, v), 0..1.
    Tap(f64, f64),
    Quit,
}

pub fn parse(text: &str) -> Result<Vec<Step>, String> {
    let mut steps = Vec::new();
    for part in text
        .split([';', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let (cmd, arg) = part
            .split_once(char::is_whitespace)
            .map(|(c, a)| (c, a.trim()))
            .unwrap_or((part, ""));
        let number = || {
            arg.parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v >= 0.0)
                .ok_or_else(|| format!("{cmd}: needs a number, got {arg:?}"))
        };
        steps.push(match cmd {
            "wait" => Step::Wait(Duration::from_secs_f64(number()?)),
            "zoom" => Step::Zoom(number()?),
            "pinch" => Step::Pinch(number()?),
            "flip" => Step::Flip,
            "shutter" => Step::Shutter,
            "shot" if !arg.is_empty() => Step::Shot(PathBuf::from(arg)),
            "shot" => return Err("shot: needs a path".into()),
            "settings" => Step::Settings(if arg.is_empty() {
                "photo".into()
            } else {
                arg.into()
            }),
            "close-settings" => Step::CloseSettings,
            "tap" => {
                let mut it = arg.split_whitespace().map(|v| v.parse::<f64>());
                match (it.next(), it.next()) {
                    (Some(Ok(u)), Some(Ok(v)))
                        if (0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v) =>
                    {
                        Step::Tap(u, v)
                    }
                    _ => return Err(format!("tap: needs two numbers in 0..1, got {arg:?}")),
                }
            }
            "quit" => Step::Quit,
            other => return Err(format!("unknown step {other:?}")),
        });
    }
    Ok(steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_script() {
        let s = parse("wait 1.5; zoom 5\npinch 0.6; flip; shutter; shot /tmp/a.png; quit").unwrap();
        assert_eq!(
            s,
            vec![
                Step::Wait(Duration::from_millis(1500)),
                Step::Zoom(5.0),
                Step::Pinch(0.6),
                Step::Flip,
                Step::Shutter,
                Step::Shot("/tmp/a.png".into()),
                Step::Quit,
            ]
        );
    }

    #[test]
    fn rejects_bad_steps() {
        assert!(parse("wait").is_err());
        assert!(parse("zoom x").is_err());
        assert!(parse("shot").is_err());
        assert!(parse("dance").is_err());
        assert!(parse("tap 2 0.5").is_err());
        assert_eq!(parse("tap 0.25 0.75").unwrap(), vec![Step::Tap(0.25, 0.75)]);
        assert_eq!(
            parse("settings video; close-settings").unwrap(),
            vec![Step::Settings("video".into()), Step::CloseSettings]
        );
        assert!(parse("wait -1").is_err());
        assert_eq!(parse(" ; ").unwrap(), vec![]);
    }
}
