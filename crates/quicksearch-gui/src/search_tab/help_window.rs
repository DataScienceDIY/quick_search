//! The floating search-syntax reference.

use super::*;

/// `(syntax, meaning)`, one row each.
const QUERY_SYNTAX: [(&str, &str); 9] = [
    (
        "budget report",
        "names, contents, and paths containing \"budget report\"",
    ),
    (
        "\"exact phrase\"",
        "quotes keep spaces, stars, and filter-like words literal",
    ),
    (
        "bud*report",
        "* matches any run of characters (within a line); also works in name: values",
    ),
    (
        "regex:\"(foo|bar)\\d+\"",
        "regular expression, matched against names, contents, and paths",
    ),
    (
        "type:Document",
        "file class: Audio, Image, Video, Document, Text, Archive, Spreadsheet, \
         Presentation, Folder",
    ),
    (
        "modified:>=2024-01-01",
        "modification date (yyyy-mm-dd); also <, <=, > and =",
    ),
    (
        "path:/home/me/docs",
        "only results in that folder and its subfolders; quote paths containing spaces",
    ),
    ("mime:application/pdf", "exact MIME type"),
    (
        "name:report",
        "filename contains, applied as an unranked filter",
    ),
];

impl SearchTab {
    pub(super) fn help_window_ui(&mut self, ctx: &egui::Context) {
        let mut open = self.help_open;
        egui::Window::new("Query syntax")
            .open(&mut open)
            .resizable(false)
            .default_width(540.0)
            .show(ctx, |ui| {
                ui.label(
                    "Everything that is not a filter is matched as one phrase, in order. \
                     Filters combine freely with the search text.",
                );
                ui.add_space(6.0);
                egui::Grid::new("query-syntax-table")
                    .num_columns(2)
                    .spacing([18.0, 5.0])
                    .striped(true)
                    .show(ui, |ui| {
                        for (syntax, meaning) in QUERY_SYNTAX {
                            ui.monospace(syntax);
                            ui.label(meaning);
                            ui.end_row();
                        }
                    });
                ui.add_space(6.0);
                ui.label("Example:");
                ui.monospace("type:Document modified:>=2024-01-01 quarterly budget");
                ui.add_space(6.0);
                ui.label(hint(
                    "Ranking: exact filename matches, then filename substrings, then \
                     full-text matches (ordered by occurrences), then fuzzy matches \
                     when enabled, and finally matches on the rest of the file path.",
                ));
                ui.label(hint(
                    "The Help tab explains ranking in full. On Linux, man quicksearch \
                     covers every filter and its aliases.",
                ));
            });
        self.help_open = open;
    }
}
