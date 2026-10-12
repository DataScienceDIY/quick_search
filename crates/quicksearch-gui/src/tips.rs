//! Plain-language tooltips: every configuration control, and any status line
//! whose wait needs explaining, explains itself on hover.

const TIP_WIDTH: f32 = 420.0;

pub struct Tip {
    pub title: &'static str,
    pub body: &'static str,
    /// Concrete values and when to choose them, rendered small underneath.
    pub examples: &'static [&'static str],
    /// Rendered small and orange; reserved for rebuilds and deletions.
    pub caution: Option<&'static str>,
}

impl Tip {
    pub fn show(&self, ui: &mut egui::Ui) {
        ui.set_max_width(TIP_WIDTH);
        ui.strong(self.title);
        ui.label(self.body);
        if !self.examples.is_empty() {
            ui.add_space(4.0);
            let single = self.examples.len() == 1;
            for example in self.examples {
                let line = if single {
                    format!("Example: {}", example)
                } else {
                    format!("•  {}", example)
                };
                ui.label(egui::RichText::new(line).small());
            }
        }
        if let Some(caution) = self.caution {
            ui.add_space(4.0);
            let caution_color = crate::color::palette(ui.visuals().dark_mode).orange;
            ui.label(crate::ui_util::hint_colored(caution, caution_color));
        }
    }
}

