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
    pub index: IndexConfig,
    pub dicomweb: DicomWebConfig,
    pub log: LogConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpConfig {
    pub listen: SocketAddr,
    /// Path under which the DICOMweb services are mounted.
    pub dicomweb_root: String,
    /// Path of the WADO-URI service.
    pub wado_root: String,
    /// Public origin used in returned URLs (e.g. `https://pacs.example.org`); derived from
    /// the `Host` and `X-Forwarded-*` request headers when unset.
    pub public_url: Option<String>,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], 8042)),
            dicomweb_root: "/dicom-web".to_owned(),
            wado_root: "/wado".to_owned(),
            public_url: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    /// Directory holding the DICOM files and the index database.
    pub path: PathBuf,
    /// Flush every received file to disk before acknowledging it. Safer on power loss,
    /// much slower on most disks.
    pub fsync: bool,
    /// Binary attributes up to this size (bytes) are inlined in metadata responses; larger
    /// ones are referenced with a BulkDataURI.
    pub inline_binary_max: usize,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from("./data"),
            fsync: false,
            inline_binary_max: 1024,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IndexConfig {
    /// Read connections to the index; 0 means one per CPU.
    pub readers: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DicomWebConfig {
    /// Instances of one STOW-RS request processed concurrently.
    pub stow_concurrency: usize,
    /// Largest accepted instance, in MiB.
    pub max_instance_size_mib: usize,
    /// Maximum number of QIDO-RS results per request (0 for no limit). Clients can page
    /// through larger result sets with `offset`.
    pub qido_max_results: usize,
}

impl Default for DicomWebConfig {
    fn default() -> Self {
        Self {
            stow_concurrency: 4,
            max_instance_size_mib: 4096,
            qido_max_results: 10_000,
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
