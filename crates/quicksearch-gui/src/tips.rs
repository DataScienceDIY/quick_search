//! Plain-language tooltips for the configuration controls: every setting in
//! the Options window, and every configuration control on the Manage Index
//! tab, explains itself on hover.

/// How wide a tooltip may get; matches `manage_tab::db_size_tooltip`.
const TIP_WIDTH: f32 = 420.0;

/// One control's tooltip.
pub struct Tip {
    /// The setting's name, in bold at the top.
    pub title: &'static str,
    /// The explanation. One or more paragraphs separated by `"\n\n"`.
    pub body: &'static str,
    /// Concrete values and when to choose them, rendered small underneath.
    /// Empty where a setting has nothing to weigh up.
    pub examples: &'static [&'static str],
    /// A consequence worth seeing before the click, rendered small and
    /// orange. Reserved for rebuilds and deletions.
    pub caution: Option<&'static str>,
}

impl Tip {
    /// Render into a hover popup.
    pub fn show(&self, ui: &mut egui::Ui) {
        ui.set_max_width(TIP_WIDTH);
        ui.strong(self.title);
        ui.label(self.body);
        if !self.examples.is_empty() {
            ui.add_space(4.0);
            // One example reads as a sentence; several read as a list.
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

/// Attach a [`Tip`] to any widget.
pub trait Tipped {
    /// Show `tip` on hover, whether or not the widget is enabled: a greyed
    /// out button is exactly when someone wants to know what it would do.
    fn tip(self, tip: &'static Tip) -> Self;
}

impl Tipped for egui::Response {
    fn tip(self, tip: &'static Tip) -> egui::Response {
        self.on_hover_ui(|ui| tip.show(ui))
            .on_disabled_hover_ui(|ui| tip.show(ui))
    }
}

/// One row of a two-column settings grid: the label and the control share a
/// tooltip, so hovering the name works as well as hovering the widget.
pub fn tip_row(
    ui: &mut egui::Ui,
    label: &str,
    tip: &'static Tip,
    widget: impl FnOnce(&mut egui::Ui) -> egui::Response,
) {
    ui.label(label).tip(tip);
    widget(ui).tip(tip);
    ui.end_row();
}

// --- Options: Paths ------------------------------------------------------

pub static DATABASE_PATH: Tip = Tip {
    title: "Database file",
    body: "Where QuickSearch keeps its index: a single file holding the \
           names, locations and text of everything it has indexed. It grows \
           with the number of files indexed, so it wants a drive with room \
           to spare.\n\n\
           Pointing this somewhere else switches to the index at the new \
           location, building a fresh one there if nothing exists yet. The \
           old file stays on disk until you delete it. Keep it off a folder \
           that syncs to the cloud, such as OneDrive or Dropbox: it is far \
           too large and too busy to synchronise.",
    examples: &["a path on a second, larger drive when the system drive is tight on space."],
    caution: None,
};

// --- Options: Indexing ---------------------------------------------------

pub static REINDEX_INTERVAL: Tip = Tip {
    title: "Full reindex every",
    body: "Indexed folders are watched, so ordinary changes appear within \
           seconds. This is the safety net behind that: a full sweep that \
           picks up anything the watching missed, such as changes made while \
           QuickSearch was closed, or on a drive that does not report them.\n\n\
           A sweep costs some disk activity for a few minutes, and searching \
           keeps working throughout.",
    examples: &[
        "1440, once a day, suits an ordinary laptop.",
        "60 when you work on a network share, or a drive other machines write to.",
        "10080, once a week, for an archive that barely changes.",
    ],
    caution: None,
};

pub static FOLLOW_SYMLINKS: Tip = Tip {
    title: "Follow symlinks",
    body: "Symlinks are entries that stand in for a file or folder living \
           somewhere else. Off, QuickSearch steps over them, so nothing is \
           indexed twice under two names. On, it looks through them and \
           indexes what they point at, stored under the real location rather \
           than the link's.\n\n\
           Turning this off removes the entries that are no longer in scope; \
           turning it on indexes whatever it now reaches. Neither rebuilds \
           the index.",
    examples: &[
        "on when a folder you search lives outside your indexed folders and is reached \
         through a link.",
    ],
    caution: None,
};

pub static INCLUDE_HIDDEN: Tip = Tip {
    title: "Include hidden files",
    body: "Files and folders the system keeps out of sight: names beginning \
           with a dot on Linux and macOS, plus anything carrying the Hidden \
           attribute on Windows, such as AppData and $RECYCLE.BIN. They are \
           mostly program settings and caches, so leaving them out keeps the \
           index smaller and the results cleaner.\n\n\
           Folders marked System but not Hidden are indexed either way. Cloud \
           sync folders and folders given a custom icon carry that mark only \
           to get the icon, and are ordinary folders otherwise.",
    examples: &["on when you want to find configuration files such as .bashrc or .gitconfig."],
    caution: None,
};

// --- Options: Processing -------------------------------------------------

pub static TOKENIZER: Tip = Tip {
    title: "Tokenizer",
    body: "How the text inside your files is cut up so that it can be \
           searched.\n\n\
           trigram indexes every run of three characters, so a search for \
           \"port\" also finds \"airport\", and it works for languages that \
           do not put spaces between words. The other two index whole words \
           instead: the index is smaller and faster, but a search matches \
           only from the start of a word.",
    examples: &[
        "trigram, the default, for finding a fragment anywhere inside a word.",
        "unicode61 when you only ever search whole words and want the smallest, \
         fastest index.",
        "porter to also match English word endings, so \"running\" finds \"run\".",
    ],
    caution: Some("Changing this builds the whole index again from scratch."),
};

pub static HASH_LENGTH: Tip = Tip {
    title: "Hash sample size",
    body: "How much of the start of each file QuickSearch reads in order to \
           recognise it. Those first bytes give the file its fingerprint, \
           which is how the Duplicates tab knows two files are identical; \
           they also say what kind of file it is, and for a small text file \
           they are the whole text.\n\n\
           Reading more is more reliable and slower, particularly over a \
           network.",
    examples: &[
        "8192, the default, is right for almost everyone.",
        "higher when the Duplicates tab groups files that are not really identical, \
         which happens with disk images and other formats that begin with a lot of \
         empty space.",
    ],
    caution: Some("Changing this builds the whole index again from scratch."),
};

pub static MAX_STORED_TEXT: Tip = Tip {
    title: "Max stored text",
    body: "How much text QuickSearch keeps out of any one file. Text past \
           this point is not stored, so a search will not find a word that \
           appears only deep inside a very long document. File names, sizes \
           and dates are unaffected.\n\n\
           Along with the two settings below it, this is one of the largest \
           influences on how big the index becomes.",
    examples: &[
        "262144, 256 KB, covers the whole of most documents.",
        "65536, 64 KB, to shrink the index when you mostly search the opening pages.",
        "higher when you search long books, transcripts or logs and expect to find \
         words near the end.",
    ],
    caution: None,
};

pub static MAX_TEXT_FILE_SIZE: Tip = Tip {
    title: "Max text file size",
    body: "Files larger than this are indexed by name only: QuickSearch does \
           not open them to read the text inside. It keeps one stray huge \
           file from holding up an indexing run.\n\n\
           Those files still appear in results, found by their name, size or \
           date.",
    examples: &[
        "2097152, 2 MB, skips very few ordinary documents.",
        "52428800, 50 MB, when you search inside large log files or scanned PDFs.",
    ],
    caution: None,
};

pub static BATCH_SIZE: Tip = Tip {
    title: "Batch size",
    body: "How many files QuickSearch handles per write to the index while \
           indexing. Larger batches mean fewer, bigger writes, which is a \
           little faster and uses more memory.\n\n\
           A speed setting only: it changes nothing about what you can find, \
           and most people never need to touch it.",
    examples: &[
        "500, the default, balances speed against memory.",
        "lower, around 50, on a machine with very little memory to spare.",
    ],
    caution: None,
};

pub static MAX_WAL_SIZE: Tip = Tip {
    title: "Max WAL size",
    body: "While indexing, changes are written to a companion file beside \
           the index and folded in afterwards. That normally happens by \
           itself, but during a long run with searches going on at the same \
           time the companion file keeps growing, sometimes past the size of \
           the index. This is the point at which QuickSearch pauses and folds \
           it in regardless.\n\n\
           Another speed setting; the default suits most machines.",
    examples: &[
        "536870912, 512 MB, is the default.",
        "67108864, 64 MB, when disk space is tight.",
        "0 to never force it and let the database decide. Any other value below 16 MB \
         is treated as 16 MB.",
    ],
    caution: None,
};

pub static STORE_TEXT: Tip = Tip {
    title: "Store text for snippets",
    body: "Keeps the text QuickSearch reads out of your files, rather than \
           only the index needed to search it. It is what makes the preview \
           line underneath a result possible.\n\n\
           Off, searching inside files still works, but there are no \
           previews, no ranking by how often a word appears, no telling \
           Report from report, and no allowance for typos inside file \
           contents. The index shrinks considerably.\n\n\
           Turning it off discards the stored text at once; turning it back \
           on reads your files again.",
    examples: &[
        "off when the index has grown larger than you want and you can do without previews.",
    ],
    caution: None,
};

// --- Options: Search -----------------------------------------------------

pub static FUZZY_DEFAULT: Tip = Tip {
    title: "Fuzzy search ON by default",
    body: "Whether the Fuzzy box on the Search tab starts ticked each time \
           QuickSearch opens. Fuzzy search also finds matches with typos in \
           them, at some cost in speed. Either way you can tick and untick \
           it whenever you like.",
    examples: &["on when you often look for names you are not sure how to spell."],
    caution: None,
};

pub static FUZZY_EDITS: Tip = Tip {
    title: "Fuzzy edit distance",
    body: "How far a word may sit from what you typed and still count as a \
           match while Fuzzy is on. One edit is one letter added, removed or \
           changed, so \"reciept\" is one edit away from \"receipt\".\n\n\
           The allowance grows with the length of what you type, one edit per \
           three characters, up to the value set here. Short searches stay \
           strict, so that three letters do not match half the index.",
    examples: &[
        "2, the default, allows one edit for short words and two for longer ones.",
        "0 turns typo matching off altogether, even with the Fuzzy box ticked.",
        "3 or more is allowed, but searches get slower and pull in a lot of \
         unrelated files.",
    ],
    caution: None,
};

pub static DISPLAY_LIMIT: Tip = Tip {
    title: "Display limit",
    body: "The most results one search will gather and show. A search \
           matching thousands of files stops here, which keeps the list quick \
           to scroll and cheap to hold in memory.\n\n\
           The best matches come first, so a lower limit rarely hides what \
           you were looking for.",
    examples: &[
        "1000, the default, is more than anyone scrolls through.",
        "higher when you use QuickSearch to list every file of a kind, such as \
         type:Image, and want them all at once.",
    ],
    caution: None,
};

pub static RESULTS_PER_PAGE: Tip = Tip {
    title: "Stream batch size",
    body: "Results arrive in batches while a search runs, and this is how \
           many are in each one. Smaller batches put the first results on \
           screen sooner and update the list more often; larger ones do less \
           work in total.\n\n\
           This is not a page size: scrolling the results does not go back \
           for more.",
    examples: &[
        "100, the default, feels immediate on most machines.",
        "25 when the first results are slow to appear on a large index.",
    ],
    caution: None,
};

pub static DEBOUNCE: Tip = Tip {
    title: "Debounce",
    body: "How long QuickSearch waits after your last keystroke before it \
           searches, so that typing a word does not fire off a search for \
           every letter in it. 1000 milliseconds is one second.",
    examples: &[
        "150, the default, keeps up with ordinary typing.",
        "0 to chase every keystroke on a fast machine with a modest index.",
        "300 or more when the results flicker or stutter as you type.",
    ],
    caution: None,
};

// --- Options: Interface --------------------------------------------------

pub static UI_SCALE: Tip = Tip {
    title: "UI scale",
    body: "Zooms the whole window: text, spacing and controls together. 1.00 \
           is the ordinary size for your screen.\n\n\
           Ctrl with + or - changes it for the moment without saving, and \
           Ctrl 0 puts it back. This slider is the size QuickSearch starts \
           at.",
    examples: &[
        "1.40 or more on a high resolution screen where the text looks small.",
        "0.80 to fit more results on screen at once.",
    ],
    caution: None,
};

pub static SEARCH_HOTKEY: Tip = Tip {
    title: "Search shortcut",
    body: "One key combination that brings QuickSearch to the front from \
           anywhere, whatever you were doing, and puts the cursor in the \
           search box with the previous search selected, so you can simply \
           start typing.\n\n\
           Click the button and press the keys you want. Combine Ctrl, Alt \
           and Shift with one other key. Clear switches the shortcut off.\n\n\
           On Wayland the shortcut is registered with your desktop rather \
           than claimed directly, so your desktop may assign a different key \
           or ask you to confirm it, and its own keyboard settings are where \
           to change it afterwards. Wayland also does not let any application \
           put itself in front of what you are doing, so there the shortcut \
           selects the Search tab and the search box, but bringing the window \
           forward is up to your desktop.",
    examples: &[
        "Ctrl+Shift+F, the default, which few other programs use.",
        "Ctrl+Alt+Space if something else on your system already answers to it.",
    ],
    caution: None,
};

pub static COLOR_SCHEME: Tip = Tip {
    title: "Color scheme",
    body: "Whether QuickSearch is dark or light. It takes effect as soon as \
           you apply it, with no restart.\n\n\
           QuickSearch does not follow your desktop's own light and dark \
           setting: on Linux the only way to read that is to connect to your \
           desktop over the message bus and listen to your settings as they \
           change, which is more of your session than a search tool should \
           be in. So it is asked here instead, once.",
    examples: &["Light for a bright room, or to match the rest of a light desktop."],
    caution: None,
};

// --- Options: Security ---------------------------------------------------

pub static ENABLE_PASSWORD: Tip = Tip {
    title: "Enable password protection",
    body: "Encrypts the index with a password of your choosing. The index \
           holds the names and the text of your files, so anyone who can \
           read that file can read those; encrypting it means they cannot.\n\n\
           QuickSearch then asks for the password each time it starts, unless \
           you let it remember.",
    examples: &[],
    caution: Some(
        "Turning protection on deletes the index and builds it again. Your files are \
         not touched.",
    ),
};

pub static CHANGE_PASSWORD: Tip = Tip {
    title: "Change password",
    body: "Replaces the password the index is encrypted with. You are asked \
           for a new one, and the index is encrypted again under it.",
    examples: &[],
    caution: Some(
        "Changing the password deletes the index and builds it again. Your files are \
         not touched.",
    ),
};

pub static DISABLE_PASSWORD: Tip = Tip {
    title: "Disable protection",
    body: "Removes the password and leaves the index unencrypted on disk. \
           Anyone able to read that file can then see the names of your files \
           and the text inside them.",
    examples: &[],
    caution: Some(
        "Turning protection off deletes the index and builds it again. Your files are \
         not touched.",
    ),
};

pub static REMEMBER_KEYCHAIN: Tip = Tip {
    title: "Remember on this device",
    body: "Hands the key to the password store your system already has, such \
           as GNOME Keyring, KWallet, or Windows Credential Manager, so that \
           QuickSearch can unlock the index without asking at startup.\n\n\
           The password itself is never stored, only the key worked out from \
           it, and only on this machine. Off, you type the password each time \
           QuickSearch starts.",
    examples: &[],
    caution: None,
};

// --- Manage Index tab: indexing controls ---------------------------------

pub static START_NOW: Tip = Tip {
    title: "Start indexing now",
    body: "Runs a full pass over your indexed folders straight away instead \
           of waiting for the next scheduled one. Worth doing after adding a \
           folder, after changing a filter, or when the computer has been off \
           for a while.\n\n\
           Searching carries on working while it runs. Unavailable while a \
           run is already under way.",
    examples: &[],
    caution: None,
};

pub static STOP_INDEXING: Tip = Tip {
    title: "Stop",
    body: "Stops the run in progress and switches to manual, so QuickSearch \
           no longer watches for changes or reindexes on a schedule. The \
           index stays as it is and searching still works, but it drifts out \
           of date as your files change.\n\n\
           Saved immediately: QuickSearch is still in manual the next time it \
           starts.",
    examples: &[],
    caution: None,
};

pub static RETURN_TO_AUTO: Tip = Tip {
    title: "Return to Automatic",
    body: "Goes back to watching your folders and reindexing on a schedule, \
           catching up on everything that changed while indexing was manual.\n\n\
           Also saved, so this is how QuickSearch starts from now on.",
    examples: &[],
    caution: None,
};

pub static CLEAR_INDEX: Tip = Tip {
    title: "Clear index",
    body: "Deletes the index database. Searching finds nothing until it is \
           built again, which for a large folder takes a while. Your own \
           files are never touched.\n\n\
           QuickSearch asks for confirmation first, then drops to manual so \
           that it does not immediately rebuild what you just deleted.",
    examples: &[],
    caution: Some("This cannot be undone: the index has to be built from scratch again."),
};

// --- Manage Index tab: indexed folders -----------------------------------

pub static ADD_ROOT: Tip = Tip {
    title: "Add an indexed folder",
    body: "Adds a folder for QuickSearch to index, along with everything \
           inside it. Choose it with the browser, or type the path and press \
           Add.\n\n\
           Indexed folders may not overlap, so a folder already inside \
           another one is refused. Adding a folder starts an indexing pass to \
           pick it up and leaves the rest of the index alone.",
    examples: &["a second drive, or a network share you search often."],
    caution: None,
};

pub static REMOVE_ROOT: Tip = Tip {
    title: "Remove this folder",
    body: "Stops indexing this folder and removes its entries from the \
           index. The rest of the index is left alone, and the files \
           themselves are not touched.\n\n\
           Takes effect when you click Apply & Save.",
    examples: &[],
    caution: None,
};

pub static ROOT_WORKERS: Tip = Tip {
    title: "Workers",
    body: "How many folders QuickSearch explores at once inside this indexed \
           folder. More of them finish sooner on storage that answers many \
           requests at a time, which network drives do especially well, but \
           they compete for the same disk.\n\n\
           auto reads 4 on local storage and 16 on a network mount. Takes \
           effect on the next indexing run.",
    examples: &[
        "auto unless indexing is slower than you would expect.",
        "16 or more for a network share that is slow to answer each request.",
        "2 to keep indexing out of the way on an older machine.",
    ],
    caution: None,
};

// --- Manage Index tab: content filters -----------------------------------

pub static EXT_WHITELIST: Tip = Tip {
    title: "Full-text extensions whitelist",
    body: "Which kinds of file QuickSearch reads the text out of, one \
           extension per line, the leading dot optional. Empty means every \
           kind it understands.\n\n\
           Every file is still indexed by name whatever you put here. A list \
           also leaves out files with no extension at all, such as Makefile \
           or README, unless you add the line (none). Anything after a # is a \
           comment, so a file type can be switched off without losing the \
           line.\n\n\
           Narrowing the list discards the text it now excludes; widening it \
           reads those files again.",
    examples: &[
        "txt, md and pdf to keep the index small and focused on documents.",
        "empty to search inside everything QuickSearch can read.",
    ],
    caution: None,
};

pub static IGNORE_PATTERNS: Tip = Tip {
    title: "Ignore patterns",
    body: "Files and folders left out of the index entirely, by name and by \
           content alike. Type one pattern and click Add.\n\n\
           A pattern without a slash matches a file or folder name anywhere, \
           and must match the whole name: .jpg matches only something called \
           exactly that, while *.jpg matches every JPEG. A pattern with a \
           slash in it is matched against the whole path, and skips \
           everything underneath. * stands for any run of characters and ? \
           for a single one.\n\n\
           Adding a pattern removes the entries it matches; removing one \
           indexes them again.",
    examples: &[
        "node_modules to skip that folder wherever it turns up.",
        "*.tmp to skip temporary files by extension.",
        "a full path such as the Videos folder to skip it and everything inside it.",
    ],
    caution: None,
};

// --- Shared --------------------------------------------------------------

pub static APPLY_SAVE: Tip = Tip {
    title: "Apply & Save",
    body: "Writes these settings to the configuration file and puts them to \
           work straight away. Until you click here, your edits are only \
           staged.\n\n\
           Narrowing a setting removes the entries it now excludes; widening \
           one indexes whatever it now allows. Only the tokenizer, the hash \
           sample size and password protection need the index built again \
           from scratch, and those ask first.",
    examples: &[],
    caution: None,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Every tip in the file. A tip missing from here is only missing from
    /// the checks below, so keep it in step when adding one.
    const ALL: &[&Tip] = &[
        &DATABASE_PATH,
        &REINDEX_INTERVAL,
        &FOLLOW_SYMLINKS,
        &INCLUDE_HIDDEN,
        &TOKENIZER,
        &HASH_LENGTH,
        &MAX_STORED_TEXT,
        &MAX_TEXT_FILE_SIZE,
        &BATCH_SIZE,
        &MAX_WAL_SIZE,
        &STORE_TEXT,
        &FUZZY_DEFAULT,
        &FUZZY_EDITS,
        &DISPLAY_LIMIT,
        &RESULTS_PER_PAGE,
        &DEBOUNCE,
        &UI_SCALE,
        &SEARCH_HOTKEY,
        &COLOR_SCHEME,
        &ENABLE_PASSWORD,
        &CHANGE_PASSWORD,
        &DISABLE_PASSWORD,
        &REMEMBER_KEYCHAIN,
        &START_NOW,
        &STOP_INDEXING,
        &RETURN_TO_AUTO,
        &CLEAR_INDEX,
        &ADD_ROOT,
        &REMOVE_ROOT,
        &ROOT_WORKERS,
        &EXT_WHITELIST,
        &IGNORE_PATTERNS,
        &APPLY_SAVE,
    ];

    /// Everything a tip can put on screen, as one string.
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

    /// House style: these tooltips use no em-dashes.
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
            // "Stops the run in progress" under the title "Stop" is fine;
            // "Stop. Stops the run" is the restatement worth catching, so
            // the title only counts as repeated when a word ends there.
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

    /// A tooltip nobody reads to the end helps nobody.
    #[test]
    fn no_tip_is_a_wall_of_text() {
        for tip in ALL {
            let len = all_text(tip).chars().count();
            assert!(len <= 900, "{} is {} characters long", tip.title, len);
        }
    }

    /// Two controls sharing a title means one of them was pasted from the
    /// other and never renamed.
    #[test]
    fn titles_are_distinct() {
        let mut seen: Vec<&str> = ALL.iter().map(|t| t.title).collect();
        seen.sort_unstable();
        let count = seen.len();
        seen.dedup();
        assert_eq!(count, seen.len(), "two tips share a title: {:?}", seen);
    }

    /// The renderer puts every part on screen: title, body, examples and
    /// caution. Written against the tip with all four.
    #[test]
    fn show_paints_every_part() {
        let ctx = egui::Context::default();
        let input = crate::test_ui::raw_input(egui::vec2(800.0, 600.0), vec![]);
        let full = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| TOKENIZER.show(ui));
        });
        let painted = crate::test_ui::painted_text(&full).join("\n");
        assert!(painted.contains(TOKENIZER.title), "no title: {painted}");
        assert!(painted.contains("trigram indexes every run"), "no body");
        assert!(painted.contains("•  trigram, the default"), "no examples");
        assert!(
            painted.contains(TOKENIZER.caution.unwrap()),
            "no caution line"
        );
    }

    /// A lone example reads as a sentence rather than a one-item list.
    #[test]
    fn a_single_example_is_prefixed_with_example() {
        let ctx = egui::Context::default();
        let input = crate::test_ui::raw_input(egui::vec2(800.0, 600.0), vec![]);
        let full = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| INCLUDE_HIDDEN.show(ui));
        });
        let painted = crate::test_ui::painted_text(&full).join("\n");
        assert!(
            painted.contains(&format!("Example: {}", INCLUDE_HIDDEN.examples[0])),
            "{painted}"
        );
    }

    /// A greyed-out control still explains itself: egui shows nothing on a
    /// disabled widget unless the *disabled* tooltip is set too.
    #[test]
    fn a_disabled_control_still_explains_itself() {
        let ctx = egui::Context::default();
        ctx.style_mut(|s| {
            s.interaction.tooltip_delay = 0.0;
            s.interaction.show_tooltips_only_when_still = false;
        });
        let run = |events: Vec<egui::Event>| {
            let input = crate::test_ui::raw_input(egui::vec2(600.0, 400.0), events);
            ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.add_enabled(false, egui::Button::new("Stop"))
                        .tip(&STOP_INDEXING);
                });
            })
        };

        run(vec![]);
        let settled = run(vec![]);
        let pos = crate::test_ui::painted_text_center(&settled, "Stop").expect("button painted");
        let opening: String = STOP_INDEXING.body.chars().take(40).collect();
        let mut out = run(vec![egui::Event::PointerMoved(pos)]);
        for _ in 0..3 {
            if crate::test_ui::painted_text(&out)
                .join("\n")
                .contains(&opening)
            {
                return;
            }
            out = run(vec![]);
        }
        panic!("a disabled control said nothing on hover");
    }
}
