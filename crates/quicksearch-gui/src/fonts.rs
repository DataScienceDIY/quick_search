//! The app's fonts. egui is built without `default_fonts`, so we install its
//! two text faces ourselves, unmodified, and skip its emoji faces (736,668
//! bytes for the four glyphs we used).
//!
//! Scripts those two lack (CJK, Arabic, Hebrew, Thai, Devanagari, emoji and
//! symbols) come from the system's own fonts; none are bundled. A background
//! thread finds them, fixed files under `%WINDIR%\Fonts` on Windows and one
//! `fc-match` per script on Linux, and appends them behind the bundled pair.
//! They land a moment after the first frame, and a row already showing such
//! text repaints once. A script the system has no font for still renders as
//! `◻`. egui does no shaping and no right-to-left layout, so Arabic letters
//! show unjoined, Thai and Indic marks sit out of place, and Arabic and
//! Hebrew run left to right.
//!
//! A system face is memory-mapped, not read in, so only the pages its glyphs
//! touch cost memory. A font file truncated in place while mapped can crash
//! the app (SIGBUS); system font directories are not rewritten in place.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;

use ab_glyph::{Font as _, FontRef};
use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};

/// Proportional body text. Ubuntu Font Licence 1.0 (`assets/fonts/UFL.txt`).
const UBUNTU_LIGHT: &[u8] = include_bytes!("../assets/fonts/Ubuntu-Light.ttf");

/// Monospace. MIT/DejaVu/Bitstream Vera (`assets/fonts/Hack-Regular.txt`).
const HACK_REGULAR: &[u8] = include_bytes!("../assets/fonts/Hack-Regular.ttf");

/// The largest system font file mapped. Noto Sans CJK, the biggest face
/// wanted, is a 19.5 MB collection.
const MAX_FONT_BYTES: u64 = 32 << 20;

/// Installs the bundled faces at once and the system's fallback faces when a
/// background thread has found them, so the lookup (process spawns, file
/// reads) never delays the first frame.
pub fn install(ctx: &egui::Context) {
    install_bundled(ctx);
    // Without the thread the bundled faces stand alone, as they always have.
    let _ = spawn_fallback(ctx, || accept(candidates(), bundled(), MAX_FONT_BYTES));
}

/// Installs the two bundled faces on `ctx`. `set_fonts` only queues; applied
/// in `begin_pass`, so it takes effect on frame 1 if called before the first
/// run. Tests use this alone, so what they see does not vary by machine.
pub fn install_bundled(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::empty();

    fonts.font_data.insert(
        "Ubuntu-Light".to_owned(),
        Arc::new(egui::FontData::from_static(UBUNTU_LIGHT)),
    );
    fonts.font_data.insert(
        "Hack".to_owned(),
        Arc::new(egui::FontData::from_static(HACK_REGULAR)),
    );

    // Hack trails Ubuntu-Light in *both* families: it is the only remaining
    // source of `◻` (without it proportional text degrades to a bare `?`),
    // and it covers Greek, Cyrillic, arrows and box drawing that
    // Ubuntu-Light lacks, all of which turn up in filenames.
    fonts.families.insert(
        egui::FontFamily::Proportional,
        vec!["Ubuntu-Light".to_owned(), "Hack".to_owned()],
    );
    // Ubuntu-Light second, as epaint has it: "fallback for √ etc".
    fonts.families.insert(
        egui::FontFamily::Monospace,
        vec!["Hack".to_owned(), "Ubuntu-Light".to_owned()],
    );

    ctx.set_fonts(fonts);
}

