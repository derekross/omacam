//! A small EXIF writer: the tags a phone photo carries, as an APP1 segment
//! spliced into the JPEG after its SOI marker.
//!
//! The pixels are already turned upright, so Orientation is always 1. No
//! serial numbers or other device identifiers, and no GPS (location tags
//! aren't built yet and are off by default).

/// What goes into a photo's EXIF.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Info {
    pub make: Option<String>,
    pub model: Option<String>,
    pub software: String,
    /// Local time as "YYYY:MM:DD HH:MM:SS".
    pub datetime: String,
    /// The UTC offset as "+HH:MM".
    pub offset: Option<String>,
    pub width: u32,
    pub height: u32,
    pub exposure_us: Option<i32>,
    /// ISO, roughly: total gain x 100.
    pub iso: Option<u32>,
    pub f_number: Option<f64>,
    pub focal_length_35mm: Option<u32>,
    /// The digital zoom (crop) applied, 1 for none.
    pub digital_zoom: Option<f64>,
}

const ASCII: u16 = 2;
const SHORT: u16 = 3;
const LONG: u16 = 4;
const RATIONAL: u16 = 5;
const UNDEFINED: u16 = 7;

/// One IFD entry: tag, type, count and its bytes (little-endian).
struct Entry {
    tag: u16,
    kind: u16,
    count: u32,
    data: Vec<u8>,
}

fn ascii(tag: u16, s: &str) -> Entry {
    let mut data: Vec<u8> = s.bytes().filter(|b| b.is_ascii() && *b != 0).collect();
    data.push(0);
    Entry {
        tag,
        kind: ASCII,
        count: data.len() as u32,
        data,
    }
}

fn short(tag: u16, v: u16) -> Entry {
    Entry {
        tag,
        kind: SHORT,
        count: 1,
        data: v.to_le_bytes().to_vec(),
    }
}

fn long(tag: u16, v: u32) -> Entry {
    Entry {
        tag,
        kind: LONG,
        count: 1,
        data: v.to_le_bytes().to_vec(),
    }
}

fn rational(tag: u16, num: u32, den: u32) -> Entry {
    let mut data = num.to_le_bytes().to_vec();
    data.extend_from_slice(&den.to_le_bytes());
    Entry {
        tag,
        kind: RATIONAL,
        count: 1,
        data,
    }
}

/// A value as a rational with the given denominator.
fn ratio(tag: u16, v: f64, den: u32) -> Entry {
    rational(tag, (v * den as f64).round().max(0.0) as u32, den)
}

/// Write an IFD at `offset` (from the TIFF header) into `out`; `next` is
/// the offset of the next IFD (0 for none). Values over 4 bytes go after
/// the entry table. Returns nothing: `out` grows.
fn write_ifd(out: &mut Vec<u8>, mut entries: Vec<Entry>, next: u32) {
    entries.sort_by_key(|e| e.tag);
    let base = out.len() as u32;
    let table = 2 + entries.len() as u32 * 12 + 4;
    let mut extra: Vec<u8> = Vec::new();
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for e in &entries {
        out.extend_from_slice(&e.tag.to_le_bytes());
        out.extend_from_slice(&e.kind.to_le_bytes());
        out.extend_from_slice(&e.count.to_le_bytes());
        if e.data.len() <= 4 {
            let mut v = e.data.clone();
            v.resize(4, 0);
            out.extend_from_slice(&v);
        } else {
            let at = base + table + extra.len() as u32;
            out.extend_from_slice(&at.to_le_bytes());
            extra.extend_from_slice(&e.data);
            if extra.len() % 2 == 1 {
                extra.push(0);
            }
        }
    }
    out.extend_from_slice(&next.to_le_bytes());
    out.extend_from_slice(&extra);
}

/// The size an IFD takes when written.
fn ifd_len(entries: &[Entry]) -> u32 {
    let table = 2 + entries.len() as u32 * 12 + 4;
    let extra: u32 = entries
        .iter()
        .filter(|e| e.data.len() > 4)
        .map(|e| (e.data.len() as u32 + 1) & !1)
        .sum();
    table + extra
}

/// The TIFF structure (header, IFD0, Exif IFD) for `info`.
pub fn tiff(info: &Info) -> Vec<u8> {
    let mut ifd0 = vec![
        short(0x0112, 1),
        ascii(0x0131, &info.software),
        ascii(0x0132, &info.datetime),
    ];
    if let Some(m) = &info.make {
        ifd0.push(ascii(0x010f, m));
    }
    if let Some(m) = &info.model {
        ifd0.push(ascii(0x0110, m));
    }
    let mut exif = vec![
        Entry {
            tag: 0x9000,
            kind: UNDEFINED,
            count: 4,
            data: b"0232".to_vec(),
        },
        ascii(0x9003, &info.datetime),
        ascii(0x9004, &info.datetime),
        short(0xa001, 1), // sRGB
        long(0xa002, info.width),
        long(0xa003, info.height),
    ];
    if let Some(o) = &info.offset {
        exif.push(ascii(0x9010, o));
        exif.push(ascii(0x9011, o));
    }
    if let Some(t) = info.exposure_us.filter(|t| *t > 0) {
        // As cameras write it: 1/N for short exposures.
        if t < 500_000 {
            exif.push(rational(0x829a, 1, (1_000_000.0 / t as f64).round() as u32));
        } else {
            exif.push(rational(0x829a, t as u32, 1_000_000));
        }
    }
    if let Some(f) = info.f_number {
        exif.push(ratio(0x829d, f, 100));
    }
    if let Some(iso) = info.iso {
        exif.push(short(0x8827, iso.min(u16::MAX as u32) as u16));
    }
    if let Some(z) = info.digital_zoom {
        exif.push(ratio(0xa404, z, 100));
    }
    if let Some(f) = info.focal_length_35mm {
        exif.push(short(0xa405, f.min(u16::MAX as u32) as u16));
    }
    // Header (8), then IFD0 with a pointer to the Exif IFD after it.
    let ifd0_len = ifd_len(&ifd0) + 12; // + the pointer entry
    let exif_at = 8 + ifd0_len;
    ifd0.push(long(0x8769, exif_at));
    let mut out = b"II*\0".to_vec();
    out.extend_from_slice(&8u32.to_le_bytes());
    write_ifd(&mut out, ifd0, 0);
    debug_assert_eq!(out.len() as u32, exif_at);
    write_ifd(&mut out, exif, 0);
    out
}

