//! Image metadata extraction via [`kamadak_exif`]. Reads EXIF tags (camera
//! make/model, date, GPS, dimensions) into properties. `text` is left empty
//! — this extractor does not OCR.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use exif::{In, Reader, Tag, Value};

use super::{ExtractError, ExtractedContent, Extractor};

pub struct ImageExtractor;

impl Extractor for ImageExtractor {
    fn supports(&self, mime: &str) -> bool {
        mime.starts_with("image/")
    }

    fn extract(&self, path: &Path) -> Result<ExtractedContent, ExtractError> {
        let file = File::open(path)
            .map_err(|e| format!("image open {}: {}", path.display(), e))?;
        let mut bufreader = BufReader::new(&file);
        let mut out = ExtractedContent::default();

        // Not every image has EXIF (e.g. PNGs usually don't). Treat "no EXIF"
        // as a successful no-op rather than a failure.
        let exif = match Reader::new().read_from_container(&mut bufreader) {
            Ok(exif) => exif,
            Err(_) => return Ok(out),
        };

        for (key, tag) in [
            ("make", Tag::Make),
            ("model", Tag::Model),
            ("date_taken", Tag::DateTimeOriginal),
            ("software", Tag::Software),
            ("orientation", Tag::Orientation),
            ("width", Tag::PixelXDimension),
            ("height", Tag::PixelYDimension),
            ("iso", Tag::PhotographicSensitivity),
            ("f_number", Tag::FNumber),
            ("exposure", Tag::ExposureTime),
            ("focal_length", Tag::FocalLength),
            ("gps_latitude", Tag::GPSLatitude),
            ("gps_longitude", Tag::GPSLongitude),
        ] {
            if let Some(field) = exif.get_field(tag, In::PRIMARY) {
                let s = field_to_string(&field.value);
                if !s.is_empty() {
                    out.properties.insert(key.to_string(), s);
                }
            }
        }

        Ok(out)
    }
}

fn field_to_string(value: &Value) -> String {
    match value {
        Value::Ascii(bytes_vec) => {
            let mut parts = Vec::new();
            for bytes in bytes_vec {
                let s: String = bytes
                    .iter()
                    .take_while(|&&b| b != 0)
                    .map(|&b| b as char)
                    .collect();
                if !s.is_empty() {
                    parts.push(s);
                }
            }
            parts.join(" ")
        }
        other => {
            // The EXIF crate's Display formatter handles integer/rational
            // types. Use it for any value we didn't handle explicitly.
            format!("{}", other.display_as(exif::Tag::Copyright))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_image_mimes() {
        let e = ImageExtractor;
        assert!(e.supports("image/jpeg"));
        assert!(e.supports("image/png"));
        assert!(!e.supports("video/mp4"));
        assert!(!e.supports("text/plain"));
    }
}
