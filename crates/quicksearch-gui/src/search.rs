#![allow(non_snake_case)]

use std::sync::Arc;
use std::time::Instant;
use dioxus::prelude::*;
use quicksearch_core::indexing::{IndexingService, SearchResult};
use quicksearch_core::search_sql::{build_count, build_select, SearchArgs};

/// One page of results. Tuned to keep DOM size bounded — rendering ten
/// thousand `<tr>` nodes wedges WebKit for tens of seconds.
const PAGE_SIZE: u32 = 50;

#[derive(Props, Clone)]
pub struct SearchProps {
    pub indexing_service: Arc<IndexingService>,
    pub db_path: String,
}

impl PartialEq for SearchProps {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.indexing_service, &other.indexing_service) && self.db_path == other.db_path
    }
}

pub fn Search(props: SearchProps) -> Element {
    let mut search_type = use_signal(|| "fulltext".to_string());
    let mut search_term = use_signal(|| String::new());
    let mut fulltext_exact = use_signal(|| false);
    let mut fulltext_case_sensitive = use_signal(|| false);
    let mut search_results = use_signal(|| Vec::<SearchResult>::new());
    let mut search_error = use_signal(|| None::<String>);
    let mut is_searching = use_signal(|| false);
    let mut last_search_time = use_signal(|| None::<f64>);
    let mut show_corruption_dialog = use_signal(|| false);
    let mut current_page = use_signal(|| 1u32);
    let mut total_count = use_signal(|| None::<u64>);
    let mut last_args = use_signal(|| None::<SearchArgs>);
    let mut goto_input = use_signal(|| String::new());

    let service = props.indexing_service.clone();
    let db_path = props.db_path.clone();

    // Spawn a search task. `refresh_count` is true for fresh searches and
    // false for in-place page navigation (the cached total still applies).
    let run_query = {
        let service = service.clone();
        let db_path = db_path.clone();
        move |args: SearchArgs, page: u32, refresh_count: bool| {
            let service = service.clone();
            let db_path = db_path.clone();
            spawn(async move {
                is_searching.set(true);
                search_error.set(None);
                last_search_time.set(None);
                let start = Instant::now();

                let count_sql = if refresh_count {
                    match build_count(&args) {
                        Ok(s) => Some(s),
                        Err(e) => {
                            search_error.set(Some(e));
                            is_searching.set(false);
                            return;
                        }
                    }
                } else {
                    None
                };

                let offset = page.saturating_sub(1).saturating_mul(PAGE_SIZE);
                // Validate early for the filename/duplicates branch so we
                // surface parse errors before dispatching the blocking task.
                let precomputed_select_sql = if args.search_type == "fulltext" {
                    None
                } else {
                    match build_select(&args, PAGE_SIZE, offset) {
                        Ok(s) => Some(s),
                        Err(e) => {
                            search_error.set(Some(e));
                            is_searching.set(false);
                            return;
                        }
                    }
                };

                // Fulltext takes the snippet-aware path (decompresses
                // documents_text and highlights in Rust); filename +
                // duplicates go through the plain SQL executor.
                let svc1 = service.clone();
                let db1 = db_path.clone();
                let args_for_select = args.clone();
                let select_handle = tokio::task::spawn_blocking(move || {
                    if args_for_select.search_type == "fulltext" {
                        svc1.execute_fulltext_search(&db1, &args_for_select, PAGE_SIZE, offset)
                    } else {
                        let sql = precomputed_select_sql
                            .expect("non-fulltext select SQL was prebuilt above");
                        svc1.execute_search(&db1, &sql)
                    }
                });

                let count_handle = count_sql.map(|sql| {
                    let svc2 = service.clone();
                    let db2 = db_path.clone();
                    tokio::task::spawn_blocking(move || svc2.execute_search(&db2, &sql))
                });

                let select_run = select_handle.await;
                let count_run = match count_handle {
                    Some(h) => Some(h.await),
                    None => None,
                };

                let elapsed = start.elapsed().as_secs_f64();

                if let Some(c) = count_run {
                    match c {
                        Ok(Ok(rs)) => {
                            let n = rs
                                .first()
                                .and_then(|r| r.rows.first())
                                .and_then(|r| r.values.first())
                                .and_then(|s| s.parse::<u64>().ok())
                                .unwrap_or(0);
                            total_count.set(Some(n));
                        }
                        Ok(Err(e)) => {
                            handle_query_error(
                                e,
                                elapsed,
                                search_error,
                                show_corruption_dialog,
                                last_search_time,
                                is_searching,
                            );
                            return;
                        }
                        Err(e) => {
                            search_error.set(Some(format!("Task execution error: {}", e)));
                            last_search_time.set(Some(elapsed));
                            is_searching.set(false);
                            return;
                        }
                    }
                }

                match select_run {
                    Ok(Ok(rs)) => {
                        search_results.set(rs);
                        current_page.set(page);
                        last_args.set(Some(args));
                        last_search_time.set(Some(elapsed));
                    }
                    Ok(Err(e)) => {
                        handle_query_error(
                            e,
                            elapsed,
                            search_error,
                            show_corruption_dialog,
                            last_search_time,
                            is_searching,
                        );
                        return;
                    }
                    Err(e) => {
                        search_error.set(Some(format!("Task execution error: {}", e)));
                        last_search_time.set(Some(elapsed));
                    }
                }
                is_searching.set(false);
            });
        }
    };

    let do_fresh_search = {
        let run_query = run_query.clone();
        move || {
            let args = SearchArgs {
                search_type: search_type(),
                term: search_term(),
                fulltext_exact: fulltext_exact(),
                fulltext_case_sensitive: fulltext_case_sensitive(),
            };
            run_query(args, 1, true);
        }
    };

    let do_goto_page = {
        let run_query = run_query.clone();
        move |target: u32| {
            if let Some(args) = last_args() {
                let total = total_count()
                    .map(|n| pages_for(n))
                    .unwrap_or(1)
                    .max(1);
                let clamped = target.clamp(1, total);
                if clamped != current_page() {
                    run_query(args, clamped, false);
                }
            }
        }
    };

    let total_pages = total_count().map(pages_for).unwrap_or(0);

    rsx! {
        div {
            class: "section",
            h2 { "Search Database" }

            div {
                class: "form-group",
                label { "Search Type: " }
                select {
                    class: "form-control",
                    value: "{search_type}",
                    onchange: move |evt| search_type.set(evt.value()),
                    option { value: "fulltext", "Full Text Search" }
                    option { value: "filename", "Filename Search" }
                    option { value: "duplicates", "Find Duplicate Files" }
                }
            }

            if search_type() == "fulltext" {
                div {
                    class: "form-group",
                    style: "display: flex; flex-direction: column; gap: 6px;",
                    span { style: "font-weight: 600;", "Full text options" }
                    label {
                        style: "display: flex; align-items: center; gap: 8px; cursor: pointer;",
                        input {
                            r#type: "checkbox",
                            checked: fulltext_exact(),
                            onchange: move |evt| fulltext_exact.set(evt.checked()),
                        }
                        "Exact phrase match"
                    }
                    label {
                        style: "display: flex; align-items: center; gap: 8px; cursor: pointer;",
                        input {
                            r#type: "checkbox",
                            checked: fulltext_case_sensitive(),
                            onchange: move |evt| fulltext_case_sensitive.set(evt.checked()),
                        }
                        "Case-sensitive match"
                    }
                }
            }

            if search_type() != "duplicates" {
                div {
                    class: "form-group",
                    label { "Search Term: " }
                    input {
                        class: "form-control",
                        r#type: "text",
                        value: "{search_term}",
                        oninput: move |evt| search_term.set(evt.value()),
                        onkeydown: {
                            let do_fresh_search = do_fresh_search.clone();
                            move |evt| {
                                if evt.code() == dioxus::events::Code::Enter {
                                    do_fresh_search();
                                }
                            }
                        }
                    }
                }
            }

            div {
                style: "display: flex; align-items: center; gap: 10px;",
                button {
                    class: "btn btn-info",
                    disabled: is_searching(),
                    onclick: {
                        let do_fresh_search = do_fresh_search.clone();
                        move |_| { do_fresh_search(); }
                    },
                    "Search"
                }

                if is_searching() {
                    div {
                        class: "loading",
                        title: "Searching..."
                    }
                } else if let Some(elapsed) = last_search_time() {
                    span {
                        style: "color: #666; font-size: 0.9em;",
                        "Search completed in {elapsed:.3}s"
                    }
                }
            }

            if let Some(error) = search_error() {
                div {
                    class: "error-message",
                    "Error: {error}"
                }
            }

            // Results panel: pagination header + bounded table. Only renders
            // when at least one search has completed (last_args is Some).
            if last_args().is_some() {
                div {
                    class: "search-results",
                    {
                        let total_str = match total_count() {
                            Some(n) => format!("{}", n),
                            None => "?".to_string(),
                        };
                        let page_first = (current_page().saturating_sub(1) as u64) * PAGE_SIZE as u64 + 1;
                        let page_last_calc = page_first + search_results().first().map(|r| r.rows.len() as u64).unwrap_or(0).saturating_sub(1);
                        let header = if total_count() == Some(0) {
                            "No results.".to_string()
                        } else {
                            format!(
                                "Showing {}-{} of {} (page {} of {})",
                                page_first,
                                page_last_calc,
                                total_str,
                                current_page(),
                                total_pages
                            )
                        };
                        rsx! { h3 { "{header}" } }
                    }

                    // Pagination controls. Hidden if there's only one page.
                    if total_pages > 1 {
                        div {
                            style: "display: flex; align-items: center; gap: 8px; margin: 8px 0;",
                            button {
                                class: "btn",
                                disabled: is_searching() || current_page() <= 1,
                                onclick: {
                                    let do_goto_page = do_goto_page.clone();
                                    move |_| do_goto_page(1)
                                },
                                "« First"
                            }
                            button {
                                class: "btn",
                                disabled: is_searching() || current_page() <= 1,
                                onclick: {
                                    let do_goto_page = do_goto_page.clone();
                                    move |_| do_goto_page(current_page().saturating_sub(1))
                                },
                                "‹ Prev"
                            }
                            button {
                                class: "btn",
                                disabled: is_searching() || current_page() >= total_pages,
                                onclick: {
                                    let do_goto_page = do_goto_page.clone();
                                    move |_| do_goto_page(current_page().saturating_add(1))
                                },
                                "Next ›"
                            }
                            button {
                                class: "btn",
                                disabled: is_searching() || current_page() >= total_pages,
                                onclick: {
                                    let do_goto_page = do_goto_page.clone();
                                    move |_| do_goto_page(total_pages)
                                },
                                "Last »"
                            }
                            span { "Go to:" }
                            input {
                                r#type: "number",
                                style: "width: 70px;",
                                value: "{goto_input}",
                                oninput: move |evt| goto_input.set(evt.value()),
                                onkeydown: {
                                    let do_goto_page = do_goto_page.clone();
                                    move |evt| {
                                        if evt.code() == dioxus::events::Code::Enter {
                                            if let Ok(p) = goto_input().trim().parse::<u32>() {
                                                do_goto_page(p);
                                                goto_input.set(String::new());
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    if !search_results().is_empty() && !search_results()[0].rows.is_empty() {
                        div {
                            class: "results-table",
                            table {
                                thead {
                                    tr {
                                        for column in search_results()[0].columns.iter() {
                                            th { "{column}" }
                                        }
                                    }
                                }
                                tbody {
                                    for row in search_results()[0].rows.iter() {
                                        tr {
                                            for (col_index, value) in row.values.iter().enumerate() {
                                                if search_results()[0].columns.get(col_index).map(|s| s.as_str()) == Some("path") {
                                                    {
                                                        let value_owned = value.clone();
                                                        let service_owned = props.indexing_service.clone();
                                                        rsx! {
                                                            td {
                                                                class: "path-cell clickable",
                                                                onclick: move |_| {
                                                                    let path = value_owned.clone();
                                                                    let service_clone = service_owned.clone();
                                                                    spawn(async move {
                                                                        if let Err(e) = service_clone.open_file_explorer(&path) {
                                                                            eprintln!("Failed to open file explorer: {}", e);
                                                                        }
                                                                    });
                                                                },
                                                                title: "Click to open in file explorer",
                                                                dangerous_inner_html: "{value}"
                                                            }
                                                        }
                                                    }
                                                } else {
                                                    td {
                                                        dangerous_inner_html: "{value}"
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if show_corruption_dialog() {
                div {
                    class: "modal-backdrop",
                    div {
                        class: "modal-dialog",
                        h3 {
                            style: "margin-top: 0; color: #d32f2f;",
                            "⚠️ Database Corruption Detected"
                        }
                        p {
                            style: "margin: 15px 0;",
                            "The database appears to be corrupted or malformed. This can happen due to unexpected shutdowns or disk issues."
                        }
                        p {
                            style: "margin: 15px 0; font-weight: bold;",
                            "Would you like to delete the corrupted database and create a new one? This will require re-indexing your files."
                        }
                        div {
                            style: "display: flex; gap: 10px; margin-top: 20px;",
                            button {
                                style: "padding: 10px 20px; background-color: #d32f2f; color: white; border: none; border-radius: 5px; cursor: pointer;",
                                onclick: move |_| {
                                    let service = props.indexing_service.clone();
                                    let db = props.db_path.clone();

                                    show_corruption_dialog.set(false);
                                    search_error.set(Some("Deleting corrupted database...".to_string()));

                                    spawn(async move {
                                        match service.delete_index_for_rebuild(&db) {
                                            Ok(()) => {
                                                search_error.set(Some("Database deleted. You can now start indexing again.".to_string()));
                                            }
                                            Err(e) => {
                                                search_error.set(Some(format!("Error deleting database: {}", e)));
                                            }
                                        }
                                    });
                                },
                                "Yes, Delete & Rebuild"
                            }
                            button {
                                style: "padding: 10px 20px; background-color: #666; color: white; border: none; border-radius: 5px; cursor: pointer;",
                                onclick: move |_| {
                                    show_corruption_dialog.set(false);
                                },
                                "Cancel"
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Number of pages required to fit `total` rows at [`PAGE_SIZE`] per page.
/// Returns 0 for 0 rows so callers can branch on "no results yet".
fn pages_for(total: u64) -> u32 {
    if total == 0 {
        0
    } else {
        ((total - 1) / PAGE_SIZE as u64) as u32 + 1
    }
}

/// Centralized error-router for the two query branches that can fail
/// identically (count and select). Sets the error/timing/spinner signals
/// and pops the corruption dialog when warranted.
fn handle_query_error(
    e: String,
    elapsed: f64,
    mut search_error: Signal<Option<String>>,
    mut show_corruption_dialog: Signal<bool>,
    mut last_search_time: Signal<Option<f64>>,
    mut is_searching: Signal<bool>,
) {
    if e.starts_with("DATABASE_CORRUPTED:") {
        search_error.set(Some("Database appears to be corrupted".into()));
        show_corruption_dialog.set(true);
    } else {
        search_error.set(Some(e));
    }
    last_search_time.set(Some(elapsed));
    is_searching.set(false);
}
