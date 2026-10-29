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
    pub dimse: DimseConfig,
    pub auth: AuthConfig,
    pub metrics: MetricsConfig,
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

/// DIMSE service (C-ECHO, C-STORE, C-FIND, C-MOVE as an SCP). It has no authentication
/// beyond AE titles: restrict access to it at the network level.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DimseConfig {
    /// Address of the DIMSE service; the service is disabled when unset.
    pub listen: Option<SocketAddr>,
    pub ae_title: String,
    /// Reject associations whose called AE title is not `ae_title`.
    pub check_called_ae_title: bool,
    /// Accept associations from AE titles that are not listed in `peers`.
    pub allow_unknown_peers: bool,
    /// Concurrent associations; more are rejected.
    pub max_associations: usize,
    /// Largest PDU accepted, in bytes.
    pub max_pdu_length: u32,
    /// Time allowed for each network read or write, and to connect to a C-MOVE destination.
    pub timeout_secs: u64,
    /// Maximum number of C-FIND matches (0 for no limit).
    pub max_find_results: usize,
    /// Storage SOP classes accepted besides the standard ones (e.g. private classes).
    pub extra_sop_classes: Vec<String>,
    /// Known application entities: C-MOVE destinations, and the only peers allowed to
    /// associate when `allow_unknown_peers` is off.
    pub peers: Vec<Peer>,
}

impl Default for DimseConfig {
    fn default() -> Self {
        let defaults = fortitude_dimse::Config::default();
        Self {
            listen: None,
            ae_title: defaults.ae_title,
            check_called_ae_title: defaults.check_called_ae_title,
            allow_unknown_peers: defaults.allow_unknown_peers,
            max_associations: defaults.max_associations,
            max_pdu_length: defaults.max_pdu_length,
            timeout_secs: defaults.timeout.as_secs(),
            max_find_results: defaults.max_find_results,
            extra_sop_classes: defaults.extra_sop_classes,
            peers: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Peer {
    pub ae_title: String,
    pub host: String,
    pub port: u16,
}

impl DimseConfig {
    /// Service configuration; `max_instance_size` bounds received data sets.
    pub fn service(&self, max_instance_size: usize) -> Result<fortitude_dimse::Config, String> {
        let valid_ae = |ae: &str| {
            !ae.trim().is_empty()
                && ae.len() <= 16
                && ae.bytes().all(|b| b.is_ascii_graphic() || b == b' ')
                && !ae.contains('\\')
        };
        for ae in std::iter::once(&self.ae_title).chain(self.peers.iter().map(|p| &p.ae_title)) {
            if !valid_ae(ae) {
                return Err(format!(
                    "invalid AE title {ae:?} (1 to 16 ASCII characters)"
                ));
            }
        }
        Ok(fortitude_dimse::Config {
            ae_title: self.ae_title.clone(),
            check_called_ae_title: self.check_called_ae_title,
            allow_unknown_peers: self.allow_unknown_peers,
            peers: self
                .peers
                .iter()
                .map(|p| fortitude_dimse::Peer {
                    ae_title: p.ae_title.clone(),
                    host: p.host.clone(),
                    port: p.port,
                })
                .collect(),
            max_associations: self.max_associations.max(1),
            max_pdu_length: self.max_pdu_length,
            timeout: std::time::Duration::from_secs(self.timeout_secs.max(1)),
            max_find_results: self.max_find_results,
            max_instance_size,
            extra_sop_classes: self.extra_sop_classes.clone(),
        })
    }
}

/// Credentials accepted by the server. With none, authentication is disabled; `/health` and
/// `/metrics` never require it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    /// HTTP Basic credentials.
    pub users: Vec<User>,
    /// HTTP Bearer tokens.
    pub tokens: Vec<Token>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct User {
    pub name: String,
    pub password: String,
    /// Only `GET`, `HEAD` and `OPTIONS` requests are allowed.
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Token {
    pub token: String,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// Expose Prometheus metrics on `/metrics`.
    pub enabled: bool,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self { enabled: true }
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
    fn credentials_from_file() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "fortitude.toml",
                r#"
                [[auth.users]]
                name = "admin"
                password = "s3cret"
                [[auth.tokens]]
                token = "abc"
                read_only = true
                "#,
            )?;
            let config = Config::load(Some(Path::new("fortitude.toml"))).map_err(|e| *e)?;
            assert_eq!(config.auth.users[0].name, "admin");
            assert!(!config.auth.users[0].read_only);
            assert!(config.auth.tokens[0].read_only);
            assert!(config.metrics.enabled);
            Ok(())
        });
    }

    #[test]
    fn dimse_peers() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "fortitude.toml",
                r#"
                [dimse]
                listen = "0.0.0.0:4242"
                [[dimse.peers]]
                ae_title = "VIEWER"
                host = "10.0.0.5"
                port = 104
                "#,
            )?;
            let config = Config::load(Some(Path::new("fortitude.toml"))).map_err(|e| *e)?;
            assert_eq!(config.dimse.listen, Some("0.0.0.0:4242".parse().unwrap()));
            let service = config.dimse.service(1 << 20).unwrap();
            assert_eq!(service.ae_title, "FORTITUDE");
            assert_eq!(service.peers[0].port, 104);

            let mut invalid = config.dimse.clone();
            invalid.ae_title = "A_VERY_LONG_AE_TITLE".into();
            assert!(invalid.service(1 << 20).is_err());
            assert_eq!(Config::default().dimse.listen, None);
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
