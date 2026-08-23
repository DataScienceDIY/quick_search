//! The audio corpus files: `.mp3` and `.flac`.
//! Audio "text" is the five tag values `extract::audio` concatenates, in
//! its emit order — order is load-bearing, so the ordered match really
//! checks it. `id3` and `metaflac` write the tags, `lofty` reads them; the
//! audio data is hand-rolled, since neither crate synthesises any.

use std::path::Path;

use super::{BodyFn, Charset, Lcg, Sample};

pub fn write_all(dir: &Path, lcg: &mut Lcg, body: &mut BodyFn<'_>, out: &mut Vec<Sample>) {
    mp3(dir, lcg, body, out);
    flac(dir, lcg, body, out);
}

/// The five tags in emit order; non-ASCII in two frames forces the writers
/// off ISO-8859-1 into a wide encoding.
fn fields(sentences: &[String]) -> [&str; 5] {
    [
        &sentences[0],
        &sentences[1],
        &sentences[2],
        &sentences[3],
        &sentences[4],
    ]
}

/// Four silent MPEG-1 Layer III frames (128 kbps, 44.1 kHz, 417 bytes each):
/// a probe confirms a sync word by checking where the next frame begins.
fn mpeg_frames() -> Vec<u8> {
    let mut out = Vec::with_capacity(4 * 417);
    for _ in 0..4 {
        out.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        out.resize(out.len() + 413, 0);
    }
    out
}

fn mp3(dir: &Path, lcg: &mut Lcg, body: &mut BodyFn<'_>, out: &mut Vec<Sample>) {
    use id3::{TagLike, Version};

    let b = body(lcg, Charset::Unicode);
    let [title, artist, album, genre, comment] = fields(&b.sentences);

    let path = super::write_file(dir, "track.mp3", &mpeg_frames());

    let mut tag = id3::Tag::new();
    tag.set_title(title);
    tag.set_artist(artist);
    tag.set_album(album);
    tag.set_genre(genre);
    tag.add_frame(id3::frame::Comment {
        lang: "eng".to_string(),
        description: String::new(),
        text: comment.to_string(),
    });
    tag.write_to_path(&path, Version::Id3v23)
        .expect("write id3 tag");

    out.push(Sample {
        path,
        label: "mp3",
        must_contain: fields(&b.sentences).iter().map(|s| s.to_string()).collect(),
        needle: b.needle.clone(),
        head_path: false,
    });
}

/// Fifty milliseconds of committed silence: a FLAC frame carries CRCs over
/// bit-packed subframes and `lofty` reads the first frame for stream
/// properties, so a metadata-only file is rejected. The *tags* are written
/// per run by `metaflac` — the fixture carries no text. Produced once with:
///
/// ```text
/// ffmpeg -f lavfi -i anullsrc=r=44100:cl=mono -t 0.05 \
///        -sample_fmt s16 -c:a flac -compression_level 12 silence.flac
/// ```
fn silence() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/silence.flac")
}

fn flac(dir: &Path, lcg: &mut Lcg, body: &mut BodyFn<'_>, out: &mut Vec<Sample>) {
    let b = body(lcg, Charset::Unicode);
    let [title, artist, album, genre, comment] = fields(&b.sentences);

    let path = dir.join("track.flac");
    std::fs::copy(silence(), &path).unwrap_or_else(|e| {
        panic!(
            "copy {} -> {}: {e}\n(committed fixture; see the doc comment on `silence`)",
            silence().display(),
            path.display()
        )
    });

    let mut tag = metaflac::Tag::read_from_path(&path).expect("read flac metadata");
    let comments = tag.vorbis_comments_mut();
    comments.set_title(vec![title]);
    comments.set_artist(vec![artist]);
    comments.set_album(vec![album]);
    comments.set_genre(vec![genre]);
    comments.set("COMMENT", vec![comment]);
    tag.save().expect("write flac tags");

    out.push(Sample {
        path,
        label: "flac",
        must_contain: fields(&b.sentences).iter().map(|s| s.to_string()).collect(),
        needle: b.needle.clone(),
        head_path: false,
    });
}
