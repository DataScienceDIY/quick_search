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
    pub default_indexing_path: String,
    pub database_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProcessingConfig {
    pub hash_length: usize,
    pub maximum_text_size: usize,
    pub maximum_file_size: u64,
    pub batch_size: usize,
    pub tokenize: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            paths: PathConfig {
                default_indexing_path: "C:\\".to_string(),
                database_path: "QuickSearch.db".to_string(),
            },
            processing: ProcessingConfig {
                hash_length: 1024 * 8,
                maximum_text_size: 1024 * 512,
                maximum_file_size: 1024 * 1024 * 50,
                batch_size: 200,
                tokenize: "trigram".to_string(),
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
