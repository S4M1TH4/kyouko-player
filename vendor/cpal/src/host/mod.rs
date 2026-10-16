// Local platform subset; retained backend implementations follow upstream.
#[cfg(target_os = "linux")]
pub(crate) mod alsa;
#[cfg(windows)]
pub(crate) mod com;
pub(crate) mod equilibrium;
pub(crate) mod latch;
#[cfg(all(target_os = "linux", feature = "pipewire"))]
pub(crate) mod pipewire;
#[cfg(all(target_os = "linux", feature = "pulseaudio"))]
pub(crate) mod pulseaudio;
#[cfg(windows)]
pub(crate) mod wasapi;

#[allow(dead_code)]
pub(crate) type ErrorCallbackArc = std::sync::Arc<std::sync::Mutex<dyn FnMut(crate::Error) + Send>>;
#[cfg(any(
    windows,
    all(target_os = "linux", any(feature = "pipewire", feature = "pulseaudio"))
))]
pub(crate) mod error_emit;
#[cfg(any(
    windows,
    all(target_os = "linux", any(feature = "pipewire", feature = "pulseaudio"))
))]
pub(crate) use error_emit::emit_error;
#[cfg(any(windows, all(target_os = "linux", feature = "pipewire")))]
pub(crate) use error_emit::try_emit_error;

/// Convert a frame count at a given sample rate to a [`std::time::Duration`].
#[inline]
#[cfg_attr(windows, allow(dead_code))]
pub(crate) fn frames_to_duration(
    frames: crate::FrameCount,
    rate: crate::SampleRate,
) -> std::time::Duration {
    if rate == 0 {
        return std::time::Duration::ZERO;
    }
    let rate = rate as u64;
    let secs = frames as u64 / rate;
    // rem_frames < rate <= u32::MAX, so rem_frames * 1_000_000_000 < u64::MAX
    let rem_frames = frames as u64 % rate;
    // Round to nearest so the duration isn't biased.
    let nanos = (rem_frames * 1_000_000_000 + rate / 2) / rate;
    std::time::Duration::new(secs, nanos as u32)
}

/// Clamps a timestamp so it never precedes one we've already returned.
#[allow(dead_code)]
fn non_decreasing(floor: &mut u64, instant: crate::StreamInstant) -> crate::StreamInstant {
    // u64 nanos covers ~585 years of runtime.
    let nanos = instant.as_nanos().min(u64::MAX as u128) as u64;
    *floor = (*floor).max(nanos);
    crate::StreamInstant::from_nanos(*floor)
}

/// Wraps an input data callback so the `capture` timestamp never regresses across callbacks.
#[allow(dead_code)]
pub(crate) fn monotonic_input_callback<D>(
    mut data_callback: D,
) -> impl FnMut(&crate::Data, &crate::InputCallbackInfo) + Send + 'static
where
    D: FnMut(&crate::Data, &crate::InputCallbackInfo) + Send + 'static,
{
    // FnMut runs on one thread at a time, so the floor needs no synchronization.
    let mut floor = 0u64;
    move |data, info| {
        let mut info = *info;
        info.timestamp.capture = non_decreasing(&mut floor, info.timestamp.capture);
        data_callback(data, &info);
    }
}

/// Wraps an output data callback so the `playback` timestamp never regresses across callbacks.
#[allow(dead_code)]
pub(crate) fn monotonic_output_callback<D>(
    mut data_callback: D,
) -> impl FnMut(&mut crate::Data, &crate::OutputCallbackInfo) + Send + 'static
where
    D: FnMut(&mut crate::Data, &crate::OutputCallbackInfo) + Send + 'static,
{
    let mut floor = 0u64;
    move |data, info| {
        let mut info = *info;
        info.timestamp.playback = non_decreasing(&mut floor, info.timestamp.playback);
        data_callback(data, &info);
    }
}
