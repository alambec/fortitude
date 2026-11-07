//! Remote application entities (modalities, PACS, workstations) registered at runtime, kept
//! in the index database so that they survive restarts and are shared by the servers of a
//! PostgreSQL index.
//!
//! They are identified by their AE title. Validation is left to the caller.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::Store;
use crate::index::{IndexError, Value};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Modality {
    pub ae_title: String,
    pub host: String,
    pub port: u16,
    /// Free text, for people (e.g. "CT, room 3").
    pub description: String,
}

impl Store {
    /// The registered modalities, by AE title.
    pub async fn modalities(self: &Arc<Self>) -> Result<Vec<Modality>, IndexError> {
        self.index()
            .readers()
            .run(|conn| {
                conn.query(
                    "SELECT ae_title, host, port, description FROM modalities ORDER BY ae_title",
                    &[],
                    |r| {
                        Ok(Modality {
                            ae_title: r.str(0)?.to_owned(),
                            host: r.str(1)?.to_owned(),
                            port: u16::try_from(r.i64(2)?).unwrap_or_default(),
                            description: r.str(3)?.to_owned(),
                        })
                    },
                )
            })
            .await
    }

    /// Register a modality, or update the one with the same AE title; returns whether it
    /// is new.
    pub async fn put_modality(self: &Arc<Self>, modality: Modality) -> Result<bool, IndexError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        let inserted = self
            .index()
            .write(move |conn| {
                let params = [
                    Value::Text(modality.host),
                    Value::Integer(i64::from(modality.port)),
                    Value::Text(modality.description),
                    Value::Text(modality.ae_title),
                ];
                let updated = conn.execute(
                    "UPDATE modalities SET host = ?, port = ?, description = ? WHERE ae_title = ?",
                    &params,
                )?;
                if updated > 0 {
                    return Ok(0);
                }
                let [host, port, description, ae_title] = params;
                conn.execute(
                    "INSERT INTO modalities (ae_title, host, port, description, created_at)
                     VALUES (?, ?, ?, ?, ?)",
                    &[ae_title, host, port, description, Value::Integer(now)],
                )
            })
            .await?;
        Ok(inserted > 0)
    }

    /// Remove a registered modality; returns whether it was registered.
    pub async fn delete_modality(self: &Arc<Self>, ae_title: &str) -> Result<bool, IndexError> {
        let ae_title = Value::Text(ae_title.to_owned());
        let deleted = self
            .index()
            .write(move |conn| {
                conn.execute("DELETE FROM modalities WHERE ae_title = ?", &[ae_title])
            })
            .await?;
        Ok(deleted > 0)
    }
}