/// Runs `lookup` on its own thread, appends the faces it returns behind the
/// bundled ones in both families, and asks for the repaint that shows them.
fn spawn_fallback(
    ctx: &egui::Context,
    lookup: impl FnOnce() -> Vec<(String, egui::FontData)> + Send + 'static,
) -> std::io::Result<JoinHandle<()>> {
    let ctx = ctx.clone();
    std::thread::Builder::new()
        .name("quicksearch-fonts".to_owned())
        .spawn(move || {
            let faces = lookup();
            if faces.is_empty() {
                return;
            }
            for (name, data) in faces {
                let families =
                    [egui::FontFamily::Proportional, egui::FontFamily::Monospace].map(|family| {
                        InsertFontFamily {
                            family,
                            priority: FontPriority::Lowest,
                        }
                    });
                ctx.add_font(FontInsert::new(&name, data, families.into()));
            }
            ctx.request_repaint();
        })
}

/// The bundled faces, parsed, for [`accept`]'s coverage check.
fn bundled() -> Vec<FontRef<'static>> {
    [UBUNTU_LIGHT, HACK_REGULAR]
        .into_iter()
        .filter_map(|bytes| FontRef::try_from_slice(bytes).ok())
        .collect()
}

/// A system face to try: its file, its index in the file (non-zero only in a
/// `.ttc` collection), and a character of the script it is wanted for.
#[derive(Debug, PartialEq)]
struct Candidate {
    path: PathBuf,
    index: u32,
    probe: char,
}

/// The candidates worth adding, in order, as named faces. epaint panics on a
/// face it cannot parse or whose units per em are out of range, so a face is
/// kept only if it passes both checks here. It must also draw its probe
/// character where no face before it (`drawn`, then each one kept) does:
/// fontconfig answers every query with something, and a collection's other
/// faces add nothing its first one lacks.
///
/// The maps of kept files are leaked. They last as long as the process
/// anyway, and epaint borrows static bytes but copies owned ones on every
/// font rebuild, which a zoom change is.
fn accept(
    candidates: Vec<Candidate>,
    mut drawn: Vec<FontRef<'static>>,
    cap: u64,
) -> Vec<(String, egui::FontData)> {
    // A collection that serves several scripts is read once.
    let mut kept: HashMap<PathBuf, &'static [u8]> = HashMap::new();
    let mut faces = Vec::new();
    for Candidate { path, index, probe } in candidates {
        if drawn.iter().any(|font| draws(font, probe)) {
            continue;
        }
        let bytes: &'static [u8] = match kept.get(&path) {
            Some(&bytes) => bytes,
            None => match read_font(&path, cap) {
                Some(map) if usable(&map[..], index, probe).is_some() => {
                    &Box::leak(Box::new(map))[..]
                }
                _ => continue,
            },
        };
        let Some(font) = usable(bytes, index, probe) else {
            continue;
        };
        let data = egui::FontData {
            index,
            ..egui::FontData::from_static(bytes)
        };
        faces.push((format!("{}#{index}", path.display()), data));
        kept.insert(path, bytes);
        drawn.push(font);
    }
    faces
}

/// The face at `index` in `bytes`, if epaint can load it and it draws `probe`.
fn usable(bytes: &[u8], index: u32, probe: char) -> Option<FontRef<'_>> {
    let font = FontRef::try_from_slice_and_index(bytes, index).ok()?;
    (font.units_per_em().is_some() && draws(&font, probe)).then_some(font)
}

/// Whether `font` has an outline for `c`. A colour bitmap face (Noto Color
/// Emoji) maps characters to glyphs with no outline, which egui cannot draw.
fn draws(font: &FontRef<'_>, c: char) -> bool {
    let glyph = font.glyph_id(c);
    glyph.0 != 0 && font.outline(glyph).is_some()
}

/// The file, mapped, if it is a regular file in a format epaint reads
/// (TrueType, OpenType, or a collection of either) of at most `cap` bytes.
fn read_font(path: &Path, cap: u64) -> Option<memmap2::Mmap> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if !matches!(ext.as_str(), "ttf" | "otf" | "ttc" | "otc") {
        return None;
    }
    let (file, meta) = quicksearch_core::platform::open_regular_file(path).ok()?;
    if meta.len() > cap {
        return None;
    }
    // Only the length the `fstat` vetted is mapped, should the file have
    // grown since.
    // SAFETY: the map is only ever read. A file truncated in place under it
    // faults on the next read of a lost page (module doc).
    unsafe {
        memmap2::MmapOptions::new()
            .len(meta.len() as usize)
            .map(&file)
    }
    .ok()
}

