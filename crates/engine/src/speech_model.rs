//! Whisper speech models for live captions on every system (plan item 7b, 30 Oct), fetched
//! the first time someone picks them rather than shipped with every install. Pinned to one
//! revision of ggerganov/whisper.cpp on Hugging Face and checked against its SHA-256 while it
//! downloads; kept in `<data>/models/`. Nothing is sent anywhere: the model runs on this
//! computer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::{Engine, EngineError};

/// The revision every model comes from (checked 30 Oct 2026).
const REVISION: &str = "5359861c739e955e79d9a303bcbc70fb988958b1";
const BASE: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve";

/// A model to offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Model {
    pub id: &'static str,
    pub label: &'static str,
    pub file: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
}

pub const MODELS: [Model; 2] = [
    Model {
        id: "base",
        label: "Fast",
        file: "ggml-base-q5_1.bin",
        bytes: 59_707_625,
        sha256: "422f1ae452ade6f30a004d7e5c6a43195e4433bc370bf23fac9cc591f01a8898",
    },
    Model {
        id: "small",
        label: "Better for Hindi and other languages",
        file: "ggml-small-q5_1.bin",
        bytes: 190_085_487,
        sha256: "ae85e4a935d7a567bd102fe55afc16bb595bdb618e11b2fc7591bc08120411bb",
    },
];

pub fn model(id: &str) -> Option<&'static Model> {
    MODELS.iter().find(|m| m.id == id)
}

/// Where a model is, or how its download goes.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ModelState {
    Ready,
    Missing,
    Downloading { percent: u8 },
    Failed { reason: String },
}

/// One model as the dashboard lists it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelInfo {
    pub id: &'static str,
    pub label: &'static str,
    pub mb: u64,
    #[serde(flatten)]
    pub state: ModelState,
}

/// Downloads in progress or failed, by model id (one engine per process).
static FETCH: Mutex<Option<HashMap<&'static str, ModelState>>> = Mutex::new(None);

fn fetch_state(id: &str) -> Option<ModelState> {
    FETCH.lock().ok().and_then(|m| m.as_ref().and_then(|m| m.get(id).cloned()))
}

fn set_fetch(id: &'static str, s: Option<ModelState>) {
    if let Ok(mut m) = FETCH.lock() {
        let m = m.get_or_insert_with(HashMap::new);
        match s {
            Some(s) => m.insert(id, s),
            None => m.remove(id),
        };
    }
}

/// Marks `id` as downloading unless it already is, in one step (a double click starts one
/// download). True when this call claimed it.
fn begin_fetch(id: &'static str) -> bool {
    let Ok(mut m) = FETCH.lock() else { return false };
    let m = m.get_or_insert_with(HashMap::new);
    if matches!(m.get(id), Some(ModelState::Downloading { .. })) {
        return false;
    }
    m.insert(id, ModelState::Downloading { percent: 0 });
    true
}

pub fn model_path(data: &Path, m: &Model) -> PathBuf {
    data.join("models").join(m.file)
}

/// Streams `url` into `dest` (beside it, renamed when whole), hashing as it goes: a file of
/// another size or checksum is thrown away. `progress` gets 0 to 100.
pub async fn download_model(m: &Model, dest: &Path, progress: impl Fn(u8) + Send) -> Result<(), String> {
    let url = format!("{BASE}/{REVISION}/{}", m.file);
    let http =
        reqwest::Client::builder().timeout(std::time::Duration::from_secs(1800)).build().map_err(|e| e.to_string())?;
    let mut res = http.get(&url).send().await.map_err(|e| format!("the download didn't start ({e})"))?;
    if !res.status().is_success() {
        return Err(format!("the download answered {}", res.status().as_u16()));
    }
    let dir = dest.parent().ok_or("no folder for the model")?;
    tokio::fs::create_dir_all(dir).await.map_err(|e| format!("couldn't make {}: {e}", dir.display()))?;
    let part = dest.with_extension("part");
    let mut file = tokio::fs::File::create(&part).await.map_err(|e| format!("couldn't save the model: {e}"))?;
    let (mut got, mut hash) = (0u64, Sha256::new());
    let result: Result<(), String> = async {
        while let Some(chunk) = res.chunk().await.map_err(|e| format!("the download stopped ({e})"))? {
            got += chunk.len() as u64;
            if got > m.bytes {
                return Err("the download is bigger than the model should be".into());
            }
            hash.update(&chunk);
            file.write_all(&chunk).await.map_err(|e| format!("couldn't save the model: {e}"))?;
            progress(((got * 100) / m.bytes).min(99) as u8);
        }
        file.flush().await.map_err(|e| format!("couldn't save the model: {e}"))?;
        let sum = hash.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>();
        if got != m.bytes || sum != m.sha256 {
            return Err("the downloaded file isn't the expected one (its checksum differs), so it wasn't used".into());
        }
        Ok(())
    }
    .await;
    drop(file);
    if let Err(e) = result {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(e);
    }
    tokio::fs::rename(&part, dest).await.map_err(|e| format!("couldn't save the model: {e}"))?;
    progress(100);
    Ok(())
}

