//! Audio tag extraction via [`lofty`]. Pulls title/artist/album/genre/year/
//! track/duration into properties; concatenates tag values into `text` so
//! full-text search works across them.

use std::path::Path;

use lofty::{
    file::{AudioFile, TaggedFileExt},
    probe::Probe,
    tag::{Accessor, ItemKey},
};

use super::{ExtractError, ExtractedContent, Extractor};

pub struct AudioExtractor;

impl Extractor for AudioExtractor {
    fn supports(&self, mime: &str) -> bool {
        mime.starts_with("audio/")
    }

    fn extract(&self, path: &Path) -> Result<ExtractedContent, ExtractError> {
        let tagged = Probe::open(path)
            .map_err(|e| format!("lofty probe {}: {}", path.display(), e))?
            .read()
            .map_err(|e| format!("lofty read {}: {}", path.display(), e))?;

        let mut out = ExtractedContent::default();

        // Duration in seconds.
        let duration_secs = tagged.properties().duration().as_secs();
        if duration_secs > 0 {
            out.properties
                .insert("duration".to_string(), duration_secs.to_string());
        }

        if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
            for (key, item_key) in [
                ("title", ItemKey::TrackTitle),
                ("artist", ItemKey::TrackArtist),
                ("album", ItemKey::AlbumTitle),
                ("genre", ItemKey::Genre),
                ("year", ItemKey::Year),
                ("track", ItemKey::TrackNumber),
                ("comment", ItemKey::Comment),
            ] {
                if let Some(v) = tag.get_string(&item_key) {
                    if !v.is_empty() {
                        out.properties.insert(key.to_string(), v.to_string());
                    }
                }
            }
            // Accessor shortcuts for common fields if the ItemKey lookup missed.
            if !out.properties.contains_key("title") {
                if let Some(t) = tag.title() {
                    out.properties.insert("title".to_string(), t.to_string());
                }
            }
            if !out.properties.contains_key("artist") {
                if let Some(a) = tag.artist() {
                    out.properties.insert("artist".to_string(), a.to_string());
                }
            }
            if !out.properties.contains_key("album") {
                if let Some(a) = tag.album() {
                    out.properties.insert("album".to_string(), a.to_string());
                }
            }
        }

        // Join the searchable tag values into one blob so FTS hits on any of them.
        let mut pieces: Vec<&str> = Vec::new();
        for k in ["title", "artist", "album", "genre", "comment"] {
            if let Some(v) = out.properties.get(k) {
                pieces.push(v.as_str());
            }
        }
        out.text = pieces.join(" ");

        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_audio_mimes() {
        let e = AudioExtractor;
        assert!(e.supports("audio/mpeg"));
        assert!(e.supports("audio/flac"));
        assert!(!e.supports("video/mp4"));
        assert!(!e.supports("image/png"));
    }
}
