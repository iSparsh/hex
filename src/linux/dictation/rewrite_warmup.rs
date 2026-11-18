//! Speculative OpenCode warmup for a Linux dictation that reaches the cutoff.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::app_settings::LabsSettings;
use crate::audio::CaptureInstant;
use crate::dictation::{DictationCapture, modes::ModeRuntime};
use crate::labs::Feature;

pub(super) fn delay(settings: &LabsSettings) -> Option<Duration> {
    (settings.delay_local_rewrite && Feature::DelayLocalRewrite.managed().is_none())
        .then(|| Duration::from_secs(settings.local_rewrite_seconds.into()))
}

pub(super) fn is_short(
    start: Option<CaptureInstant>,
    end: CaptureInstant,
    after: Option<Duration>,
) -> bool {
    start
        .zip(after)
        .is_some_and(|(start, after)| end.duration_since(start) < after)
}

pub(super) struct Warmup {
    modes: Arc<RwLock<ModeRuntime>>,
    attempted: bool,
    worker: Option<JoinHandle<()>>,
    cancelled: Arc<AtomicBool>,
}

impl Warmup {
    pub(super) fn new(modes: Arc<RwLock<ModeRuntime>>) -> Self {
        Self {
            modes,
            attempted: false,
            worker: None,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(super) fn begin_capture(&mut self) {
        self.attempted = false;
    }

    pub(super) fn maybe_start(
        &mut self,
        capture: &DictationCapture,
        through: CaptureInstant,
        settings: &LabsSettings,
    ) {
        if self.attempted || !capture.is_intentional() {
            return;
        }
        let Some((start, after)) = capture.started_at().zip(delay(settings)) else {
            return;
        };
        if through.duration_since(start) < after {
            return;
        }
        self.attempted = true;
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
        {
            return;
        }
        let modes = self.modes.clone();
        let cancelled = self.cancelled.clone();
        // Context discovery and HTTP both run off the listener. The output job
        // still selects its mode using the context captured at release.
        self.worker = thread::Builder::new()
            .name("hex-rewrite-warmup".into())
            .spawn(move || {
                let modes = modes
                    .read()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone();
                if !cancelled.load(Ordering::Acquire) && modes.has_local_rewrite() {
                    let context = crate::linux::context::capture(modes.reads_page());
                    modes.warm_local_rewrite(&context, &cancelled);
                }
            })
            .ok();
    }
}

impl Drop for Warmup {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_cutoff_defaults_to_ten_seconds_including_the_boundary() {
        let settings = LabsSettings::default();
        let delay = delay(&settings);
        assert_eq!(delay, Some(Duration::from_secs(10)));
        let start = CaptureInstant::ZERO;
        assert!(is_short(
            Some(start),
            start + Duration::from_millis(9_999),
            delay
        ));
        assert!(!is_short(
            Some(start),
            start + Duration::from_secs(10),
            delay
        ));
        assert!(!is_short(Some(start), start, None));
    }
}
