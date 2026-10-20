//! Narrow metadata readers used by the planner.
//!
//! Reading metadata must never make a plan fail. Unsupported, unreadable, or
//! malformed files simply produce `None`; the plan can surface a row-specific
//! “missing metadata” error only when a rule requires the value.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

/// Returns Exif DateTimeOriginal or DateTimeDigitized for JPEG-like files.
/// The value is kept in its source form (`YYYY:MM:DD HH:MM:SS`) because date
/// formatting belongs in the rule layer, where it is visible in the preview.
pub fn jpeg_capture_datetime(path: &Path) -> Option<String> {
    let extension = path.extension()?.to_string_lossy().to_ascii_lowercase();
    if !matches!(extension.as_str(), "jpg" | "jpeg") {
        return None;
    }

    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let exif = exif::Reader::new().read_from_container(&mut reader).ok()?;

    [exif::Tag::DateTimeOriginal, exif::Tag::DateTimeDigitized]
        .into_iter()
        .find_map(|tag| {
            exif.get_field(tag, exif::In::PRIMARY)
                .map(|field| field.display_value().with_unit(&exif).to_string())
        })
}