impl Engine {
    fn models_dir(&self) -> Option<PathBuf> {
        self.golive_data_dir()
    }

    /// A ready model's file.
    pub fn speech_model_path(&self, id: &str) -> Option<PathBuf> {
        let m = model(id)?;
        let p = model_path(&self.models_dir()?, m);
        p.is_file().then_some(p)
    }

    pub fn speech_models(&self) -> Vec<ModelInfo> {
        MODELS
            .iter()
            .map(|m| ModelInfo {
                id: m.id,
                label: m.label,
                mb: m.bytes.div_ceil(1_000_000),
                state: if self.speech_model_path(m.id).is_some() {
                    ModelState::Ready
                } else {
                    fetch_state(m.id).unwrap_or(ModelState::Missing)
                },
            })
            .collect()
    }

    /// Starts fetching a model in the background (nothing when it's here or on its way).
    pub fn fetch_speech_model(&self, id: &str) -> Result<Vec<ModelInfo>, EngineError> {
        let m = model(id).ok_or(EngineError::Missing {
            code: "speech_model_not_found",
            message: "There's no speech model by that name.",
            hint: "Pick Fast or Better for Hindi.",
        })?;
        let data =
            self.models_dir().ok_or_else(|| EngineError::Invalid("No data folder to keep the model in.".into()))?;
        let dest = model_path(&data, m);
        if !dest.is_file() && begin_fetch(m.id) {
            tokio::spawn(async move {
                let id = m.id;
                match download_model(m, &dest, move |p| set_fetch(id, Some(ModelState::Downloading { percent: p })))
                    .await
                {
                    Ok(()) => {
                        tracing::info!(model = id, "speech model downloaded");
                        set_fetch(id, None);
                    }
                    Err(reason) => {
                        tracing::warn!(model = id, %reason, "speech model didn't download");
                        set_fetch(id, Some(ModelState::Failed { reason }));
                    }
                }
            });
        }
        Ok(self.speech_models())
    }

    /// Removes a downloaded model (to free the space).
    pub fn remove_speech_model(&self, id: &str) -> Result<Vec<ModelInfo>, EngineError> {
        let m = model(id).ok_or(EngineError::Missing {
            code: "speech_model_not_found",
            message: "There's no speech model by that name.",
            hint: "Pick Fast or Better for Hindi.",
        })?;
        if let Some(d) = self.models_dir() {
            let _ = std::fs::remove_file(model_path(&d, m));
        }
        set_fetch(m.id, None);
        Ok(self.speech_models())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_download_claims_a_model() {
        set_fetch("small", None);
        assert!(begin_fetch("small"));
        assert!(!begin_fetch("small"), "a second click doesn't start another");
        set_fetch("small", Some(ModelState::Failed { reason: "x".into() }));
        assert!(begin_fetch("small"), "a failed one can be tried again");
        set_fetch("small", None);
    }

    #[test]
    fn models_are_pinned_and_known() {
        assert!(model("base").is_some() && model("small").is_some());
        assert!(model("../etc").is_none() && model("").is_none());
        for m in MODELS {
            assert_eq!(m.sha256.len(), 64);
            assert!(m.file.ends_with(".bin") && !m.file.contains('/'));
        }
        assert_eq!(model_path(Path::new("/d"), model("base").unwrap()), Path::new("/d/models/ggml-base-q5_1.bin"));
    }
}