/// `jpeg` with an EXIF APP1 segment for `info` right after its SOI.
pub fn insert(jpeg: &[u8], info: &Info) -> Vec<u8> {
    if jpeg.len() < 2 || jpeg[0] != 0xff || jpeg[1] != 0xd8 {
        return jpeg.to_vec();
    }
    let mut payload = b"Exif\0\0".to_vec();
    payload.extend_from_slice(&tiff(info));
    let len = payload.len() + 2;
    if len > u16::MAX as usize {
        return jpeg.to_vec();
    }
    let mut out = Vec::with_capacity(jpeg.len() + len + 2);
    out.extend_from_slice(&jpeg[..2]);
    out.extend_from_slice(&[0xff, 0xe1]);
    out.extend_from_slice(&(len as u16).to_be_bytes());
    out.extend_from_slice(&payload);
    out.extend_from_slice(&jpeg[2..]);
    out
}

/// Make and model from the device tree (`Google Pixel 7 Pro` becomes
/// "Google" and "Pixel 7 Pro") or DMI. No serial numbers.
pub fn device() -> (Option<String>, Option<String>) {
    let read = |p: &str| {
        std::fs::read_to_string(p)
            .ok()
            .map(|s| {
                s.trim_matches(|c: char| c == '\0' || c.is_whitespace())
                    .to_string()
            })
            .filter(|s| !s.is_empty())
    };
    if let Some(model) = read("/sys/firmware/devicetree/base/model") {
        return match model.split_once(' ') {
            Some((make, rest)) => (Some(make.into()), Some(rest.into())),
            None => (None, Some(model)),
        };
    }
    (
        read("/sys/class/dmi/id/sys_vendor"),
        read("/sys/class/dmi/id/product_name"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> Info {
        Info {
            make: Some("Google".into()),
            model: Some("Pixel 7 Pro".into()),
            software: "omacam 0.1.5".into(),
            datetime: "2026:10:14 12:34:56".into(),
            offset: Some("-04:00".into()),
            width: 3072,
            height: 4072,
            exposure_us: Some(33_214),
            iso: Some(1600),
            f_number: Some(1.85),
            focal_length_35mm: Some(25),
            digital_zoom: Some(2.0),
        }
    }

    fn jpeg() -> Vec<u8> {
        let img = crate::photo::Bgrx {
            width: 16,
            height: 16,
            data: vec![100; 16 * 16 * 4],
        };
        crate::photo::encode_jpeg(&img, 90).unwrap()
    }

    #[test]
    fn a_reader_finds_every_tag() {
        let out = insert(&jpeg(), &info());
        let exif = ::exif::Reader::new()
            .read_from_container(&mut std::io::Cursor::new(&out))
            .expect("EXIF parses");
        let get = |tag| {
            exif.get_field(tag, ::exif::In::PRIMARY)
                .unwrap_or_else(|| panic!("{tag} missing"))
                .display_value()
                .to_string()
        };
        use ::exif::Tag;
        assert_eq!(get(Tag::Make), "\"Google\"");
        assert_eq!(get(Tag::Model), "\"Pixel 7 Pro\"");
        assert_eq!(get(Tag::Orientation), "row 0 at top and column 0 at left");
        assert_eq!(get(Tag::DateTimeOriginal), "2026-10-14 12:34:56");
        assert_eq!(get(Tag::OffsetTimeOriginal), "\"-04:00\"");
        assert_eq!(get(Tag::ExposureTime), "1/30");
        assert_eq!(get(Tag::FNumber), "1.85");
        assert_eq!(get(Tag::PhotographicSensitivity), "1600");
        assert_eq!(get(Tag::FocalLengthIn35mmFilm), "25");
        assert_eq!(get(Tag::DigitalZoomRatio), "2");
        assert_eq!(get(Tag::PixelXDimension), "3072");
        assert_eq!(get(Tag::PixelYDimension), "4072");
        assert!(exif
            .get_field(Tag::GPSLatitude, ::exif::In::PRIMARY)
            .is_none());
    }

    #[test]
    fn the_jpeg_still_decodes() {
        let out = insert(&jpeg(), &info());
        let img = turbojpeg::decompress(&out, turbojpeg::PixelFormat::RGB).expect("decodes");
        assert_eq!((img.width, img.height), (16, 16));
    }

    #[test]
    fn missing_values_are_left_out() {
        let bare = Info {
            software: "omacam".into(),
            datetime: "2026:10:14 12:34:56".into(),
            width: 10,
            height: 20,
            ..Info::default()
        };
        let out = insert(&jpeg(), &bare);
        let exif = ::exif::Reader::new()
            .read_from_container(&mut std::io::Cursor::new(&out))
            .unwrap();
        assert!(exif
            .get_field(::exif::Tag::Make, ::exif::In::PRIMARY)
            .is_none());
        assert!(exif
            .get_field(::exif::Tag::FNumber, ::exif::In::PRIMARY)
            .is_none());
    }

    #[test]
    fn not_a_jpeg_is_returned_unchanged() {
        assert_eq!(insert(b"nope", &info()), b"nope");
    }
}
