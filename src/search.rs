#![allow(non_snake_case)]

use std::sync::Arc;
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
    let mut search_results = use_signal(|| Vec::<SearchResult>::new());
    let mut search_error = use_signal(|| None::<String>);
    
    let service = props.indexing_service.clone();
    let db_path = props.db_path.clone();
    
    rsx! {
        div {
            style: "margin-top: 30px;",
            h2 { "Search Database" }
            
            div { 
                style: "margin-bottom: 10px;",
                label { "Search Type: " }
                select { 
                    value: "{search_type}",
                    onchange: move |evt| search_type.set(evt.value()),
                    option { value: "fulltext", "Full Text Search" }
                    option { value: "filename", "Filename Search" }
                    option { value: "duplicates", "Find Duplicate Files" }
                }
            }
            
            if search_type() != "duplicates" {
                div { 
                    style: "margin-bottom: 10px;",
                    label { "Search Term: " }
                    input { 
                        r#type: "text",
                        value: "{search_term}",
                        oninput: move |evt| search_term.set(evt.value())
                    }
                }
            }
            
            button { 
                style: "padding: 10px 20px; background-color: #2196F3; color: white; border: none; cursor: pointer;",
                onclick: move |_| {
                    let service_clone = service.clone();
                    let db_clone = db_path.clone();
                    let search_type_val = search_type().clone();
                    let search_term_val = search_term().clone();
                    
                    let mut search_results_clone = search_results.clone();
                    let mut search_error_clone = search_error.clone();
                    
                    spawn(async move {
                        search_error_clone.set(None);
                        
                        let query = match search_type_val.as_str() {
                            "fulltext" => {
                                if search_term_val.trim().is_empty() {
                                    search_error_clone.set(Some("Please enter a search term".to_string()));
                                    return;
                                }
                                format!("SELECT name, path, snippet(searchabletext, 2, '<b>', '</b>', '<b>...</b>', 64) as snippet FROM searchabletext WHERE text MATCH '{}'", search_term_val.replace("'", "''"))
                            },
                            "filename" => {
                                if search_term_val.trim().is_empty() {
                                    search_error_clone.set(Some("Please enter a filename pattern".to_string()));
                                    return;
                                }
                                format!("SELECT name, path FROM files WHERE name LIKE '%{}%'", search_term_val.replace("'", "''"))
                            },
                            "duplicates" => "SELECT name, count(*) as cnt, path FROM files GROUP BY hash HAVING cnt > 1 ORDER BY cnt DESC".to_string(),
                            _ => return
                        };
                        
                        match service_clone.execute_search(&db_clone, &query) {
                            Ok(results) => search_results_clone.set(results),
                            Err(e) => search_error_clone.set(Some(e))
                        }
                    });
                },
                "Search"
            }
            
            if let Some(error) = search_error() {
                div {
                    style: "color: red; margin-top: 10px;",
                    "Error: {error}"
                }
            }
            
            if !search_results().is_empty() {
                div {
                    style: "margin-top: 20px;",
                    h3 { "Search Results ({search_results()[0].rows.len()} rows)" }
                    div {
                        style: "max-height: 400px; overflow: auto; border: 1px solid #ddd;",
                        table {
                            style: "width: 100%; border-collapse: collapse; font-size: 12px;",
                            thead {
                                style: "background-color: #f5f5f5; position: sticky; top: 0;",
                                tr {
                                    for column in search_results()[0].columns.iter() {
                                        th {
                                            style: "padding: 8px; border: 1px solid #ddd; text-align: left;",
                                            "{column}"
                                        }
                                    }
                                }
                            }
                            tbody {
                                for (i, row) in search_results()[0].rows.iter().enumerate() {
                                    tr {
                                        style: if i % 2 == 0 { "background-color: #f9f9f9;" } else { "" },
                                        for value in row.values.iter() {
                                            td {
                                                style: "padding: 8px; border: 1px solid #ddd; word-break: break-all;",
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
}
