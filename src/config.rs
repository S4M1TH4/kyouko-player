//! Zero-dependency persistence: a tiny `key=value` file at
//! `%LOCALAPPDATA%\kyouko-player\state.cfg` (fallback `./state.cfg` when the
//! variable is missing, e.g. portable runs).
//!
//! Format — human-editable, forward-compatible:
//!
//! ```text
//! # kyouko-player state — rewritten automatically
//! volume=0.050
//! eq_gains=6.00,0.00,0.00,0.00,0.00,0.00,0.00,0.00,0.00,0.00
//! last_track=C:\My Music\some file.mp3
//! last_folder=C:\My Music
//! ```
//!
//! Rules: unknown keys are ignored on read (so future versions round-trip
//! through older builds harmlessly); a value that fails to parse falls back
//! to its default per-key, never aborting the load. Writes are atomic — a
//! temp file + rename, so a crash mid-write can never corrupt the real file.
//! No handle is held across the app lifetime: each save is open→write→close.
//! Saves happen only on the broker (main) thread; there is no locking.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::broker::{EQ_BANDS, EQ_MAX_GAIN_DB};
use crate::log_warn;

/// The state we persist. Everything else in the player is runtime-only.
#[derive(Clone, Debug, PartialEq)]
pub struct PersistedState {
    /// Linear gain, 0.0..=1.0.
    pub volume: f32,
    /// Band gains in dB, 31 Hz → 16 kHz.
    pub eq_gains: [f32; EQ_BANDS],
    /// Raw last track: a local file path or a YouTube URL.
    pub last_track: Option<String>,
    /// Absolute path of the active local folder, rescanned on startup.
    pub last_folder: Option<String>,
    /// Track repeat (mpv `loop-file` style).
    pub loop_enabled: bool,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            volume: 0.8, eq_gains: [0.0; EQ_BANDS], last_track: None,
            last_folder: None, loop_enabled: false,
        }
    }
}

/// Where state lives. Computed per call — cheap, and honest if the
/// environment changes under us.
pub fn path() -> PathBuf {
    match std::env::var_os("LOCALAPPDATA") {
        Some(dir) => PathBuf::from(dir).join("kyouko-player").join("state.cfg"),
        None => PathBuf::from("state.cfg"),
    }
}

/// Read the persisted state. A missing file is a first run: defaults.
pub fn load() -> PersistedState {
    load_from(&path())
}

/// Persist the state. Failures are logged, never fatal.
pub fn store(state: &PersistedState) {
    store_to(&path(), state);
}

pub fn load_from(path: &Path) -> PersistedState {
    match fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(_) => PersistedState::default(),
    }
}

pub fn store_to(path: &Path, state: &PersistedState) {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && fs::create_dir_all(parent).is_err() {
            log_warn!("CFG", "cannot create config dir {}", parent.display());
            return;
        }
    }
    let mut eq = String::with_capacity(EQ_BANDS * 7);
    for (i, gain) in state.eq_gains.iter().enumerate() {
        if i > 0 {
            eq.push(',');
        }
        eq.push_str(&format!("{gain:.2}"));
    }
    let body = format!(
        "# kyouko-player state — rewritten automatically, safe to edit\nvolume={:.3}\neq_gains={eq}\nlast_track={}\nlast_folder={}\nloop={}\n",
        state.volume,
        state.last_track.as_deref().unwrap_or(""),
        state.last_folder.as_deref().unwrap_or(""),
        if state.loop_enabled { "true" } else { "false" },
    );
    let tmp = path.with_extension("cfg.tmp");
    let outcome = fs::File::create(&tmp)
        .and_then(|mut f| f.write_all(body.as_bytes()))
        .and_then(|_| fs::rename(&tmp, path));
    if let Err(e) = outcome {
        log_warn!("CFG", "save failed: {e}");
        let _ = fs::remove_file(&tmp);
    }
}

