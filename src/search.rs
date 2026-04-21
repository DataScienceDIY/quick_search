#![allow(non_snake_case)]

use std::sync::Arc;
use std::time::Instant;
use dioxus::prelude::*;
use crate::indexing::{IndexingService, SearchResult};

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
    let search_results = use_signal(|| Vec::<SearchResult>::new());
    let mut search_error = use_signal(|| None::<String>);
    let is_searching = use_signal(|| false);
    let last_search_time = use_signal(|| None::<f64>);
    let mut show_corruption_dialog = use_signal(|| false);
    
    let service = props.indexing_service.clone();
    let db_path = props.db_path.clone();
    
    // Create a callback to perform search
    let perform_search = {
        let service = service.clone();
        let db_path = db_path.clone();
        let search_type = search_type.clone();
        let search_term = search_term.clone();
        let fulltext_exact = fulltext_exact.clone();
        let fulltext_case_sensitive = fulltext_case_sensitive.clone();
        let search_results = search_results.clone();
        let search_error = search_error.clone();
        let is_searching = is_searching.clone();
        let last_search_time = last_search_time.clone();
        let show_corruption_dialog = show_corruption_dialog.clone();
        
        move || {
            let service_clone = service.clone();
            let db_clone = db_path.clone();
            let search_type_val = search_type().clone();
            let search_term_val = search_term().clone();
            let fulltext_exact_val = fulltext_exact();
            let fulltext_case_sensitive_val = fulltext_case_sensitive();
            
            let mut search_results_clone = search_results.clone();
            let mut search_error_clone = search_error.clone();
            let mut is_searching_clone = is_searching.clone();
            let mut last_search_time_clone = last_search_time.clone();
            let mut show_corruption_dialog_clone = show_corruption_dialog.clone();
            
            spawn(async move {
                is_searching_clone.set(true);
                search_error_clone.set(None);
                last_search_time_clone.set(None);
                let start_time = Instant::now();
                
                let query = match search_type_val.as_str() {
                    "fulltext" => {
                        let trimmed = search_term_val.trim();
                        if trimmed.is_empty() {
                            search_error_clone.set(Some("Please enter a search term".to_string()));
                            is_searching_clone.set(false);
                            return;
                        }

                        let sanitized_term = trimmed
                            .replace(':', " ")
                            .replace(';', " ")
                            .replace('(', " ")
                            .replace(')', " ")
                            .replace('[', " ")
                            .replace(']', " ")
                            .replace('{', " ")
                            .replace('}', " ")
                            .replace('^', " ")
                            .replace('~', " ")
                            .replace('"', " ");

                        let tokens: Vec<&str> = sanitized_term.split_whitespace().collect();
                        if tokens.is_empty() {
                            search_error_clone.set(Some("Please enter a valid search term".to_string()));
                            is_searching_clone.set(false);
                            return;
                        }

                        let words: Vec<&str> = if fulltext_exact_val {
                            tokens
                        } else {
                            let filtered: Vec<&str> = tokens
                                .into_iter()
                                .filter(|w| w.chars().count() >= 3)
                                .collect();
                            if filtered.is_empty() {
                                search_error_clone.set(Some(
                                    "Trigram index needs each word to be at least 3 characters unless you use exact phrase search.".to_string(),
                                ));
                                is_searching_clone.set(false);
                                return;
                            }
                            filtered
                        };

                        let sql_quote = |s: &str| s.replace('\'', "''");

                        let fts_match = if fulltext_exact_val {
                            let phrase = words.join(" ");
                            format!("\"{}\"", phrase.replace('"', "\"\""))
                        } else {
                            words.join(" AND ")
                        };

                        let mut where_clause = format!("st.text MATCH '{}'", sql_quote(&fts_match));
                        if fulltext_case_sensitive_val {
                            if fulltext_exact_val {
                                let literal = words.join(" ");
                                where_clause.push_str(&format!(
                                    " AND instr(st.text, '{}') > 0",
                                    sql_quote(&literal)
                                ));
                            } else {
                                for w in &words {
                                    where_clause.push_str(&format!(
                                        " AND instr(st.text, '{}') > 0",
                                        sql_quote(w)
                                    ));
                                }
                            }
                        }

                        format!(
                            "SELECT d.name, d.path, snippet(st, 1, '<b>', '</b>', '<b>...</b>', 64) as snippet FROM searchabletext AS st JOIN documents d ON d.id = st.rowid WHERE {} ORDER BY rank",
                            where_clause
                        )
                    },
                    "filename" => {
                        if search_term_val.trim().is_empty() {
                            search_error_clone.set(Some("Please enter a filename pattern".to_string()));
                            is_searching_clone.set(false);
                            return;
                        }
                        format!("SELECT name, path FROM files WHERE name LIKE '%{}%'", search_term_val.replace("'", "''"))
                    },
                    "duplicates" => "SELECT name, count(*) as cnt, path FROM files GROUP BY hash HAVING cnt > 1 ORDER BY cnt DESC".to_string(),
                    _ => {
                        is_searching_clone.set(false);
                        return;
                    }
                };
                
                // Run the search in a blocking task to prevent UI freezing
                let search_result = tokio::task::spawn_blocking(move || {
                    service_clone.execute_search(&db_clone, &query)
                }).await;
                
                let elapsed = start_time.elapsed().as_secs_f64();
                match search_result {
                    Ok(db_result) => {
                        match db_result {
                            Ok(results) => {
                                search_results_clone.set(results);
                                last_search_time_clone.set(Some(elapsed));
                        },
                        Err(e) => {
                            if e.starts_with("DATABASE_CORRUPTED:") {
                                search_error_clone.set(Some("Database appears to be corrupted".to_string()));
                                show_corruption_dialog_clone.set(true);
                            } else {
                                search_error_clone.set(Some(e));
                            }
                            last_search_time_clone.set(Some(elapsed));
                        }
                        }
                    },
                    Err(e) => {
                        search_error_clone.set(Some(format!("Task execution error: {}", e)));
                        last_search_time_clone.set(Some(elapsed));
                    }
                }
                is_searching_clone.set(false);
            });
        }
    };
    
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
                            let perform_search = perform_search.clone();
                            move |evt| {
                                if evt.code() == dioxus::events::Code::Enter {
                                    perform_search();
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
                        let perform_search = perform_search.clone();
                        move |_| {
                            perform_search();
                        }
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
            
            if !search_results().is_empty() {
                div {
                    class: "search-results",
                    h3 { "Search Results ({search_results()[0].rows.len()} results)" }
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
                                for (_i, row) in search_results()[0].rows.iter().enumerate() {
                                    tr {
                                        for (col_index, value) in row.values.iter().enumerate() {
                                            // Check if this column is a path column
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
            
            // Database corruption recovery dialog
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
