//! Optional decoder-stage wall timings, enabled only for finite diagnostic runs.
//! No callback hooks, timers, sampling threads or changes to audio scheduling.
use crate::broker::Source;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
pub(super) enum Stage {
    Read,
    Decode,
    Copy,
    Eq,
    Volume,
    Resample,
    Mix,
}
const NAMES: [&str; 7] = [
    "packet_read",
    "decode",
    "sample_copy",
    "eq",
    "volume",
    "resample",
    "channel_mix_copy",
];
#[derive(Clone, Copy, Default)]
struct Counter {
    calls: u64,
    nanos: u128,
}
pub(super) struct Profile {
    destination: Option<PathBuf>,
    source: String,
    counters: [Counter; 7],
}
impl Profile {
    pub(super) fn new(source: &Source) -> Self {
        let destination = std::env::var_os("KYOUKO_DSP_PROFILE").map(PathBuf::from);
        Self {
            source: if destination.is_some() {
                source.raw().into()
            } else {
                String::new()
            },
            destination,
            counters: [Counter::default(); 7],
        }
    }
    pub(super) fn start(&self) -> Option<Instant> {
        self.destination.as_ref().map(|_| Instant::now())
    }
    pub(super) fn end(&mut self, stage: Stage, start: Option<Instant>) {
        if let Some(start) = start {
            self.add(stage, start.elapsed());
        }
    }
    fn add(&mut self, stage: Stage, elapsed: Duration) {
        let counter = &mut self.counters[stage as usize];
        counter.calls += 1;
        counter.nanos += elapsed.as_nanos();
    }
}
impl Drop for Profile {
    fn drop(&mut self) {
        let Some(destination) = &self.destination else {
            return;
        };
        let stages: Vec<_> = NAMES.iter().zip(self.counters).map(|(name, counter)|
            serde_json::json!({"stage": name, "calls": counter.calls, "wall_ns": counter.nanos})).collect();
        let result = serde_json::json!({"source": self.source, "stages": stages,
            "scope": "successful packet stages; includes preemption/allocation; excludes PCM capacity waits and callbacks"});
        if let Ok(bytes) = serde_json::to_vec_pretty(&result) {
            let _ = std::fs::write(destination, bytes);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stage_counters_are_separate_and_disabled_start_is_empty() {
        let mut profile = Profile {
            destination: None,
            source: String::new(),
            counters: [Counter::default(); 7],
        };
        assert!(profile.start().is_none());
        profile.end(Stage::Read, None);
        profile.add(Stage::Eq, Duration::from_nanos(100));
        profile.add(Stage::Eq, Duration::from_nanos(200));
        profile.add(Stage::Decode, Duration::from_nanos(50));
        assert_eq!(profile.counters[Stage::Eq as usize].calls, 2);
        assert_eq!(profile.counters[Stage::Eq as usize].nanos, 300);
        assert_eq!(profile.counters[Stage::Decode as usize].nanos, 50);
        assert_eq!(profile.counters[Stage::Read as usize].calls, 0);
    }
}
