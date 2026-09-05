//! Byte-for-byte verification of one duplicate group, and the modal that
//! reports it. Grouping only hashes each file's size and head, so this is the
//! second opinion — asked for before what usually follows it, deletion.

use super::*;

use std::path::PathBuf;

use quicksearch_core::verify::{MemberVerdict, VerifyReport, VerifyUpdate};

use crate::format::{group_thousands, human_size};
use crate::ui_util::{centered_modal, hint, progress_bar};

const MODAL_WIDTH: f32 = 560.0;

pub(crate) enum VerifyState {
    Running {
        bytes_read: u64,
        /// Zero until the worker's first update lands; the bar is
        /// indeterminate until then.
        bytes_total: u64,
    },
    Done(Box<VerifyReport>),
    Cancelled,
}

pub(crate) struct VerifyModal {
    pub paths: Vec<PathBuf>,
    pub state: VerifyState,
}

impl VerifyModal {
    pub(crate) fn new(paths: Vec<PathBuf>) -> VerifyModal {
        VerifyModal {
            paths,
            state: VerifyState::Running {
                bytes_read: 0,
                bytes_total: 0,
            },
        }
    }
}

/// One line of the report, in the words the modal paints. `reference_len` is
/// how long the file everything was compared against was: an offset means
/// little on its own, and "byte 91 of 2.1 MB" is what says the difference is
/// in the header rather than in anything anyone typed.
pub(crate) fn verdict_line(verdict: &MemberVerdict, reference: bool, reference_len: u64) -> String {
    match verdict {
        MemberVerdict::Identical if reference => "compared against".to_string(),
        MemberVerdict::Identical => "identical".to_string(),
        MemberVerdict::DiffersAt(offset) => format!(
            "differs at byte {} of {}",
            group_thousands(*offset),
            human_size(reference_len)
        ),
        MemberVerdict::LengthDiffers { len, reference_len } => format!(
            "size differs: {} against {}",
            human_size(*len),
            human_size(*reference_len)
        ),
        MemberVerdict::Unreadable(e) => format!("could not be read — {e}"),
    }
}

/// The headline the report earns.
pub(crate) fn summary_line(report: &VerifyReport) -> String {
    let total = report.verdicts.len();
    if report.reference.is_none() {
        return "None of these files could be read.".to_string();
    }
    let differing = report.differing();
    if differing == 0 {
        return match total {
            0 | 1 => "Nothing to compare: the group holds one file.".to_string(),
            n => format!("All {n} files are byte-for-byte identical."),
        };
    }
    format!(
        "{} of {} files {} not identical.",
        differing,
        total,
        if differing == 1 { "is" } else { "are" }
    )
}

impl QuickSearchApp {
    pub(super) fn drain_verify(&mut self) {
        use std::sync::mpsc::TryRecvError;
        let Some(job) = &self.backend.verify_job else {
            return;
        };
        let mut finished = false;
        loop {
            match job.rx.try_recv() {
                Ok(VerifyUpdate::Progress {
                    bytes_read: read,
                    bytes_total: total,
                }) => {
                    if let Some(modal) = &mut self.verify {
                        modal.state = VerifyState::Running {
                            bytes_read: read,
                            bytes_total: total,
                        };
                    }
                }
                Ok(VerifyUpdate::Done(report)) => {
                    if let Some(modal) = &mut self.verify {
                        modal.state = VerifyState::Done(Box::new(report));
                    }
                    finished = true;
                    break;
                }
                Ok(VerifyUpdate::Cancelled) => {
                    if let Some(modal) = &mut self.verify {
                        modal.state = VerifyState::Cancelled;
                    }
                    finished = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                // The worker died without a terminal update.
                Err(TryRecvError::Disconnected) => {
                    if let Some(modal) = &mut self.verify {
                        if matches!(modal.state, VerifyState::Running { .. }) {
                            modal.state = VerifyState::Cancelled;
                        }
                    }
                    finished = true;
                    break;
                }
            }
        }
        if finished {
            self.backend.verify_job = None;
        }
    }

    pub(super) fn verify_modal_ui(&mut self, ctx: &egui::Context) {
        let Some(modal) = &self.verify else {
            return;
        };
        if verify_modal(ctx, modal) {
            self.backend.cancel_verify();
            self.verify = None;
        }
    }
}

/// Paint the modal; `true` when its dismiss button was clicked. Takes a bare
/// context, so a test can render it without an app or an index.
pub(crate) fn verify_modal(ctx: &egui::Context, modal: &VerifyModal) -> bool {
    centered_modal(ctx, "Verify duplicates", |ui| {
        ui.set_max_width(MODAL_WIDTH);
        match &modal.state {
            VerifyState::Running {
                bytes_read,
                bytes_total,
            } => {
                ui.label(format!(
                    "Comparing {} files byte for byte…",
                    modal.paths.len()
                ));
                let fraction =
                    (*bytes_total > 0).then(|| (*bytes_read as f64 / *bytes_total as f64) as f32);
                progress_bar(ui, fraction, MODAL_WIDTH);
                ui.label(hint(match bytes_total {
                    0 => format!("{} read", human_size(*bytes_read)),
                    total => format!("{} of {}", human_size(*bytes_read), human_size(*total)),
                }));
                ui.add_space(6.0);
                ui.horizontal(|ui| ui.button("Cancel").clicked()).inner
            }
            VerifyState::Cancelled => {
                ui.label("Verification cancelled.");
                ui.add_space(6.0);
                ui.horizontal(|ui| ui.button("Close").clicked()).inner
            }
            VerifyState::Done(report) => {
                let p = crate::color::palette(ui.visuals().dark_mode);
                let identical = report.all_identical() && report.reference.is_some();
                let color = if identical {
                    p.green
                } else {
                    ui.visuals().error_fg_color
                };
                ui.colored_label(color, summary_line(report));
                if !identical {
                    ui.label(
                        "Grouping only reads each file's size and how it begins; \
                         this read every byte.",
                    );
                    // The question this answers is the one everybody asks next:
                    // the files look the same when opened, so how can they
                    // differ? Named formats, because those are the ones people
                    // hit — a run of invoices or a folder of .docx.
                    ui.label(hint(
                        "A difference near the start of a file is usually metadata you \
                         never see. PDFs and Office documents store creation and \
                         modification times, document IDs and revision numbers inside \
                         the file itself, so copies that look and print identically are \
                         still different files on disk.",
                    ));
                }
                ui.add_space(6.0);
                // Listed even when everything matched: what was read.
                egui::ScrollArea::vertical()
                    .max_height(260.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for (i, path) in modal.paths.iter().enumerate() {
                            let Some(verdict) = report.verdicts.get(i) else {
                                continue;
                            };
                            let is_reference = report.reference == Some(i);
                            ui.horizontal_wrapped(|ui| {
                                ui.label(
                                    egui::RichText::new(path.display().to_string()).monospace(),
                                );
                                let line =
                                    verdict_line(verdict, is_reference, report.reference_len);
                                if verdict.is_identical() {
                                    ui.label(hint(line));
                                } else {
                                    ui.colored_label(
                                        ui.visuals().error_fg_color,
                                        egui::RichText::new(line).small(),
                                    );
                                }
                            });
                        }
                    });
                ui.add_space(6.0);
                ui.label(hint(format!("{} read", human_size(report.bytes_read))));
                ui.horizontal(|ui| ui.button("Close").clicked()).inner
            }
        }
    })
    .unwrap_or(false)
}

#[cfg(test)]
#[path = "verify_tests.rs"]
mod tests;
