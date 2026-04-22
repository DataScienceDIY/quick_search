use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    pub paths: PathConfig,
    pub processing: ProcessingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PathConfig {
    /// One or more directory roots to index. Indexing walks each root
    /// independently; duplicates and nested roots are de-duplicated by the
    /// indexer at run time. Must contain at least one entry.
    pub indexing_paths: Vec<String>,
    pub database_path: String,
}

fn default_fts_update_batch_size() -> usize {
    1000
}

/// Platform-sensible default for the first indexing root when no config
/// exists. `$HOME` on Unix, `%USERPROFILE%` on Windows; falls back to the
/// current directory.
fn default_home_path() -> String {
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        return home.to_string_lossy().into_owned();
    }
    ".".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProcessingConfig {
    pub hash_length: usize,
    pub maximum_text_size: usize,
    pub maximum_text_file_size: u64,
    pub batch_size: usize,
    #[serde(default = "default_fts_update_batch_size")]
    pub fts_update_batch_size: usize,
    pub tokenize: String,
    #[serde(default)]
    pub precount_files_for_progress: bool,
    #[serde(default)]
    pub follow_symlinks: bool,
    #[serde(default)]
    pub include_hidden: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            paths: PathConfig {
                indexing_paths: vec![default_home_path()],
                database_path: "QuickSearch.db".to_string(),
            },
            processing: ProcessingConfig {
                hash_length: 1024 * 8,
                maximum_text_size: 1024 * 256,
                maximum_text_file_size: 1024 * 1024 * 2,
                batch_size: 200,
                fts_update_batch_size: 1000,
                tokenize: "trigram".to_string(),
                precount_files_for_progress: false,
                follow_symlinks: false,
                include_hidden: false,
            },
        }
    }
}

impl Config {
    pub fn load() -> Result<Self, String> {
        let config_path = "config.toml";
        
        if Path::new(config_path).exists() {
            let content = fs::read_to_string(config_path)
                .map_err(|e| format!("Failed to read config file: {}", e))?;
            
            toml::from_str(&content)
                .map_err(|e| format!("Failed to parse config file: {}", e))
        } else {
            let default_config = Config::default();
            default_config.save()?;
            Ok(default_config)
        }
    }
    
    pub fn save(&self) -> Result<(), String> {
        let content = toml::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize config: {}", e))?;
        
        fs::write("config.toml", content)
            .map_err(|e| format!("Failed to write config file: {}", e))?;
        
        Ok(())
    }
}
