use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tracing::debug;

use kaos::{Kaos, KaosPath, LocalKaos, get_current_kaos};

use crate::share::{ensure_share_dir, get_share_dir};

pub(crate) fn normalize_path_string(s: &str) -> String {
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s.to_string()
    }
}

pub fn get_metadata_file() -> PathBuf {
    get_share_dir().join("kimi.json")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkDirMeta {
    pub path: String,
    #[serde(default = "default_kaos_name")]
    pub kaos: String,
    #[serde(default)]
    pub last_session_id: Option<String>,
}

impl WorkDirMeta {
    pub fn sessions_dir(&self) -> PathBuf {
        let normalized = normalize_path_string(&self.path);
        let hash = md5::compute(normalized.as_bytes());
        let hash_hex = format!("{:x}", hash);
        let dir_basename = if self.kaos == default_kaos_name() {
            hash_hex
        } else {
            format!("{}_{}", self.kaos, hash_hex)
        };
        get_share_dir().join("sessions").join(dir_basename)
    }

    pub async fn ensure_sessions_dir(&self) -> PathBuf {
        let dir = self.sessions_dir();
        tokio::fs::create_dir_all(&dir)
            .await
            .unwrap_or_else(|err| panic!("Failed to create sessions dir {}: {err}", dir.display()));
        dir
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Metadata {
    #[serde(default)]
    pub work_dirs: Vec<WorkDirMeta>,
}

impl Metadata {
    pub fn get_work_dir_meta(&self, path: &KaosPath) -> Option<WorkDirMeta> {
        let kaos_name = get_current_kaos().name().to_string();
        let normalized_input = normalize_path_string(&path.to_string());
        self.work_dirs
            .iter()
            .find(|wd| normalize_path_string(&wd.path) == normalized_input && wd.kaos == kaos_name)
            .cloned()
    }

    pub fn new_work_dir_meta(&mut self, path: &KaosPath) -> WorkDirMeta {
        let normalized = normalize_path_string(&path.to_string());
        let meta = WorkDirMeta {
            path: normalized,
            kaos: get_current_kaos().name().to_string(),
            last_session_id: None,
        };
        self.work_dirs.push(meta.clone());
        meta
    }
}

pub async fn load_metadata() -> Metadata {
    let _ = ensure_share_dir().await;
    let metadata_file = get_metadata_file();
    debug!("Loading metadata from file: {}", metadata_file.display());
    if tokio::fs::metadata(&metadata_file).await.is_err() {
        debug!("No metadata file found, creating empty metadata");
        return Metadata::default();
    }
    let text = tokio::fs::read_to_string(&metadata_file)
        .await
        .unwrap_or_else(|err| {
            panic!(
                "Failed to read metadata file {}: {err}",
                metadata_file.display()
            )
        });
    let mut metadata: Metadata = serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("Invalid metadata file {}: {err}", metadata_file.display()));
    // Normalize all stored paths to lowercase on Windows to avoid case-
    // sensitive duplication of the same physical directory.
    // When the path changes case, rename the on-disk sessions directory so
    // existing sessions are not orphaned.
    if cfg!(windows) {
        let sessions_root = get_share_dir().join("sessions");
        let mut seen: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let mut deduped: Vec<WorkDirMeta> = Vec::new();
        for wd in metadata.work_dirs.into_iter() {
            let normalized = normalize_path_string(&wd.path);
            // If the path changed case, migrate the old sessions directory.
            if wd.path != normalized {
                let old_hash = format!("{:x}", md5::compute(wd.path.as_bytes()));
                let new_hash = format!("{:x}", md5::compute(normalized.as_bytes()));
                if old_hash != new_hash {
                    let old_dir = if wd.kaos == default_kaos_name() {
                        sessions_root.join(&old_hash)
                    } else {
                        sessions_root.join(format!("{}_{}", wd.kaos, old_hash))
                    };
                    let new_dir = if wd.kaos == default_kaos_name() {
                        sessions_root.join(&new_hash)
                    } else {
                        sessions_root.join(format!("{}_{}", wd.kaos, new_hash))
                    };
                    if old_dir.exists() && !new_dir.exists() {
                        if let Err(e) = std::fs::rename(&old_dir, &new_dir) {
                            tracing::warn!(
                                "Failed to migrate sessions dir {} -> {}: {e}",
                                old_dir.display(), new_dir.display()
                            );
                        } else {
                            tracing::info!(
                                "Migrated sessions dir {} -> {}",
                                old_dir.display(), new_dir.display()
                            );
                        }
                    }
                }
            }
            if let Some(prev_idx) = seen.get(&normalized) {
                // Merge: keep the entry with a last_session_id if possible.
                let prev = &mut deduped[*prev_idx];
                if prev.last_session_id.is_none() && wd.last_session_id.is_some() {
                    prev.last_session_id = wd.last_session_id;
                }
            } else {
                seen.insert(normalized.clone(), deduped.len());
                deduped.push(WorkDirMeta {
                    path: normalized,
                    kaos: wd.kaos,
                    last_session_id: wd.last_session_id,
                });
            }
        }
        metadata.work_dirs = deduped;
    }
    metadata
}

pub async fn save_metadata(metadata: &Metadata) {
    let metadata_file = get_metadata_file();
    debug!("Saving metadata to file: {}", metadata_file.display());
    if let Some(parent) = metadata_file.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .unwrap_or_else(|err| {
                panic!("Failed to create metadata dir {}: {err}", parent.display())
            });
    }
    let text = serde_json::to_string_pretty(metadata).unwrap_or_else(|err| {
        panic!(
            "Failed to serialize metadata file {}: {err}",
            metadata_file.display()
        )
    });
    tokio::fs::write(&metadata_file, text)
        .await
        .unwrap_or_else(|err| {
            panic!(
                "Failed to write metadata file {}: {err}",
                metadata_file.display()
            )
        });
}

fn default_kaos_name() -> String {
    LocalKaos::new().name().to_string()
}
