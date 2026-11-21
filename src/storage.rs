use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::gpu::Intensity;
use crate::mining::{self, Hash256};

pub const FILE_NAME: &str = "bitcoin-detailed.json";
pub type SetupForm = [String; 5];
const DEFAULT_POOL: &str = "stratum.ckpool.org:3333";
const LEGACY_BEST: &str = ".bitcoin-detailed-best.json";

#[derive(Clone)]
pub struct Best {
    pub hash: Hash256,
    pub header: [u8; 80],
    pub job: String,
    pub nonce2: String,
    pub recorded_at: Option<OffsetDateTime>,
}

impl Best {
    fn from_json(record: &Value) -> Result<Self> {
        let hash: Hash256 =
            mining::hex_array(record["hash"].as_str().context("Missing best hash")?)?;
        let header = mining::hex_array(record["header"].as_str().context("Missing best header")?)?;
        let mut computed = mining::hash256(&header);
        computed.reverse();
        ensure!(hash == computed, "The saved hash does not match its header");
        Ok(Self {
            hash,
            header,
            job: record["job"].as_str().unwrap_or_default().to_owned(),
            nonce2: record["extranonce2"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            recorded_at: record["recorded_at"]
                .as_str()
                .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok()),
        })
    }

    fn to_json(&self) -> Result<Value> {
        let recorded_at = self
            .recorded_at
            .map(|timestamp| timestamp.format(&Rfc3339))
            .transpose()?;
        Ok(json!({
            "hash": hex::encode(self.hash),
            "header": hex::encode(self.header),
            "job": self.job,
            "extranonce2": self.nonce2,
            "recorded_at": recorded_at,
        }))
    }
}

pub struct SavedState {
    pub form: SetupForm,
    pub best: Option<Best>,
    pub gpu_intensity: Intensity,
}

impl Default for SavedState {
    fn default() -> Self {
        Self {
            form: [
                DEFAULT_POOL.into(),
                String::new(),
                "tui".into(),
                "x".into(),
                "1".into(),
            ],
            best: None,
            gpu_intensity: Intensity::default(),
        }
    }
}

#[derive(Clone)]
pub struct Storage {
    path: PathBuf,
    legacy_settings: Option<PathBuf>,
    legacy_best: Vec<PathBuf>,
}

impl Storage {
    pub fn for_current_exe() -> Result<Self> {
        let executable = std::env::current_exe().context("Could not locate the executable")?;
        let directory = executable
            .parent()
            .context("Executable has no parent directory")?;
        let working_directory = std::env::current_dir().ok();
        let legacy_settings = dirs::config_dir()
            .map(|path| path.join("bitcoin-detailed").join("settings.json"))
            .or_else(|| {
                working_directory
                    .as_ref()
                    .map(|path| path.join(".bitcoin-detailed-settings.json"))
            });
        let mut legacy_best = vec![directory.join(LEGACY_BEST)];
        if let Some(working_directory) = &working_directory
            && working_directory != directory
        {
            legacy_best.push(working_directory.join(LEGACY_BEST));
        }
        Ok(Self {
            path: directory.join(FILE_NAME),
            legacy_settings,
            legacy_best,
        })
    }

    pub fn load(&self) -> Result<SavedState> {
        let mut state = SavedState::default();
        if let Some(document) = read_json(&self.path)? {
            let document = document
                .as_object()
                .context("Saved state must be a JSON object")?;
            if let Some(settings) = document.get("settings") {
                state.form = read_form(settings, state.form)?;
                state.gpu_intensity = settings["gpu_intensity"]
                    .as_u64()
                    .and_then(|percent| u8::try_from(percent).ok())
                    .and_then(Intensity::new)
                    .unwrap_or_default();
            }
            if let Some(best) = document.get("best").filter(|value| !value.is_null()) {
                state.best = Some(Best::from_json(best)?);
            }
        } else if let Some(path) = &self.legacy_settings
            && let Ok(Some(document)) = read_json(path)
            && let Ok(form) = read_form(&document, state.form.clone())
        {
            state.form = form;
        }
        if state.best.is_none() {
            state.best = self
                .legacy_best
                .iter()
                .filter_map(|path| read_json(path).ok().flatten())
                .filter_map(|document| Best::from_json(&document).ok())
                .min_by_key(|best| best.hash);
        }
        Ok(state)
    }

    pub fn save_settings(&self, form: &SetupForm) -> Result<SavedState> {
        self.update(|state| state.form.clone_from(form))
    }

    pub fn save_threads(&self, count: usize) -> Result<()> {
        self.update(|state| state.form[4] = count.to_string())?;
        Ok(())
    }

    pub fn save_gpu_intensity(&self, intensity: Intensity) -> Result<()> {
        self.update(|state| state.gpu_intensity = intensity)?;
        Ok(())
    }

    pub fn save_best(&self, best: &Best) -> Result<SavedState> {
        self.update(|state| {
            if state
                .best
                .as_ref()
                .is_none_or(|saved| best.hash < saved.hash)
            {
                state.best = Some(best.clone());
            }
        })
    }

