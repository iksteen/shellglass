//! Parser-agnostic terminal source boundary.
//!
//! Publishing code consumes a [`SourceSession`] and does not need to know whether
//! frames came from the built-in PTY/parser pipeline or from an external
//! terminal capture provider. Sink status is deliberately one-way: a PTY source
//! can pause and repaint its owned terminal during a hub outage, while a source
//! observing somebody else's terminal uses the default no-op implementation.

use crate::model::Frame;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::watch;

/// Hub connection status as observed by a frame source.
///
/// Implementations must return quickly. In particular, external capture sources
/// must not write to, clear, pause, or repaint the terminal they observe.
pub trait SinkStatus: Send + Sync {
    /// The external sink became unavailable.
    fn hub_down(&self, _reason: &str) {}

    /// The external sink is available again.
    fn hub_up(&self) {}
}

/// No-op status sink for sources that do not own the observed terminal.
#[derive(Debug, Default)]
pub struct NoopSinkStatus;

impl SinkStatus for NoopSinkStatus {}

/// One active source, ready for the existing frame-oriented publishing pipeline.
///
/// `non_exhaustive`: build one with [`SourceSession::external`] or
/// [`SourceSession::new`] so a future field doesn't break every producer.
#[non_exhaustive]
pub struct SourceSession {
    /// Latest complete screen. `watch` gives every backend newest-frame
    /// backpressure: an unread intermediate frame is replaced, never queued.
    pub frames: watch::Receiver<Arc<Frame>>,
    /// Receives hub outage/recovery notifications.
    pub sink_status: Arc<dyn SinkStatus>,
}

impl SourceSession {
    /// Construct a source that reports hub status to `sink_status` — for a
    /// producer that owns the terminal it captures and can pause/repaint it.
    pub fn new(frames: watch::Receiver<Arc<Frame>>, sink_status: Arc<dyn SinkStatus>) -> Self {
        Self {
            frames,
            sink_status,
        }
    }

    /// Construct an externally-owned source with outage notifications disabled.
    pub fn external(frames: watch::Receiver<Arc<Frame>>) -> Self {
        Self::new(frames, Arc::new(NoopSinkStatus))
    }
}

/// Newest-wins publisher for a parser-independent external frame source.
///
/// Clones share one channel and presentation epoch. Publishing replaces any
/// unread frame instead of queueing it. [`switch_source`](Self::switch_source)
/// increments producer-only metadata so the next browser message is a full
/// snapshot even when the old and new sources have identical dimensions — which
/// is also how a producer resets the OSC 8 link table (see
/// [`crate::model::Grid::links`]).
///
/// Publish from ONE task at a time: clones exist so a producer can hand the
/// handle around, not so several can race. A `publish` concurrent with another
/// clone's `switch_source` can stamp the pre-switch epoch onto the post-switch
/// frame, and the viewer would then get a diff across the discontinuity.
#[derive(Clone)]
pub struct FramePublisher {
    frames: watch::Sender<Arc<Frame>>,
    source_epoch: Arc<AtomicU64>,
}

impl FramePublisher {
    /// Publish the latest complete frame, replacing an unread predecessor.
    pub fn publish(&self, mut frame: Frame) {
        set_source_epoch(&mut frame, self.source_epoch.load(Ordering::Acquire));
        self.frames.send_replace(Arc::new(frame));
    }

    /// Mark a source discontinuity and publish its first complete frame.
    pub fn switch_source(&self, mut frame: Frame) {
        let epoch = self
            .source_epoch
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        set_source_epoch(&mut frame, epoch);
        self.frames.send_replace(Arc::new(frame));
    }

    /// Current complete frame, primarily for producer-side image protection.
    pub fn current(&self) -> Arc<Frame> {
        self.frames.borrow().clone()
    }
}

/// Create an externally-owned source and its newest-wins publisher.
pub fn external_source(mut initial: Frame) -> (FramePublisher, SourceSession) {
    set_source_epoch(&mut initial, 0);
    let (frames, receiver) = watch::channel(Arc::new(initial));
    (
        FramePublisher {
            frames,
            source_epoch: Arc::new(AtomicU64::new(0)),
        },
        SourceSession::external(receiver),
    )
}

fn set_source_epoch(frame: &mut Frame, epoch: u64) {
    let Frame::Screen(grid) = frame;
    grid.source_epoch = epoch;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Color, Grid};

    fn frame(title: &str) -> Arc<Frame> {
        Arc::new(Frame::Screen(Grid {
            source_epoch: 0,
            cols: 1,
            rows: vec![vec![Default::default()]],
            cursor: None,
            cursor_style: 0,
            default_colors: (Color::Default, Color::Default),
            title: title.into(),
            links: Default::default(),
            images: vec![],
            image_data: Default::default(),
        }))
    }

    #[test]
    fn synthetic_external_source_is_frame_compatible_and_latest_only() {
        let (publisher, mut source) = external_source((*frame("initial")).clone());
        publisher.publish((*frame("skipped")).clone());
        publisher.publish((*frame("latest")).clone());
        {
            let current = source.frames.borrow_and_update();
            let Frame::Screen(grid) = &**current;
            assert_eq!(grid.title, "latest");
            assert_eq!(grid.source_epoch, 0);
        }

        publisher.switch_source((*frame("switched")).clone());
        {
            let current = source.frames.borrow_and_update();
            let Frame::Screen(grid) = &**current;
            assert_eq!(grid.title, "switched");
            assert_eq!(grid.source_epoch, 1);
        }

        publisher.publish((*frame("same source")).clone());
        {
            let current = source.frames.borrow_and_update();
            let Frame::Screen(grid) = &**current;
            assert_eq!(grid.source_epoch, 1);
            assert_eq!(publisher.current().as_ref(), current.as_ref());
        }

        // External status reporting must be harmless by construction.
        source.sink_status.hub_down("test outage");
        source.sink_status.hub_up();
    }
}
