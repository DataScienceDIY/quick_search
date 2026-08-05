//! The Help tab: a quickstart guide for first-time users.
//!
//! Everything technical — building, configuration, the complete query
//! reference — deliberately stays in README.md; this page only has to get
//! someone from a fresh install to useful search results.

pub fn ui(ui: &mut egui::Ui) {
    let scroll = egui::ScrollArea::vertical()
        .auto_shrink([false; 2])
        .show(ui, |ui| {
            // Cap the column like a document page: a maximized window
            // would otherwise stretch every paragraph into one long line.
            ui.set_max_width(620.0);

            ui.heading(egui::RichText::new("Welcome to QuickSearch").strong());
            ui.add_space(4.0);
            ui.label(
                "QuickSearch keeps an index of the folders you choose and finds \
                 files by name and by what is inside them, as you type.",
            );

            ui.add_space(12.0);
            ui.heading(egui::RichText::new("Getting started").strong());
            ui.add_space(4.0);
            ui.label(
                "1.  The first time QuickSearch runs it starts indexing your home \
                 folder on its own. The status bar along the bottom shows the \
                 progress, and searching already works while it runs.",
            );
            ui.label(
                "2.  To index different folders, open the Manage Index tab and edit \
                 the folder list. Indexed folders are watched, so the index follows \
                 your files as they change.",
            );
            ui.label(
                "3.  Type in the search box on the Search tab. Results appear as \
                 you type, best matches first.",
            );

            ui.add_space(12.0);
            ui.heading(egui::RichText::new("Searching").strong());
            ui.add_space(4.0);
            ui.label("Plain words match file names, file contents, and paths:");
            ui.monospace("quarterly budget");
            ui.label(
                "Filters narrow the results and combine freely with the search \
                 words:",
            );
            ui.monospace("type:Document modified:>=2024-01-01 report");
            ui.label("The ? button next to the search box shows the full query syntax.");
            ui.add_space(6.0);
            ui.label(
                "•  Tick Fuzzy to also find matches with typos in them, at some \
                 cost in speed.",
            );
            ui.label(
                "•  Click a column header — Name, Path, Size, Modified, Rank — to \
                 sort the results; click it again to reverse the order.",
            );
            ui.label(
                "•  Right-click a result to open it, open its containing folder, \
                 or hide files like it from the results.",
            );
            ui.label(
                "•  Matches inside a file's contents show a snippet of the \
                 surrounding text under the file name.",
            );

            ui.add_space(12.0);
            ui.heading(egui::RichText::new("The other tabs").strong());
            ui.add_space(4.0);
            egui::Grid::new("help-tabs")
                .num_columns(2)
                .spacing([18.0, 5.0])
                .show(ui, |ui| {
                    let row = |ui: &mut egui::Ui, name: &str, what: &str| {
                        ui.strong(name);
                        ui.label(what);
                        ui.end_row();
                    };
                    row(
                        ui,
                        "Manage Index",
                        "indexing status and controls, the indexed folder list, \
                         and the filters that decide what is skipped",
                    );
                    row(
                        ui,
                        "Duplicates",
                        "files whose contents are identical, grouped",
                    );
                    row(
                        ui,
                        "Logs",
                        "warnings from indexing and folder watching that a \
                         terminal would have shown",
                    );
                    row(ui, "⚙ (top right)", "application options");
                });

            ui.add_space(12.0);
            ui.heading(egui::RichText::new("Terminal").strong());
            ui.add_space(4.0);
            ui.label("QuickSearch also searches straight from a terminal:");
            ui.monospace("quicksearch \"quarterly budget\"");
            ui.label(
                "On Windows use quicksearch-cli. Either way, --help lists all \
                 the flags.",
            );

            ui.add_space(12.0);
            ui.horizontal_wrapped(|ui| {
                // The sentence is assembled from several widgets, so the
                // spacing between them has to come from the text itself.
                ui.spacing_mut().item_spacing.x = 0.0;
                let quiet = |text: &str| egui::RichText::new(text).small().weak();
                ui.label(quiet(
                    "Building from source, configuration, query structuring and more \
                     are covered in ",
                ));
                if let Some(path) = readme_path() {
                    let path = path.display().to_string();
                    if ui
                        .link(egui::RichText::new("README.md").small())
                        .on_hover_text(&path)
                        .clicked()
                    {
                        crate::platform::open_file(&path);
                    }
                } else {
                    ui.label(quiet("README.md"));
                }
                ui.label(quiet("."));
            });
        });
    crate::ui_util::more_below_hint(ui, &scroll);
}

/// Where this build left the README: under the install prefix's `share/doc`
/// (the .deb puts it in `/usr/share/doc/quicksearch/`), beside the executable
/// (the Windows installer and portable copies), or at the top of a build tree
/// a few levels above `target/`.
fn readme_path() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let installed = dir
        .parent()
        .map(|prefix| prefix.join("share/doc/quicksearch/README.md"));
    installed
        .into_iter()
        .chain(dir.ancestors().take(4).map(|d| d.join("README.md")))
        .find(|p| p.is_file())
}