/// Where this platform's fallback faces may be, in fallback order. macOS has
/// neither source, so it keeps the bundled faces alone.
fn candidates() -> Vec<Candidate> {
    #[cfg(windows)]
    let found = windows_candidates(std::env::var_os("WINDIR").map(PathBuf::from));
    #[cfg(all(unix, not(target_os = "macos")))]
    let found = fontconfig::candidates("fc-match", std::time::Instant::now() + fontconfig::TIMEOUT);
    #[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
    let found = Vec::new();
    found
}

/// Windows' own faces for what the bundled pair lacks, in fallback order,
/// each with a character of its script. Each ships with Windows 10;
/// [`windows_candidates`] skips any a system lacks.
#[cfg(any(windows, test))]
const WINDOWS_FACES: [(&str, char); 8] = [
    ("msyh.ttc", '中'),     // Microsoft YaHei
    ("YuGothM.ttc", 'あ'),  // Yu Gothic Medium
    ("malgun.ttf", '한'),   // Malgun Gothic
    ("segoeui.ttf", 'ع'),   // Segoe UI: Arabic and Hebrew
    ("LeelawUI.ttf", 'ก'),  // Leelawadee UI: Thai
    ("Nirmala.ttf", 'अ'),   // Nirmala UI: Indic scripts
    ("seguiemj.ttf", '😀'), // Segoe UI Emoji, drawn in one colour
    ("seguisym.ttf", '♫'),  // Segoe UI Symbol
];

/// [`WINDOWS_FACES`] that exist in `windir`'s `Fonts` folder. An unset or
/// relative `%WINDIR%` gives none, rather than paths under the working
/// directory.
#[cfg(any(windows, test))]
fn windows_candidates(windir: Option<PathBuf>) -> Vec<Candidate> {
    let Some(fonts) = windir.filter(|w| w.is_absolute()).map(|w| w.join("Fonts")) else {
        return Vec::new();
    };
    WINDOWS_FACES
        .iter()
        .map(|&(file, probe)| Candidate {
            path: fonts.join(file),
            index: 0,
            probe,
        })
        .filter(|candidate| candidate.path.is_file())
        .collect()
}

/// The Linux lookup: fontconfig's best face per script.
#[cfg(all(unix, not(target_os = "macos")))]
mod fontconfig {
    use std::io::Read;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use super::Candidate;

    /// One `fc-match` pattern per script, with a character of that script.
    /// fc-match answers every pattern with its best face, whatever that
    /// covers (DejaVu Sans for a script nothing installed has), which
    /// [`super::accept`]'s probe check weeds out. `zh-cn` because a bare
    /// `zh` names no fontconfig orthography and gets the default face.
    const QUERIES: [(&str, char); 9] = [
        (":lang=zh-cn", '中'),
        (":lang=ja", 'あ'),
        (":lang=ko", '한'),
        (":lang=ar", 'ع'),
        (":lang=he", 'א'),
        (":lang=th", 'ก'),
        (":lang=hi", 'अ'),
        // A colour emoji face is a bitmap egui cannot draw: ask for another.
        (":charset=1f600:color=false", '😀'),
        (":charset=266b", '♫'),
    ];

    /// How long all the queries may take together. Each answers from
    /// fontconfig's cache in milliseconds; a cold cache or a hung font
    /// mount must not hold the thread for long.
    pub(super) const TIMEOUT: Duration = Duration::from_secs(10);

    /// How often a running query is checked on.
    const POLL: Duration = Duration::from_millis(5);

