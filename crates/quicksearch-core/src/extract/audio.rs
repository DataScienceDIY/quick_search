//! Audio tag extraction via [`lofty`]: title, artist, album, genre, and
//! comment concatenate into the searchable `text`.
//!
//! The registry parses audio in a sandbox child once a binary enables one
//! ([`super::sandbox`]): lofty sizes a Vorbis comment list from the count
//! the file declares, past its own allocation limit, and a refused
//! allocation aborts the process.

use std::io::BufReader;
use std::path::Path;

use lofty::{
    config::ParseOptions,
    file::{FileType, TaggedFileExt},
    probe::Probe,
    tag::{Accessor, ItemKey},
};

use super::{ExtractError, Extractor, Scratch};

pub struct AudioExtractor;

impl Extractor for AudioExtractor {
    fn supports(&self, mime: &str) -> bool {
        mime.starts_with("audio/")
    }

    fn extract(
        &self,
        path: &Path,
        _mime: &str,
        out: &mut String,
        scratch: &mut Scratch,
    ) -> Result<(), ExtractError> {
        let (file, _) = super::open_for_extract(path, scratch.limits().read)?;
        // Tags only: the audio properties (duration, bitrate) cost a seek to
        // the end and, in an MP3 without a Xing header, a frame-sync search
        // back from it, and fail a file whose audio is cut short. Measured
        // (`extractprobe`, 18 generated 3-minute MP3s and 40 Ogg sounds):
        // `lseek` + `read` calls 2,567 → 790, cached throughput 1.4–2.3x.
        let probe =
            Probe::new(BufReader::new(file)).options(ParseOptions::new().read_properties(false));
        // The format from the extension when lofty knows it, as
        // `Probe::open` takes it; from the bytes otherwise, which is how the
        // sniff typed an extensionless or misnamed file (`track`,
        // `track.bin`) and the one way to read a name lofty lacks (`.oga`).
        let probe = match FileType::from_path(path) {
            Some(format) => probe.set_file_type(format),
            None => probe
                .guess_file_type()
                .map_err(|e| format!("lofty probe {}: {}", path.display(), e))?,
        };
        let tagged = probe
            .read()
            .map_err(|e| format!("lofty read {}: {}", path.display(), e))?;

        if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
            // The `Accessor` shortcuts hand back a `Cow`, so their fallbacks
            // are bound here rather than inside an `or_else` that would let
            // the temporary die before it is read.
            let (title, artist, album) = (tag.title(), tag.artist(), tag.album());
            // Space-separated, straight into the caller's buffer.
            let mut push = |value: Option<&str>| {
                if let Some(v) = value.filter(|v: &&str| !v.is_empty()) {
                    if !out.is_empty() {
                        out.push(' ');
                    }
                    out.push_str(v);
                }
            };
            // A tag can carry a value under `ItemKey` or the `Accessor` shortcut.
            for (key, fallback) in [
                (ItemKey::TrackTitle, &title),
                (ItemKey::TrackArtist, &artist),
                (ItemKey::AlbumTitle, &album),
            ] {
                push(
                    tag.get_string(&key)
                        .filter(|v| !v.is_empty())
                        .or(fallback.as_deref()),
                );
            }
            push(tag.get_string(&ItemKey::Genre));
            push(tag.get_string(&ItemKey::Comment));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(path: &std::path::Path) -> Result<String, ExtractError> {
        crate::extract::extract_text(&AudioExtractor, path)
    }

    /// An ID3v2.3 tag carrying `frames`, then silent MPEG frames so the probe
    /// recognizes the format from content, written as `name`.
    fn write_mp3(tag: &str, name: &str, frames: &[(&str, &str)]) -> std::path::PathBuf {
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

        // MPEG-1 Layer III, 128 kbps, 44.1 kHz, no padding: 417-byte frames;
        // the probe confirms sync by finding the next frame where the first says.
        for _ in 0..4 {
            out.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
            out.resize(out.len() + 413, 0);
        }

        let path = crate::testutil::scratch_dir(tag).join(name);
        std::fs::write(&path, &out).expect("write fixture mp3");
        path
    }

    const BLUE_MONDAY: &[(&str, &str)] = &[
        ("TIT2", "Blue Monday"),
        ("TPE1", "New Order"),
        ("TALB", "Power Corruption"),
        ("TCON", "Synthpop"),
    ];

    #[test]
    fn tag_values_become_searchable_text() {
        let path = write_mp3("audio-tags", "track.mp3", BLUE_MONDAY);
        let out = extract(&path).expect("extract");
        assert_eq!(out, "Blue Monday New Order Power Corruption Synthpop");
    }

    /// The sniff types these from their bytes, and so must lofty: the name
    /// says nothing it knows.
    #[test]
    fn an_mp3_named_without_its_extension_is_typed_from_its_bytes() {
        for name in ["track", "track.bin"] {
            let path = write_mp3(&format!("audio-named-{name}"), name, BLUE_MONDAY);
            let head = std::fs::read(&path).unwrap();
            let mime = crate::mime::guess_mime_from_head(&path, &head);
            assert_eq!(mime, Some("audio/mpeg"), "{name}");
            let out = extract(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(out, "Blue Monday New Order Power Corruption Synthpop");
        }
    }

    /// A file the sniff calls audio in a format lofty does not read (MIDI),
    /// named nothing lofty knows: no format from the name or the bytes.
    #[test]
    fn a_file_lofty_cannot_type_is_an_error() {
        let path = crate::testutil::scratch_dir("audio-midi").join("song");
        // A header chunk, then one track holding only its end-of-track event.
        std::fs::write(
            &path,
            b"MThd\0\0\0\x06\0\0\0\x01\0\x60MTrk\0\0\0\x04\0\xff\x2f\0",
        )
        .unwrap();
        let head = std::fs::read(&path).unwrap();
        let mime = crate::mime::guess_mime_from_head(&path, &head).unwrap();
        assert!(mime.starts_with("audio/"), "{mime}");
        let err = extract(&path).expect_err("no format to read");
        assert!(err.contains("lofty read"), "{err}");
    }

    /// A download cut short keeps its tag: the audio after it is never read.
    #[test]
    fn a_truncated_stream_still_yields_its_tags() {
        let path = write_mp3("audio-truncated", "track.mp3", &[("TIT2", "Half Written")]);
        let bytes = std::fs::read(&path).unwrap();
        // The tag, the first frame's header and a few bytes of its body.
        let tag_end = bytes.len() - 4 * 417;
        std::fs::write(&path, &bytes[..tag_end + 10]).unwrap();
        assert_eq!(extract(&path).expect("extract"), "Half Written");
    }

    #[test]
    fn an_untagged_file_yields_empty_text() {
        let path = write_mp3("audio-untagged", "track.mp3", &[]);
        let out = extract(&path).expect("extract");
        assert!(out.is_empty(), "unexpected text {:?}", out);
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
