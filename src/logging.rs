//! Minimal structured logger. Zero dependencies, zero locks held across
//! output, one `println!` per line so concurrent threads never tear a line.
//!
//! `tracing` was rejected on purpose: it drags in a subscriber ecosystem for
//! needs a music player does not have. Level filter: `KYOUKO_LOG=debug|info|warn|error`
//! (default `info`).

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

pub const LVL_ERROR: u8 = 0;
pub const LVL_WARN: u8 = 1;
pub const LVL_INFO: u8 = 2;
pub const LVL_DEBUG: u8 = 3;

static START: OnceLock<Instant> = OnceLock::new();
static MAX: AtomicU8 = AtomicU8::new(LVL_INFO);

pub fn init() {
    START.set(Instant::now()).ok();
    let lvl = match std::env::var("KYOUKO_LOG").as_deref() {
        Ok("error") => LVL_ERROR,
        Ok("warn") => LVL_WARN,
        Ok("debug") => LVL_DEBUG,
        _ => LVL_INFO,
    };
    MAX.store(lvl, Ordering::Relaxed);
}

#[inline]
pub fn enabled(lvl: u8) -> bool {
    lvl <= MAX.load(Ordering::Relaxed)
}

/// Monotonic seconds since startup — enough to reason about event timing.
pub fn stamp() -> String {
    let t = START.get().map(Instant::elapsed).unwrap_or_default();
    format!("{:7.3}s", t.as_secs_f32())
}

pub fn level_name(lvl: u8) -> &'static str {
    match lvl {
        LVL_ERROR => "ERROR",
        LVL_WARN => "WARN ",
        LVL_INFO => "INFO ",
        _ => "DEBUG",
    }
}

#[macro_export]
macro_rules! log_at {
    ($lvl:expr, $tag:expr, $($arg:tt)+) => {
        if $crate::logging::enabled($lvl) {
            println!(
                "[{} {} {:>7}] {}",
                $crate::logging::stamp(),
                $crate::logging::level_name($lvl),
                $tag,
                format_args!($($arg)+)
            );
        }
    };
}

#[macro_export]
macro_rules! log_error {
    ($tag:expr, $($arg:tt)+) => { $crate::log_at!($crate::logging::LVL_ERROR, $tag, $($arg)+) };
}

#[macro_export]
macro_rules! log_warn {
    ($tag:expr, $($arg:tt)+) => { $crate::log_at!($crate::logging::LVL_WARN, $tag, $($arg)+) };
}

#[macro_export]
macro_rules! log_info {
    ($tag:expr, $($arg:tt)+) => { $crate::log_at!($crate::logging::LVL_INFO, $tag, $($arg)+) };
}

#[macro_export]
macro_rules! log_debug {
    ($tag:expr, $($arg:tt)+) => { $crate::log_at!($crate::logging::LVL_DEBUG, $tag, $($arg)+) };
}