fn parse(text: &str) -> PersistedState {
    let mut state = PersistedState::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "volume" => {
                if let Ok(v) = value.parse::<f32>() {
                    state.volume = v.clamp(0.0, 1.0);
                }
            }
            "eq_gains" => {
                for (band, part) in value.split(',').enumerate() {
                    if band >= EQ_BANDS {
                        break;
                    }
                    if let Ok(g) = part.trim().parse::<f32>() {
                        state.eq_gains[band] = g.clamp(-EQ_MAX_GAIN_DB, EQ_MAX_GAIN_DB);
                    }
                }
            }
            "last_track" => {
                state.last_track = if value.is_empty() { None } else { Some(value.to_string()) };
            }
            "last_folder" => {
                state.last_folder = if value.is_empty() { None } else { Some(value.to_string()) };
            }
            "loop" => {
                state.loop_enabled = value.eq_ignore_ascii_case("true");
            }
            _ => {} // unknown key — ignore, forward compatibility
        }
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("kyouko-cfg-test-{}-{tag}.cfg", std::process::id()))
    }

    #[test]
    fn round_trip_preserves_values_and_spaces() {
        let path = temp_path("roundtrip");
        let state = PersistedState {
            volume: 0.05,
            eq_gains: [6.0, 0.0, 0.0, 0.0, 0.0, 0.0, -1.5, 0.0, 8.0, 0.0],
            last_track: Some(r"C:\My Music\kyouko test file.mp3".into()),
            last_folder: Some(r"C:\My Music".into()),
            loop_enabled: true,
        };
        store_to(&path, &state);
        let loaded = load_from(&path);
        assert_eq!(loaded, state);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn folder_state_round_trip_and_legacy_defaults() {
        let path = temp_path("folder");
        let state = PersistedState {
            last_folder: Some(r"C:\音楽\Album = Live".into()),
            ..Default::default()
        };
        store_to(&path, &state);
        assert_eq!(load_from(&path), state);
        store_to(&path, &PersistedState::default());
        assert_eq!(load_from(&path).last_folder, None);
        fs::write(&path, "last_track=C:\\Music\\old.mp3\nvolume=0.5\n").unwrap();
        let legacy = load_from(&path);
        assert_eq!(legacy.last_folder, None);
        assert_eq!(legacy.last_track.as_deref(), Some(r"C:\Music\old.mp3"));
        assert_eq!(legacy.volume, 0.5);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn garbage_file_degrades_to_defaults_per_key() {
        let path = temp_path("garbage");
        fs::write(&path, "volume=notanumber\neq_gains=x,y,z\nrandom=1\nlast_track=\n").unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded, PersistedState::default());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn values_are_clamped_on_load() {
        let path = temp_path("clamp");
        fs::write(&path, "volume=2.5\neq_gains=99,-99,0,0,0,0,0,0,0,0\n").unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.volume, 1.0);
        assert_eq!(loaded.eq_gains[0], EQ_MAX_GAIN_DB);
        assert_eq!(loaded.eq_gains[1], -EQ_MAX_GAIN_DB);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn loop_flag_round_trip_and_garbage() {
        let path = temp_path("loop");
        store_to(&path, &PersistedState { loop_enabled: true, ..Default::default() });
        assert_eq!(load_from(&path).loop_enabled, true);
        store_to(&path, &PersistedState { loop_enabled: false, ..Default::default() });
        assert_eq!(load_from(&path).loop_enabled, false);
        fs::write(&path, "loop=banana
").unwrap();
        assert_eq!(load_from(&path).loop_enabled, false); // strict parse → default
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn partial_eq_gains_fill_flat() {
        let path = temp_path("partial");
        fs::write(&path, "eq_gains=3.5\n").unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.eq_gains[0], 3.5);
        assert!(loaded.eq_gains[1..].iter().all(|g| *g == 0.0));
        let _ = fs::remove_file(&path);
    }
}
