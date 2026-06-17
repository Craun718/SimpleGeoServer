use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSection {
    pub port: Option<u16>,
    pub address: Option<String>,
    pub threads: Option<u32>,
    pub root: Option<String>,
    pub cache_max_age: Option<i32>,
    pub cors: Option<bool>,
    pub gzip: Option<bool>,
    pub no_dotfiles: Option<bool>,
    pub log_format: Option<String>,
}

impl Default for ServerSection {
    fn default() -> Self {
        Self {
            port: None,
            address: None,
            threads: None,
            root: None,
            cache_max_age: None,
            cors: None,
            gzip: None,
            no_dotfiles: None,
            log_format: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceEntry {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheSection {
    pub l2_size_mb: Option<u64>,
    pub disk_dir: Option<String>,
}

impl Default for CacheSection {
    fn default() -> Self {
        Self {
            l2_size_mb: Some(512),
            disk_dir: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub server: Option<ServerSection>,
    #[serde(default)]
    pub sources: Option<Vec<SourceEntry>>,
    #[serde(default)]
    pub cache: Option<CacheSection>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server: None,
            sources: None,
            cache: Some(CacheSection::default()),
        }
    }
}

pub fn load_config(path: &Path) -> Result<AppConfig, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read config file {}: {e}", path.display()))?;
    serde_yaml::from_str(&content)
        .map_err(|e| format!("Failed to parse config file {}: {e}", path.display()))
}

pub fn generate_default_config(path: &Path) -> Result<(), String> {
    let config = AppConfig {
        server: Some(ServerSection {
            port: Some(8080),
            address: Some("0.0.0.0".to_string()),
            threads: Some(4),
            root: Some(".".to_string()),
            cache_max_age: Some(3600),
            cors: Some(false),
            gzip: Some(false),
            no_dotfiles: Some(false),
            log_format: Some("default".to_string()),
        }),
        sources: Some(vec![
            SourceEntry {
                name: "example-raster".to_string(),
                path: "./data/raster.tif".to_string(),
            },
            SourceEntry {
                name: "example-vector".to_string(),
                path: "./data/vector.geojson".to_string(),
            },
        ]),
        cache: Some(CacheSection {
            l2_size_mb: Some(512),
            disk_dir: Some("./cache/tiles".to_string()),
        }),
    };

    let yaml =
        serde_yaml::to_string(&config).map_err(|e| format!("Failed to serialize config: {e}"))?;

    std::fs::write(path, &yaml)
        .map_err(|e| format!("Failed to write config file {}: {e}", path.display()))?;

    tracing::info!("Generated default config: {}", path.display());
    Ok(())
}
