//! Audio tag extraction via [`lofty`]. Concatenates the searchable tag
//! values — title, artist, album, genre, comment — into `text` so full-text
//! search works across them.

use std::path::Path;

use lofty::{
    file::TaggedFileExt,
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

        // properties (parked): year, track and duration went to the property
        // map alone and never reached `text`, so nothing collects them now.
        // See `super::ExtractedContent`.
        let mut pieces: Vec<String> = Vec::new();
        if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
            let mut push = |value: Option<String>| {
                if let Some(v) = value.filter(|v: &String| !v.is_empty()) {
                    pieces.push(v);
                }
            };
            // `ItemKey` first, falling back to the `Accessor` shortcut for the
            // three fields that have one — a tag can carry the value under
            // either.
            push(
                tag.get_string(&ItemKey::TrackTitle)
                    .filter(|v| !v.is_empty())
                    .map(str::to_string)
                    .or_else(|| tag.title().map(|t| t.to_string())),
            );
            push(
                tag.get_string(&ItemKey::TrackArtist)
                    .filter(|v| !v.is_empty())
                    .map(str::to_string)
                    .or_else(|| tag.artist().map(|a| a.to_string())),
            );
            push(
                tag.get_string(&ItemKey::AlbumTitle)
                    .filter(|v| !v.is_empty())
                    .map(str::to_string)
                    .or_else(|| tag.album().map(|a| a.to_string())),
            );
            push(tag.get_string(&ItemKey::Genre).map(str::to_string));
            push(tag.get_string(&ItemKey::Comment).map(str::to_string));
        }

        Ok(ExtractedContent::with_text(pieces.join(" ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal but real MPEG file: an ID3v2.3 tag carrying `frames`
    /// (`("TPE1", "…")` and friends), followed by one silent MPEG-1 Layer III
    /// frame so the probe recognizes the format from its content.
    fn write_mp3(tag: &str, frames: &[(&str, &str)]) -> std::path::PathBuf {
        let mut body = Vec::new();
        for (id, value) in frames {
            let mut payload = vec![0x00]; // ISO-8859-1
            payload.extend_from_slice(value.as_bytes());
            body.extend_from_slice(id.as_bytes());
            // ID3v2.3 frame sizes are plain big-endian, unlike the tag size.
            body.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            body.extend_from_slice(&[0, 0]); // flags
            body.extend_from_slice(&payload);
        }

        let mut out = Vec::new();
        out.extend_from_slice(b"ID3");
        out.extend_from_slice(&[0x03, 0x00, 0x00]); // v2.3, no flags
                                                    // Tag size is syncsafe: seven bits per byte.
        let n = body.len() as u32;
        out.extend_from_slice(&[
            ((n >> 21) & 0x7F) as u8,
            ((n >> 14) & 0x7F) as u8,
            ((n >> 7) & 0x7F) as u8,
            (n & 0x7F) as u8,
        ]);
        out.extend_from_slice(&body);

        // MPEG-1 Layer III, 128 kbps, 44.1 kHz, no padding: 417-byte frames.
        // Four of them, because the probe confirms a sync word by finding the
        // next frame where the first one says it will be.
        for _ in 0..4 {
            out.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
            out.resize(out.len() + 413, 0);
        }

        let path = crate::testutil::scratch_dir(tag).join("track.mp3");
        std::fs::write(&path, &out).expect("write fixture mp3");
        path
    }

    /// The searchable text is assembled from the tag values, which is the
    /// only reason audio files are full-text indexed at all. Pins the
    /// rewrite that dropped the property map this used to be built from.
    #[test]
    fn tag_values_become_searchable_text() {
        let path = write_mp3(
            "audio-tags",
            &[
                ("TIT2", "Blue Monday"),
                ("TPE1", "New Order"),
                ("TALB", "Power Corruption"),
                ("TCON", "Synthpop"),
            ],
        );
        let out = AudioExtractor.extract(&path).expect("extract");
        for expected in ["Blue Monday", "New Order", "Power Corruption", "Synthpop"] {
            assert!(
                out.text.contains(expected),
                "{:?} missing from {:?}",
                expected,
                out.text
            );
        }
        // Title first, then artist, album, genre — a stable order so the
        // stored text does not churn between runs.
        assert_eq!(out.text, "Blue Monday New Order Power Corruption Synthpop");
    }

    /// No tags at all is a successful extraction with nothing to store, not
    /// a failure — `set_content_done` writes no sidecar row for it.
    #[test]
    fn an_untagged_file_yields_empty_text() {
        let path = write_mp3("audio-untagged", &[]);
        let out = AudioExtractor.extract(&path).expect("extract");
        assert!(out.text.is_empty(), "unexpected text {:?}", out.text);
    }

    #[test]
    fn supports_audio_mimes() {
        let e = AudioExtractor;
        assert!(e.supports("audio/mpeg"));
        assert!(e.supports("audio/flac"));
        assert!(!e.supports("video/mp4"));
        assert!(!e.supports("image/png"));
    }
}