    fn update(&self, apply: impl FnOnce(&mut SavedState)) -> Result<SavedState> {
        // Lock a stable path while replacing the JSON so concurrent launches merge changes.
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.path.with_file_name(".bitcoin-detailed.lock"))?;
        lock.lock()?;
        let mut state = self.load()?;
        apply(&mut state);
        let [pool, address, worker, password, threads] = &state.form;
        let document = json!({
            "settings": {
                "pool": pool,
                "address": address,
                "worker": worker,
                "password": password,
                "threads": threads.trim().parse::<u64>().context("Invalid saved thread count")?,
                "gpu_intensity": state.gpu_intensity.percent(),
            },
            "best": state.best.as_ref().map(Best::to_json).transpose()?,
        });
        write_json(&self.path, &document)?;
        Ok(state)
    }
}

fn read_form(document: &Value, mut form: SetupForm) -> Result<SetupForm> {
    let settings = document
        .as_object()
        .context("Settings must be a JSON object")?;
    for (field, key) in form
        .iter_mut()
        .zip(["pool", "address", "worker", "password"])
    {
        if let Some(value) = settings.get(key) {
            *field = value
                .as_str()
                .with_context(|| format!("Setting {key} must be a string"))?
                .to_owned();
        }
    }
    if let Some(threads) = settings.get("threads") {
        form[4] = threads
            .as_u64()
            .map(|count| count.to_string())
            .context("Setting threads must be a nonnegative integer")?;
    }
    Ok(form)
}

fn read_json(path: &Path) -> Result<Option<Value>> {
    match fs::read(path) {
        Ok(bytes) => {
            Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
                format!("Invalid JSON in {}", path.display())
            })?))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("Could not read {}", path.display())),
    }
}

fn write_json(path: &Path, document: &Value) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(document)?;
    // A unique temporary file also allows saves after an interrupted write.
    let now = OffsetDateTime::now_utc().unix_timestamp_nanos();
    let temp = path.with_extension(format!("{}.{now}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    let written = file.write_all(&bytes).and_then(|()| file.sync_all());
    drop(file);
    let result = written.and_then(|()| fs::rename(&temp, path));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_legacy_data_and_keeps_settings_and_best_together() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("bitcoin-detailed-storage-{}", std::process::id()));
        fs::create_dir(&directory)?;
        let result = (|| -> Result<_> {
            let legacy_settings = directory.join("settings.json");
            let legacy_best = directory.join(LEGACY_BEST);
            let storage = Storage {
                path: directory.join(FILE_NAME),
                legacy_settings: Some(legacy_settings.clone()),
                legacy_best: vec![legacy_best.clone()],
            };

            let mut records = [0, 1].map(|byte| {
                let header = [byte; 80];
                let mut hash = mining::hash256(&header);
                hash.reverse();
                Best {
                    hash,
                    header,
                    job: "test".into(),
                    nonce2: "00".into(),
                    recorded_at: None,
                }
            });
            records.sort_by_key(|record| record.hash);
            let [mut better, older] = records;
            let timestamp = OffsetDateTime::parse("2026-11-19T21:04:03.123Z", &Rfc3339)?;
            better.recorded_at = Some(timestamp);
            fs::write(
                &legacy_settings,
                br#"{"pool":"127.0.0.1:3333","worker":"old-worker"}"#,
            )?;
            let mut old_json = older.to_json()?;
            old_json.as_object_mut().unwrap().remove("recorded_at");
            fs::write(&legacy_best, serde_json::to_vec(&old_json)?)?;

            let initial = storage.load()?;
            assert_eq!(initial.gpu_intensity.percent(), 100);
            let mut form = initial.form;
            assert_eq!(form[0], "127.0.0.1:3333");
            assert_eq!(form[2], "old-worker");
            assert_eq!(form[4], "1");
            let imported = storage
                .save_settings(&form)?
                .best
                .context("Legacy best hash was not imported")?;
            assert_eq!(imported.hash, older.hash);
            assert_eq!(imported.recorded_at, None);

            form[2] = "new-worker".into();
            form[4] = "6".into();
            std::thread::scope(|scope| -> Result<()> {
                let settings_write = scope.spawn(|| storage.save_settings(&form));
                let gpu_write =
                    scope.spawn(|| storage.save_gpu_intensity(Intensity::new(37).unwrap()));
                storage.save_best(&better)?;
                settings_write.join().unwrap()?;
                gpu_write.join().unwrap()?;
                Ok(())
            })?;
            storage.save_best(&older)?;
            storage.save_threads(3)?;
            form[4] = "3".into();
            let saved = storage.load()?;
            Ok((form, saved, better))
        })();
        fs::remove_dir_all(directory)?;
        let (form, saved, better) = result?;
        let best = saved.best.context("Best hash was not saved")?;
        assert_eq!(saved.form, form);
        assert_eq!(saved.gpu_intensity.percent(), 37);
        assert_eq!(best.hash, better.hash);
        assert_eq!(best.recorded_at, better.recorded_at);
        Ok(())
    }
}