pub trait Tipped {
    /// Shown even while disabled: a greyed-out button is exactly when
    /// someone wants to know what it would do.
    fn tip(self, tip: &'static Tip) -> Self;
}

impl Tipped for egui::Response {
    fn tip(self, tip: &'static Tip) -> egui::Response {
        self.on_hover_ui(|ui| tip.show(ui))
            .on_disabled_hover_ui(|ui| tip.show(ui))
    }
}

/// One grid row; the label and the control share the tooltip.
pub fn tip_row(
    ui: &mut egui::Ui,
    label: impl Into<egui::WidgetText>,
    tip: &'static Tip,
    widget: impl FnOnce(&mut egui::Ui) -> egui::Response,
) {
    ui.label(label).tip(tip);
    widget(ui).tip(tip);
    ui.end_row();
}

// --- Settings: the advanced toggle ----------------------------------------

pub static SHOW_ADVANCED: Tip = Tip {
    title: "Show advanced settings",
    body: "Shows the rest of the Settings tab: the index location, how text \
           is split for searching, how much of each file is read, search \
           memory, and more.\n\n\
           They are hidden because the defaults suit almost every \
           installation and a wrong value can slow indexing, worsen searching \
           or force a rebuild. Hidden settings keep working at their defaults.",
    examples: &[
        "on to move the index to another drive, or to tune a very large collection.",
        "off for everyday use.",
    ],
    caution: None,
};

// --- Settings: Paths ------------------------------------------------------

pub static DATABASE_PATH: Tip = Tip {
    title: "Database file",
    body: "The single file holding the index: the names, locations and text \
           of everything indexed. It grows with your files, so put it on a \
           drive with room to spare.\n\n\
           Changing this switches to the index at the new location, building \
           one if none exists; the old file stays until you delete it. Keep \
           it out of cloud-synced folders such as OneDrive or Dropbox, which \
           cannot keep up with it.",
    examples: &["a path on a second, larger drive when the system drive is short of space."],
    caution: None,
};

// --- Settings: Indexing ---------------------------------------------------

pub static REINDEX_INTERVAL: Tip = Tip {
    title: "Full reindex every",
    body: "Indexed folders are watched, so most changes appear within \
           seconds. This full sweep is the safety net: it picks up what \
           watching missed, such as changes made while QuickSearch was closed \
           or on a drive that does not report them.\n\n\
           A sweep costs a few minutes of disk activity; searching keeps \
           working throughout.",
    examples: &[
        "1440, once a day, suits an ordinary laptop.",
        "60 on a network share, or a drive other machines write to.",
        "10080, once a week, for an archive that barely changes.",
    ],
    caution: None,
};

pub static INCLUDE_HIDDEN: Tip = Tip {
    title: "Include hidden files",
    body: "Files and folders the system keeps out of sight: names starting \
           with a dot on Linux and macOS, and anything with the Hidden \
           attribute on Windows, such as AppData and $RECYCLE.BIN. They are \
           mostly program settings and caches, so leaving them out keeps the \
           index smaller and the results cleaner.\n\n\
           Folders marked System but not Hidden are always indexed: cloud \
           sync folders and folders with a custom icon carry that mark only \
           for the icon.",
    examples: &["on to find configuration files such as .bashrc or .gitconfig."],
    caution: None,
};

// --- Settings: Processing -------------------------------------------------

pub static HASH_LENGTH: Tip = Tip {
    title: "Hash sample size",
    body: "How many bytes from the start of each file QuickSearch reads to \
           recognise it. They make the fingerprint the Duplicates tab groups \
           files by, tell what kind of file it is, and for a small text file \
           are the whole text.\n\n\
           Reading more is more reliable but slower, especially over a \
           network.",
    examples: &[
        "8192, the default, is right for almost everyone.",
        "higher when the Duplicates tab groups files that are not really identical, \
         as happens with disk images and other formats that start with a lot of \
         empty space.",
    ],
    caution: Some("Changing this rebuilds the index from scratch."),
};

pub static MAX_STORED_TEXT: Tip = Tip {
    title: "Max stored text",
    body: "The most text QuickSearch keeps from any one file. Text past this \
           point is not stored, so a word that appears only deep inside a \
           very long document is not found. Names, sizes and dates are \
           unaffected.\n\n\
           With the two settings below it, this has the most effect on how \
           big the index grows.",
    examples: &[
        "262144, 256 KiB, covers the whole of most documents.",
        "65536, 64 KiB, to shrink the index when you mostly search the opening pages.",
        "higher for long books, transcripts or logs you expect to search to the end.",
    ],
    caution: None,
};

pub static MAX_TEXT_FILE_SIZE: Tip = Tip {
    title: "Max text file size",
    body: "Files larger than this are indexed without their text, so one \
           huge file cannot hold up an indexing run. They are still found by \
           name, size or date.",
    examples: &[
        "52428800, 50 MiB, the default, reads inside large logs, scanned PDFs and \
         presentations that are mostly pictures.",
        "2097152, 2 MiB, to read ordinary documents only and leave anything larger \
         to be found by name.",
    ],
    caution: None,
};

pub static BATCH_SIZE: Tip = Tip {
    title: "Batch size",
    body: "How many files go into each write to the index while indexing. \
           Larger batches mean fewer, bigger writes: a little faster, and \
           more memory.\n\n\
           It changes only speed, never what you can find.",
    examples: &[
        "500, the default, balances speed against memory.",
        "about 50 on a machine with very little memory to spare.",
    ],
    caution: None,
};

pub static MAX_WAL_SIZE: Tip = Tip {
    title: "Max WAL size",
    body: "Indexing writes changes to a companion file (the WAL) and later \
           folds them into the index. During a long run with searches going \
           on, that file can outgrow the index itself. At this size \
           QuickSearch pauses indexing and folds it in.\n\n\
           A larger value makes indexing faster and searching during it \
           slower.",
    examples: &[
        "2147483648, 2 GiB, the default.",
        "67108864, 64 MiB, to favour searching while indexing runs.",
        "0 to leave it to the database. Other values under 16 MiB count as 16 MiB.",
    ],
    caution: None,
};

pub static COMPACT_INDEX: Tip = Tip {
    title: "Compact text index",
    body: "Builds the index of your files' text without recording where in \
           each file every word sits, which makes it several times smaller \
           and quicker to build.\n\n\
           Searches find exactly the same files either way, this simply trades \
           reduced index size and faster indexing speed for slower searches.",
    examples: &[
        "on, the default: 1.1 GiB of text documents went from a 1.3 GiB index to 219 MiB, built in a third less time.",
        "off when disk space is plentiful and you often search for phrases of common words.",
    ],
    caution: Some("Changing this rebuilds the index from scratch."),
};

// --- Settings: Search -----------------------------------------------------

pub static FUZZY_DEFAULT: Tip = Tip {
    title: "Fuzzy search ON by default",
    body: "Whether the Fuzzy box on the Search tab starts ticked when \
           QuickSearch opens. Fuzzy search also finds matches with typos, at \
           some cost in speed. You can tick or untick it at any time.",
    examples: &["on when you often look for names you are not sure how to spell."],
    caution: None,
};

pub static FUZZY_EDITS: Tip = Tip {
    title: "Fuzzy edit distance",
    body: "How far a word may be from what you typed and still match while \
           Fuzzy is on. One edit is one letter added, removed or changed, so \
           \"repot\" is one edit from \"report\".\n\n\
           The allowance grows with what you type, one edit per three \
           letters (bytes, for non-ASCII text), up to this value. Short \
           searches still match loosely: letters may drop off either end, so \
           \"report\" also matches \"port\", and a three-letter search \
           matches any two of its letters.\n\n\
           At the default of 2, fuzzy content search reads every stored \
           document for searches under 9 letters, and each edit above 2 adds \
           three letters to that.",
    examples: &[
        "2, the default: one edit for words of three to five letters, two for longer ones.",
        "0 turns typo matching off, even with the Fuzzy box ticked.",
        "3 up to the maximum of 8 is allowed, but searches slow down and pull in many \
         unrelated files.",
    ],
    caution: None,
};

pub static DISPLAY_LIMIT: Tip = Tip {
    title: "Display limit",
    body: "The most results one search gathers and shows. A search matching \
           more stops here, which keeps the list quick to scroll and cheap to \
           hold.\n\n\
           The best matches come first, so a lower limit rarely hides what \
           you are looking for.",
    examples: &[
        "1000, the default, is more than anyone scrolls through.",
        "higher to list every file of a kind, such as type:Image, all at once.",
    ],
    caution: None,
};

pub static SEARCH_CACHE: Tip = Tip {
    title: "Search cache",
    body: "Memory, in MiB, that searching keeps index pages in. 0 sizes it \
           for your index automatically, up to 128 MiB; the value it picks is \
           shown below.\n\n\
           It matters on an encrypted index, which must decrypt every page the \
           cache misses: a cache smaller than the one shown can make searches \
           about four times slower. An unencrypted index gets a small fixed \
           cache, because a larger one measures no faster. The memory is \
           released after a long idle.",
    examples: &[
        "0, the default, is right unless your folders are nested unusually deep.",
        "a fixed value to cap the memory QuickSearch holds, at the cost of slower \
         searches on a large index.",
    ],
    caution: None,
};

pub static RESULTS_PER_PAGE: Tip = Tip {
    title: "Stream batch size",
    body: "How many results arrive in each batch while a search runs. Smaller \
           batches show the first results sooner and update the list more \
           often; larger ones do less work in total.\n\n\
           This is not a page size: scrolling does not fetch more.",
    examples: &[
        "100, the default, feels immediate on most machines.",
        "25 when the first results are slow to appear on a large index.",
    ],
    caution: None,
};

pub static DEBOUNCE: Tip = Tip {
    title: "Debounce",
    body: "How long QuickSearch waits after your last keystroke before \
           searching, so typing a word does not start a search for every \
           letter. In milliseconds: 1000 is one second.",
    examples: &[
        "150, the default, keeps up with ordinary typing.",
        "0 to search on every keystroke on a fast machine with a modest index.",
        "300 or more when results flicker or stutter as you type.",
    ],
    caution: None,
};

pub static LIVE_RESULTS: Tip = Tip {
    title: "Live results",
    body: "Keeps the rows on screen current as their files change. A rename, \
           edit or deletion made elsewhere shows here, read from the file \
           itself rather than from the index. Each row is also checked \
           against the disk as it scrolls into view, which corrects a stale \
           index; on a network share that check is all you get.\n\n\
           Only visible rows are watched, and only until you change the \
           search. Nothing is added, removed or re-sorted while you read.",
    examples: &[
        "off if a stopped index must mean nothing is written to it, since the files \
         on screen are otherwise updated in the index even while indexing is stopped.",
    ],
    caution: None,
};

pub static COLUMNS: Tip = Tip {
    title: "Search columns",
    body: "Which columns the results table shows. The path is always shown, \
           because it is the only column that identifies a result on its \
           own.\n\n\
           Size and Modified start off: their width is usually better spent \
           on the path and the matched text. Turning a column on also lets \
           you sort by it. Right-clicking a column header on the Search tab \
           does the same.",
    examples: &[],
    caution: None,
};

// --- Settings: Interface --------------------------------------------------

pub static UI_SCALE: Tip = Tip {
    title: "UI scale",
    body: "Zooms the whole window: text, spacing and controls together. 1.00 \
           is the ordinary size for your screen.\n\n\
           Ctrl with + or - changes it for now without saving, and Ctrl 0 \
           puts it back. This slider sets the size QuickSearch starts at.",
    examples: &[
        "1.40 or more on a high-resolution screen where text looks small.",
        "0.80 to fit more results on screen.",
    ],
    caution: None,
};

pub static SEARCH_HOTKEY: Tip = Tip {
    title: "Search shortcut",
    body: "Brings QuickSearch to the front from anywhere, with the previous \
           search selected so you can start typing.\n\n\
           Click the button, then press a key together with any of Ctrl, Alt \
           and Shift. Clear turns the shortcut off.\n\n\
           While running, QuickSearch holds the key itself on Windows and \
           X11. For the key to also start QuickSearch, your desktop must bind \
           it; the set-up button below does that where it can. On Wayland the \
           desktop binding is the only option.",
    examples: &["Ctrl+Shift+F, the default, which few other programs use."],
    caution: None,
};

pub static COLOR_SCHEME: Tip = Tip {
    title: "Color scheme",
    body: "Dark or light. Takes effect when you apply it, with no \
           restart.\n\n\
           QuickSearch does not follow your desktop's theme: on Linux that \
           would mean listening to your session's settings over the message \
           bus, which a search tool has no business doing.",
    examples: &["Light for a bright room, or to match a light desktop."],
    caution: None,
};

// --- Settings: Security ---------------------------------------------------

pub static ENABLE_PASSWORD: Tip = Tip {
    title: "Enable password protection",
    body: "Encrypts the index with a password you choose. The index holds the \
           names and text of your files, so without encryption anyone who can \
           read it can read those.\n\n\
           QuickSearch then asks for the password at every start, unless you \
           let it remember.",
    examples: &[],
    caution: Some(
        "Turning protection on deletes and rebuilds the index. Your files are not touched. \
         The unencrypted index is not securely erased; parts can remain recoverable in \
         freed disk space.",
    ),
};

pub static CHANGE_PASSWORD: Tip = Tip {
    title: "Change password",
    body: "Replaces the index password. You are asked for a new one, and the \
           index is encrypted again with it.",
    examples: &[],
    caution: Some(
        "Changing the password deletes and rebuilds the index. Your files are not touched. \
         The old index is not securely erased; recoverable parts of it still open with the \
         old password.",
    ),
};

pub static DISABLE_PASSWORD: Tip = Tip {
    title: "Disable protection",
    body: "Removes the password and stores the index unencrypted. Anyone who \
           can read that file can then see your file names and the text \
           inside them.",
    examples: &[],
    caution: Some(
        "Turning protection off deletes and rebuilds the index. Your files are not touched.",
    ),
};

pub static SHOW_KEY: Tip = Tip {
    title: "Show database key",
    body: "Shows the raw SQLCipher key the index is encrypted with, after you \
           confirm your password. Tools such as DB Browser for SQLCipher \
           accept it in the 0x form shown and can then open the index \
           file.\n\n\
           The key is derived from your password and the salt in the config \
           file, so it stays the same until the password changes. Anyone \
           holding it can read the index without the password, so guard a \
           copy as you would the password itself. Changing the password \
           does not retire it: copies of the index made before the change \
           still open with it.",
    examples: &[],
    caution: None,
};

pub static REMEMBER_KEYCHAIN: Tip = Tip {
    title: "Remember on this device",
    body: "Stores the index key in your system's password store, such as \
           GNOME Keyring, KWallet or Windows Credential Manager, so \
           QuickSearch can unlock the index without asking at startup.\n\n\
           The password itself is never stored, only a key derived from it, \
           and only on this machine. Off, you type the password each time \
           QuickSearch starts.",
    examples: &[],
    caution: None,
};

// --- Manage Index tab: indexing controls ---------------------------------

pub static START_NOW: Tip = Tip {
    title: "Start indexing now",
    body: "Runs a full pass over your indexed folders now instead of at the \
           next scheduled time. Worth doing after adding a folder or changing \
           a filter, or when the computer has been off for a while.\n\n\
           Searching keeps working meanwhile. Unavailable while a run is \
           under way.",
    examples: &[],
    caution: None,
};

pub static STOP_INDEXING: Tip = Tip {
    title: "Stop",
    body: "Stops the run in progress and switches to manual, so QuickSearch \
           stops watching for changes and reindexing on a schedule. The index \
           stays as it is and searching still works, but it falls behind as \
           your files change.\n\n\
           Saved at once: QuickSearch starts in manual next time too.",
    examples: &[],
    caution: None,
};

pub static RETURN_TO_AUTO: Tip = Tip {
    title: "Return to Automatic",
    body: "Goes back to watching your folders and reindexing on a schedule, \
           first catching up on everything that changed while indexing was \
           manual.\n\n\
           Also saved, so QuickSearch starts this way from now on.",
    examples: &[],
    caution: None,
};

pub static CLEAR_INDEX: Tip = Tip {
    title: "Clear index",
    body: "Deletes the index database. Searches find nothing until it is \
           built again, which takes a while for large folders. Your own files \
           are never touched.\n\n\
           QuickSearch asks first, then switches to manual so it does not \
           rebuild what you just deleted.",
    examples: &[],
    caution: Some("This cannot be undone: the index has to be built again from scratch."),
};

// --- Index status --------------------------------------------------------

pub static REBUILDING_FTS: Tip = Tip {
    title: "Rebuilding the full-text search cache",
    body: "A setting that decides which text is searchable changed, so \
           QuickSearch is updating the search index. On a large index this \
           takes several minutes and the progress can look frozen. Searching \
           keeps working meanwhile.",
    examples: &[],
    caution: None,
};

// --- Manage Index tab: indexed folders -----------------------------------

pub static ADD_ROOT: Tip = Tip {
    title: "Add an indexed folder",
    body: "Adds a folder and everything inside it to the index. Choose it \
           with the browser, or type the path and press Add.\n\n\
           An indexing pass adds the new folder to the current index; the \
           rest of the index is left alone and nothing is rebuilt.\n\n\
           Indexed folders may not overlap, so a folder inside another \
           indexed folder, or containing one, is refused, and so is a file. \
           A typed relative path starts from the config file's folder. A \
           path that does not exist yet, such as a drive not plugged in, is \
           added with a notice, and the first indexing run after it \
           appears indexes it.",
    examples: &["a second drive, or a network share you search often."],
    caution: None,
};

pub static REMOVE_ROOT: Tip = Tip {
    title: "Remove this folder",
    body: "Stops indexing the folder on this line and removes its entries \
           from the index. Everything else stays searchable and nothing is \
           rebuilt; your files are never touched.\n\n\
           Takes effect when you click Apply & Save.",
    examples: &[],
    caution: None,
};

pub static ROOT_WORKERS: Tip = Tip {
    title: "Workers",
    body: "How many folders QuickSearch reads at once inside this indexed \
           folder. More workers finish sooner on fast storage and network \
           shares, but compete for the same disk and can slow your \
           system.\n\n\
           auto means 4 on local storage and 16 on a network mount. Some \
           Linux shares (WebDAV, rclone, GVfs) look local to auto; set a \
           number for those. Takes effect on the next indexing run.",
    examples: &["1 or 2 to keep indexing low-priority."],
    caution: None,
};

pub static ROOT_COUNTS: Tip = Tip {
    title: "What this folder holds",
    body: "How many files under this folder are in the index, and how many \
           of those had their text extracted, so they can be found by content \
           and not only by name. The difference is files with no readable \
           text (images, videos, archives, programs) plus whatever the \
           extensions whitelist excludes.\n\n\
           Both update when an indexing run finishes.",
    examples: &[],
    caution: None,
};

// --- Manage Index tab: content filters -----------------------------------

pub static EXT_WHITELIST: Tip = Tip {
    title: "Full-text extensions whitelist",
    body: "Which kinds of file QuickSearch reads the text of. It limits \
           contents only: every file is still indexed and found by name and \
           path, but a file left off the list cannot be found by the text \
           inside it.\n\n\
           Empty, the default, means every kind QuickSearch understands. To \
           narrow it, enter one extension per line, leading dot optional. A \
           non-empty list also leaves out files with no extension, such as \
           Makefile or README, unless you add the line (none). Anything after \
           a # is a comment.",
    examples: &[
        "txt, md and pdf to keep the stored text small and focused on documents, \
         with everything else still findable by name.",
        "empty to search inside everything QuickSearch can read.",
    ],
    caution: None,
};

pub static IGNORE_PATTERNS: Tip = Tip {
    title: "Ignore patterns",
    body: "Files and folders kept out of the index entirely, name and text \
           alike. Patterns match names and paths, never file contents.\n\n\
           Without a slash, a pattern must match a whole file or folder name, \
           anywhere. With a slash, it matches a whole path and everything \
           under it. * is any run of characters, ? exactly one.\n\n\
           Adding a pattern removes what it matches; deleting it indexes \
           those files again. The Help tab has worked examples.",
    examples: &[
        "node_modules skips that folder wherever it appears.",
        "*.log skips every file ending in .log.",
        "a full path, such as your Videos folder, skips it and everything inside.",
    ],
    caution: None,
};

// --- Shared --------------------------------------------------------------

pub static APPLY_SAVE: Tip = Tip {
    title: "Apply & Save",
    body: "Saves these settings to the configuration file and applies them at \
           once. Until then, your edits are only staged.\n\n\
           Narrowing a setting removes the entries it now excludes; widening \
           one indexes what it now allows. Only the compact text index and \
           the hash sample size rebuild the index from scratch: at once in \
           automatic mode, otherwise after you confirm, or at the next run if \
           you choose Later.",
    examples: &[],
    caution: None,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Every tip, in file order. `all_lists_every_tip` catches one left out,
    /// which would otherwise skip every check below.
    const ALL: &[&Tip] = &[
        &SHOW_ADVANCED,
        &DATABASE_PATH,
        &REINDEX_INTERVAL,
        &INCLUDE_HIDDEN,
        &HASH_LENGTH,
        &MAX_STORED_TEXT,
        &MAX_TEXT_FILE_SIZE,
        &BATCH_SIZE,
        &MAX_WAL_SIZE,
        &COMPACT_INDEX,
        &FUZZY_DEFAULT,
        &FUZZY_EDITS,
        &DISPLAY_LIMIT,
        &SEARCH_CACHE,
        &RESULTS_PER_PAGE,
        &DEBOUNCE,
        &LIVE_RESULTS,
        &COLUMNS,
        &UI_SCALE,
        &SEARCH_HOTKEY,
        &COLOR_SCHEME,
        &ENABLE_PASSWORD,
        &CHANGE_PASSWORD,
        &DISABLE_PASSWORD,
        &SHOW_KEY,
        &REMEMBER_KEYCHAIN,
        &START_NOW,
        &STOP_INDEXING,
        &RETURN_TO_AUTO,
        &CLEAR_INDEX,
        &REBUILDING_FTS,
        &ADD_ROOT,
        &REMOVE_ROOT,
        &ROOT_WORKERS,
        &ROOT_COUNTS,
        &EXT_WHITELIST,
        &IGNORE_PATTERNS,
        &APPLY_SAVE,
    ];

    #[test]
    fn all_lists_every_tip() {
        // Split so this line does not count itself.
        let defined = include_str!("tips.rs")
            .matches(concat!(": Tip = ", "Tip {"))
            .count();
        assert_eq!(defined, ALL.len(), "a tip is missing from ALL");
    }

    fn all_text(tip: &Tip) -> String {
        let mut text = format!("{}\n{}", tip.title, tip.body);
        for example in tip.examples {
            text.push('\n');
            text.push_str(example);
        }
        if let Some(caution) = tip.caution {
            text.push('\n');
            text.push_str(caution);
        }
        text
    }

    #[test]
    fn no_tip_uses_an_em_dash() {
        for tip in ALL {
            assert!(
                !all_text(tip).contains('—'),
                "{} uses an em-dash",
                tip.title
            );
        }
    }

    #[test]
    fn every_tip_is_filled_in() {
        for tip in ALL {
            assert!(!tip.title.trim().is_empty(), "a tip has no title");
            assert!(
                !tip.title.ends_with('.'),
                "{}: title is not a sentence",
                tip.title
            );
            assert!(
                tip.body.trim().len() > 40,
                "{}: body says too little",
                tip.title
            );
            assert!(
                tip.body.trim_end().ends_with('.'),
                "{}: body is not a finished sentence",
                tip.title
            );
            // "Stop. Stops the run" is the restatement worth catching; the
            // title only counts as repeated when a word ends there.
            let restates = tip
                .body
                .strip_prefix(tip.title)
                .is_some_and(|rest| !rest.starts_with(|c: char| c.is_alphanumeric()));
            assert!(!restates, "{}: body repeats the title", tip.title);
            for example in tip.examples {
                assert!(
                    !example.trim().is_empty() && example.trim_end().ends_with('.'),
                    "{}: bad example {:?}",
                    tip.title,
                    example
                );
            }
            if let Some(caution) = tip.caution {
                assert!(
                    caution.trim_end().ends_with('.'),
                    "{}: caution is not a finished sentence",
                    tip.title
                );
            }
        }
    }

    #[test]
    fn no_tip_is_a_wall_of_text() {
        for tip in ALL {
            let len = all_text(tip).chars().count();
            assert!(len <= 900, "{} is {} characters long", tip.title, len);
        }
    }

    #[test]
    fn titles_are_distinct() {
        let mut seen: Vec<&str> = ALL.iter().map(|t| t.title).collect();
        seen.sort_unstable();
        let count = seen.len();
        seen.dedup();
        assert_eq!(count, seen.len(), "two tips share a title: {:?}", seen);
    }

    #[test]
    fn show_paints_every_part() {
        let ctx = crate::test_ui::ctx();
        let full = crate::test_ui::run_panel(&ctx, egui::vec2(800.0, 600.0), vec![], |ui| {
            HASH_LENGTH.show(ui)
        });
        let painted = crate::test_ui::painted_text(&full).join("\n");
        assert!(painted.contains(HASH_LENGTH.title), "no title: {painted}");
        assert!(painted.contains("How many bytes from the start"), "no body");
        assert!(painted.contains("•  8192, the default"), "no examples");
        assert!(
            painted.contains(HASH_LENGTH.caution.unwrap()),
            "no caution line"
        );
    }

    #[test]
    fn a_single_example_is_prefixed_with_example() {
        let ctx = crate::test_ui::ctx();
        let full = crate::test_ui::run_panel(&ctx, egui::vec2(800.0, 600.0), vec![], |ui| {
            INCLUDE_HIDDEN.show(ui)
        });
        let painted = crate::test_ui::painted_text(&full).join("\n");
        assert!(
            painted.contains(&format!("Example: {}", INCLUDE_HIDDEN.examples[0])),
            "{painted}"
        );
    }

    /// egui shows nothing on a disabled widget unless the *disabled*
    /// tooltip is set too.
    #[test]
    fn a_disabled_control_still_explains_itself() {
        let ctx = crate::test_ui::ctx();
        crate::test_ui::instant_tooltips(&ctx);
        let run = |events: Vec<egui::Event>| {
            crate::test_ui::run_panel(&ctx, egui::vec2(600.0, 400.0), events, |ui| {
                ui.add_enabled(false, egui::Button::new("Stop"))
                    .tip(&STOP_INDEXING);
            })
        };

        run(vec![]);
        let settled = run(vec![]);
        let pos = crate::test_ui::painted_text_center(&settled, "Stop").expect("button painted");
        let opening: String = STOP_INDEXING.body.chars().take(40).collect();
        assert!(
            crate::test_ui::hover_shows(run, pos, &opening),
            "a disabled control said nothing on hover"
        );
    }
}
