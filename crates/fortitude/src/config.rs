use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml};
use serde::{Deserialize, Serialize};

/// Server configuration, layered as: built-in defaults, then the TOML file (if any), then
/// `FORTITUDE_*` environment variables, where `__` separates sections
/// (e.g. `FORTITUDE_HTTP__LISTEN=0.0.0.0:8042`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub http: HttpConfig,
    pub storage: StorageConfig,
    pub log: LogConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpConfig {
    pub listen: SocketAddr,
    /// Path under which the DICOMweb services are mounted.
    pub dicomweb_root: String,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], 8042)),
            dicomweb_root: "/dicom-web".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    /// Directory holding the DICOM files and the index database.
    pub path: PathBuf,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from("./data"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    /// `tracing` filter directive, overridden by `RUST_LOG` when set.
    pub filter: String,
    pub format: LogFormat,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            filter: "info".to_owned(),
            format: LogFormat::Text,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Text,
    Json,
}

impl Config {
    pub fn load(file: Option<&Path>) -> Result<Self, Box<figment::Error>> {
        let mut figment = Figment::from(Serialized::defaults(Config::default()));
        if let Some(file) = file {
            figment = figment.merge(Toml::file_exact(file));
        }
        figment
            .merge(Env::prefixed("FORTITUDE_").split("__"))
            .extract()
            .map_err(Box::new)
    }
}

#[cfg(test)]
// `figment::Jail` closures must return `figment::Result`, whose error is large.
#[allow(clippy::result_large_err)]
mod tests {
    use super::*;

    #[test]
    fn env_overrides_file_which_overrides_defaults() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "fortitude.toml",
                r#"
                [http]
                listen = "127.0.0.1:9000"
                [storage]
                path = "/srv/fortitude"
                "#,
            )?;
            jail.set_env("FORTITUDE_HTTP__LISTEN", "127.0.0.1:9001");

            let config = Config::load(Some(Path::new("fortitude.toml"))).map_err(|e| *e)?;
            assert_eq!(config.http.listen, "127.0.0.1:9001".parse().unwrap());
            assert_eq!(config.storage.path, PathBuf::from("/srv/fortitude"));
            assert_eq!(config.http.dicomweb_root, "/dicom-web");
            Ok(())
        });
    }

    #[test]
    fn unknown_keys_are_rejected() {
        figment::Jail::expect_with(|jail| {
            jail.create_file("fortitude.toml", "[http]\nlisten_on = \"0.0.0.0:1\"\n")?;
            assert!(Config::load(Some(Path::new("fortitude.toml"))).is_err());
            Ok(())
        });
    }
}