    /// What `program` (fc-match) answers for each of [`QUERIES`] before
    /// `deadline`. A missing program, a failed query or a late one gives no
    /// candidate.
    pub(super) fn candidates(program: &str, deadline: Instant) -> Vec<Candidate> {
        QUERIES
            .iter()
            .filter_map(|&(pattern, probe)| {
                let mut cmd = Command::new(program);
                cmd.args(["-f", "%{index}:%{file}", pattern]);
                let (path, index) = parse(&run_until(cmd, deadline)?)?;
                Some(Candidate { path, index, probe })
            })
            .collect()
    }

    /// `cmd`'s standard output, if it exits successfully before `deadline`.
    /// Past the deadline the child is killed and reaped.
    pub(super) fn run_until(mut cmd: Command, deadline: Instant) -> Option<Vec<u8>> {
        if Instant::now() >= deadline {
            return None;
        }
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    // A child that fills the pipe blocks and runs out the
                    // clock, so reading after the exit cannot hang.
                    let mut out = Vec::new();
                    child.stdout.take()?.read_to_end(&mut out).ok()?;
                    return status.success().then_some(out);
                }
                Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        }
    }

    /// fc-match's `%{index}:%{file}` answer as a path and a face index. The
    /// index's upper 16 bits pick a variable font's named instance, which
    /// egui cannot select, so only the face number is kept. The path is
    /// taken as bytes, since a file name need not be UTF-8.
    pub(super) fn parse(out: &[u8]) -> Option<(PathBuf, u32)> {
        use std::os::unix::ffi::OsStrExt;
        let colon = out.iter().position(|&b| b == b':')?;
        let index: u32 = std::str::from_utf8(&out[..colon]).ok()?.parse().ok()?;
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(&out[colon + 1..]));
        path.is_absolute().then_some((path, index & 0xFFFF))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quicksearch_core::testutil::{scratch_dir, touch};

    /// The coverage contract the UI depends on. `↻` is the load-bearing one
    /// for Proportional: U+21BB is in Hack and not Ubuntu-Light, so it
    /// passing proves Hack really is in the proportional fallback chain.
    /// `◻` itself cannot be asserted: `has_glyph` reports the replacement
    /// glyph as missing, epaint's own documented quirk.
    #[test]
    fn the_installed_faces_cover_what_the_ui_paints_and_no_more() {
        let ctx = crate::test_ui::ctx();
        // `Context::fonts` panics until the first pass has run.
        let _ = ctx.run(egui::RawInput::default(), |_| {});

        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            let id = egui::FontId::new(14.0, family.clone());
            ctx.fonts(|fonts| {
                for c in ['↻', '×', '·', '…', '•', '−', '°', 'é', 'Ω', 'д'] {
                    assert!(fonts.has_glyph(&id, c), "{family:?} lost {c:?}");
                }
                // If these start resolving, the 736,668 emoji bytes are back.
                for c in ['⟳', '🗙', '⚠', '🔥', '📋'] {
                    assert!(!fonts.has_glyph(&id, c), "{family:?} still has {c:?}");
                }
            });
        }
    }

    #[test]
    fn egui_bundles_no_fonts_of_its_own() {
        assert!(
            egui::FontDefinitions::builtin_font_names().is_empty(),
            "egui's `default_fonts` feature is back on: {:?}",
            egui::FontDefinitions::builtin_font_names()
        );
    }

    /// Both families' fallback chains as `ctx` last built them.
    fn chains(ctx: &egui::Context) -> [Vec<String>; 2] {
        ctx.fonts(|fonts| {
            let defs = fonts.lock().fonts.definitions().clone();
            [egui::FontFamily::Proportional, egui::FontFamily::Monospace]
                .map(|family| defs.families[&family].clone())
        })
    }

    /// The lookup runs while frames go on with the bundled faces, and what
    /// it finds lands behind Hack in both families with a repaint to show
    /// it. Were the lookup run inline, its 30 s wait would put the face in
    /// frame 1 and fail the first check rather than hang.
    #[test]
    fn the_fallback_lookup_never_holds_a_frame_and_lands_behind_hack() {
        let ctx = egui::Context::default();
        install_bundled(&ctx);
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let lookup = spawn_fallback(&ctx, move || {
            let _ = gate.recv_timeout(std::time::Duration::from_secs(30));
            vec![(
                "system".to_owned(),
                egui::FontData::from_static(HACK_REGULAR),
            )]
        })
        .expect("spawn the lookup");

        let _ = ctx.run(egui::RawInput::default(), |_| {});
        assert_eq!(
            chains(&ctx),
            [vec!["Ubuntu-Light", "Hack"], vec!["Hack", "Ubuntu-Light"]]
        );
        // egui asks for a few repaints of its own at start; outlast them, so
        // the one asked for next is the lookup's.
        for _ in 0..10 {
            if !ctx.has_requested_repaint() {
                break;
            }
            let _ = ctx.run(egui::RawInput::default(), |_| {});
        }
        assert!(!ctx.has_requested_repaint(), "egui never went idle");

        release.send(()).expect("the lookup is waiting");
        lookup.join().expect("the lookup thread");
        assert!(ctx.has_requested_repaint(), "the faces landed unseen");
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        assert_eq!(
            chains(&ctx),
            [
                vec!["Ubuntu-Light", "Hack", "system"],
                vec!["Hack", "Ubuntu-Light", "system"]
            ]
        );
    }

    fn candidate(path: &Path, index: u32, probe: char) -> Candidate {
        Candidate {
            path: path.to_owned(),
            index,
            probe,
        }
    }

    /// The faces [`accept`] keeps, as (file, index), with nothing drawn yet.
    fn kept(candidates: Vec<Candidate>, cap: u64) -> Vec<(PathBuf, u32)> {
        accept(candidates, Vec::new(), cap)
            .into_iter()
            .map(|(name, data)| {
                let (path, _) = name.rsplit_once('#').expect("name carries the index");
                (PathBuf::from(path), data.index)
            })
            .collect()
    }

    #[test]
    fn a_valid_face_is_kept_with_its_bytes_and_index() {
        let dir = scratch_dir("fonts-valid");
        let hack = dir.join("Hack.ttf");
        touch(&hack, HACK_REGULAR);
        let faces = accept(vec![candidate(&hack, 0, 'д')], Vec::new(), MAX_FONT_BYTES);
        assert_eq!(faces.len(), 1);
        let (name, data) = &faces[0];
        assert_eq!(name, &format!("{}#0", hack.display()));
        assert_eq!(data.index, 0);
        assert!(
            matches!(data.font, std::borrow::Cow::Borrowed(_)),
            "copied on every rebuild"
        );
        assert_eq!(&*data.font, HACK_REGULAR);
    }

    #[test]
    fn a_missing_file_is_skipped() {
        let gone = scratch_dir("fonts-missing").join("gone.ttf");
        assert!(kept(vec![candidate(&gone, 0, 'д')], MAX_FONT_BYTES).is_empty());
    }

    /// epaint would panic on each of these; `accept` must not.
    #[test]
    fn garbage_truncated_and_empty_fonts_are_skipped() {
        let dir = scratch_dir("fonts-garbage");
        let garbage: Vec<u8> = (0..4096u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
            .collect();
        // A real header and table directory, the tables themselves cut off.
        let truncated = &HACK_REGULAR[..1024];
        let mut candidates = Vec::new();
        for (name, body) in [
            ("garbage.ttf", &garbage[..]),
            ("truncated.ttf", truncated),
            ("empty.ttf", &[][..]),
        ] {
            touch(&dir.join(name), body);
            candidates.push(candidate(&dir.join(name), 0, 'д'));
        }
        // A directory with a font's name is no regular file.
        std::fs::create_dir(dir.join("folder.ttf")).expect("mkdir");
        candidates.push(candidate(&dir.join("folder.ttf"), 0, 'д'));
        assert!(kept(candidates, MAX_FONT_BYTES).is_empty());
    }

    /// Bitmap (`.pcf`) and Type 1 (`.pfb`) fonts are skipped by name, even
    /// holding TrueType bytes; the TrueType names pass in any case.
    #[test]
    fn only_truetype_and_opentype_names_are_read() {
        let dir = scratch_dir("fonts-ext");
        for name in ["a.pcf", "b.pfb", "c", "d.TTF", "e.otf", "f.ttc"] {
            touch(&dir.join(name), HACK_REGULAR);
        }
        let read = |name: &str| read_font(&dir.join(name), MAX_FONT_BYTES).is_some();
        assert!(!read("a.pcf") && !read("b.pfb") && !read("c"));
        assert!(read("d.TTF") && read("e.otf") && read("f.ttc"));
    }

    #[test]
    fn the_size_cap_admits_a_file_of_exactly_its_size() {
        let dir = scratch_dir("fonts-cap");
        let hack = dir.join("Hack.ttf");
        touch(&hack, HACK_REGULAR);
        let len = HACK_REGULAR.len() as u64;
        assert_eq!(read_font(&hack, len).as_deref(), Some(HACK_REGULAR));
        assert!(read_font(&hack, len - 1).is_none());
        assert!(kept(vec![candidate(&hack, 0, 'д')], len - 1).is_empty());
    }

    /// A face that does not draw its probe is fontconfig's fallback answer
    /// (DejaVu Sans for `:lang=zh-cn` on a system without CJK), and one
    /// whose probe an earlier face draws adds nothing.
    #[test]
    fn a_face_is_kept_only_for_a_probe_nothing_before_it_draws() {
        let dir = scratch_dir("fonts-probe");
        let (first, second) = (dir.join("first.ttf"), dir.join("second.ttf"));
        touch(&first, HACK_REGULAR);
        touch(&second, HACK_REGULAR);

        assert!(kept(vec![candidate(&first, 0, '中')], MAX_FONT_BYTES).is_empty());
        assert_eq!(
            kept(
                vec![candidate(&first, 0, 'д'), candidate(&second, 0, 'Ω')],
                MAX_FONT_BYTES
            ),
            vec![(first.clone(), 0)]
        );
        // The bundled faces count as drawn before any candidate.
        assert!(accept(vec![candidate(&first, 0, 'д')], bundled(), MAX_FONT_BYTES).is_empty());
    }

    /// A space maps to a glyph with nothing to draw, as every glyph of a
    /// colour bitmap face does for egui.
    #[test]
    fn drawing_needs_an_outline_not_just_a_glyph() {
        let hack = FontRef::try_from_slice(HACK_REGULAR).expect("Hack parses");
        assert!(draws(&hack, 'д'));
        assert_ne!(hack.glyph_id(' ').0, 0);
        assert!(!draws(&hack, ' '));
        assert!(!draws(&hack, '中'));
    }

    /// `ttf` as a two-face collection, both faces the same font.
    fn collection_of(ttf: &[u8]) -> Vec<u8> {
        const HEADER: u32 = 12 + 2 * 4;
        let mut out = b"ttcf".to_vec();
        for word in [0x0001_0000, 2, HEADER, HEADER] {
            out.extend_from_slice(&u32::to_be_bytes(word));
        }
        let mut font = ttf.to_vec();
        // Table offsets count from the start of the file, so each moves by
        // the header placed in front.
        let tables = u16::from_be_bytes([font[4], font[5]]) as usize;
        for at in (0..tables).map(|i| 12 + 16 * i + 8) {
            let offset = u32::from_be_bytes(font[at..at + 4].try_into().expect("4 bytes"));
            font[at..at + 4].copy_from_slice(&(offset + HEADER).to_be_bytes());
        }
        out.extend_from_slice(&font);
        out
    }

    #[test]
    fn a_collection_face_is_loaded_at_its_index() {
        let dir = scratch_dir("fonts-ttc");
        let ttc = dir.join("Pair.ttc");
        touch(&ttc, &collection_of(HACK_REGULAR));
        assert_eq!(
            kept(vec![candidate(&ttc, 1, 'д')], MAX_FONT_BYTES),
            vec![(ttc.clone(), 1)]
        );
        assert!(kept(vec![candidate(&ttc, 2, 'д')], MAX_FONT_BYTES).is_empty());
    }

    #[test]
    fn windows_faces_are_the_fixed_files_that_exist_under_windir() {
        let windir = scratch_dir("fonts-windir");
        let fonts = windir.join("Fonts");
        touch(&fonts.join("seguiemj.ttf"), b"");
        touch(&fonts.join("msyh.ttc"), b"");
        std::fs::create_dir_all(fonts.join("malgun.ttf")).expect("mkdir");
        assert_eq!(
            windows_candidates(Some(windir)),
            vec![
                candidate(&fonts.join("msyh.ttc"), 0, '中'),
                candidate(&fonts.join("seguiemj.ttf"), 0, '😀'),
            ]
        );
        assert!(windows_candidates(None).is_empty());
        assert!(windows_candidates(Some(PathBuf::from("Windows"))).is_empty());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn fc_match_answers_parse_to_a_path_and_a_face_index() {
        use std::os::unix::ffi::OsStrExt;
        let parse = fontconfig::parse;
        let at = |path: &str, index| Some((PathBuf::from(path), index));
        assert_eq!(
            parse(b"2:/usr/share/fonts/NotoSansCJK.ttc"),
            at("/usr/share/fonts/NotoSansCJK.ttc", 2)
        );
        assert_eq!(parse(b"0:/fonts/a:b.ttf"), at("/fonts/a:b.ttf", 0));
        // Named instance 1 of face 3.
        assert_eq!(parse(b"65539:/fonts/v.ttc"), at("/fonts/v.ttc", 3));
        let raw = parse(b"0:/fonts/\xff.ttf").expect("non-UTF-8 path");
        assert_eq!(raw.0.as_os_str().as_bytes(), b"/fonts/\xff.ttf");
        for bad in [
            &b""[..],
            b"/fonts/a.ttf",
            b"0:",
            b"0:fonts/a.ttf",
            b"x:/fonts/a.ttf",
            b"-1:/fonts/a.ttf",
            b"4294967296:/fonts/a.ttf",
        ] {
            assert_eq!(parse(bad), None, "{:?}", String::from_utf8_lossy(bad));
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn a_query_that_is_missing_fails_or_hangs_gives_nothing() {
        use std::process::Command;
        use std::time::{Duration, Instant};
        let soon = || Instant::now() + Duration::from_secs(10);
        let mut printf = Command::new("printf");
        printf.arg("0:/fonts/a.ttf");
        assert_eq!(
            fontconfig::run_until(printf, soon()).as_deref(),
            Some(&b"0:/fonts/a.ttf"[..])
        );
        assert_eq!(fontconfig::run_until(Command::new("false"), soon()), None);
        assert_eq!(
            fontconfig::run_until(Command::new("/nonexistent/fc-match"), soon()),
            None
        );
        assert!(fontconfig::candidates("/nonexistent/fc-match", soon()).is_empty());

        let started = Instant::now();
        let mut sleep = Command::new("sleep");
        sleep.arg("30");
        let deadline = Instant::now() + Duration::from_millis(200);
        assert_eq!(fontconfig::run_until(sleep, deadline), None);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "waited out the hang"
        );
        // Past the deadline nothing more starts, not even a query that
        // would answer.
        let mut printf = Command::new("printf");
        printf.arg("0:/fonts/a.ttf");
        assert_eq!(fontconfig::run_until(printf, Instant::now()), None);
    }
}
