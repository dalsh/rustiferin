//! Capture layer: frame types, recycle pool, single-slot newest-wins handoff.
//!
//! The production source ([`portal::PortalCapture`]) is wired in `app.rs`; tests use
//! [`FakeCapture`] to replay deterministic frames without touching D-Bus or PipeWire.

#[cfg(feature = "wayland")]
pub mod portal;

#[cfg(feature = "kms")]
pub mod kms;

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Pixel layout we accept from the compositor. Anything else is rejected at format
/// negotiation; the pipeline trusts the tag and reads channels accordingly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra,
    Rgba,
    /// Byte 0 = B, byte 1 = G, byte 2 = R, byte 3 = padding.
    Bgrx,
    /// Byte 0 = padding, byte 1 = R, byte 2 = G, byte 3 = B.
    Xrgb,
}

/// A single captured frame. `buf` is owned and was allocated by [`FramePool`]; the
/// consumer is responsible for returning it after processing (or after dropping it
/// due to newest-wins displacement).
#[derive(Debug)]
pub struct Frame {
    pub buf: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: PixelFormat,
}

/// Fixed-size recycler for pixel buffers. Cheap-to-clone (internally `Arc`-backed).
///
/// `acquire` pops a buffer or, when empty, allocates a fresh one (logged so a
/// mis-sized pool is visible). `release` pushes a buffer back, dropping it if the
/// pool is already at `pool_size` so it can never grow unbounded.
#[derive(Clone)]
pub struct FramePool {
    inner: Arc<Mutex<Vec<Vec<u8>>>>,
    pool_size: usize,
    capacity_hint: usize,
}

impl FramePool {
    pub fn new(pool_size: usize, capacity_hint: usize) -> Self {
        let mut buffers = Vec::with_capacity(pool_size);
        for _ in 0..pool_size {
            buffers.push(Vec::with_capacity(capacity_hint));
        }
        Self {
            inner: Arc::new(Mutex::new(buffers)),
            pool_size,
            capacity_hint,
        }
    }

    pub fn pool_size(&self) -> usize {
        self.pool_size
    }

    pub fn capacity_hint(&self) -> usize {
        self.capacity_hint
    }

    pub fn acquire(&self) -> Vec<u8> {
        if let Some(mut buf) = self.inner.lock().expect("frame pool poisoned").pop() {
            buf.clear();
            return buf;
        }
        tracing::warn!(
            capacity_hint = self.capacity_hint,
            "frame pool exhausted; allocating a fresh buffer"
        );
        Vec::with_capacity(self.capacity_hint)
    }

    pub fn release(&self, mut buf: Vec<u8>) {
        let mut inner = self.inner.lock().expect("frame pool poisoned");
        if inner.len() >= self.pool_size {
            return;
        }
        buf.clear();
        inner.push(buf);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}

/// Newest-wins, single-slot handoff between capture and pipeline.
///
/// Both sides hold cheap clones; the underlying state is one `Mutex<Option<Frame>>`
/// plus a `Condvar` (for the pipeline's blocking wait) and a `Notify` (for async
/// waiters used in tests). `put` is atomic and returns any displaced frame so the
/// caller can recycle its buffer.
#[derive(Clone)]
pub struct FrameSlot {
    inner: Arc<FrameSlotInner>,
}

struct FrameSlotInner {
    state: Mutex<Option<Frame>>,
    cv: Condvar,
    notify: Notify,
}

/// Bounded timeout for [`FrameSlot::wait_blocking`]. Long enough to avoid burning the
/// CPU when capture stalls; short enough that the pipeline's outer loop observes a
/// `CancellationToken` cancellation within ~100 ms.
const WAIT_BLOCKING_TIMEOUT: Duration = Duration::from_millis(100);

impl FrameSlot {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(FrameSlotInner {
                state: Mutex::new(None),
                cv: Condvar::new(),
                notify: Notify::new(),
            }),
        }
    }

    /// Atomically place `frame` into the slot, returning any displaced frame.
    pub fn put(&self, frame: Frame) -> Option<Frame> {
        let mut guard = self.inner.state.lock().expect("frame slot poisoned");
        let previous = guard.take();
        *guard = Some(frame);
        drop(guard);
        self.inner.cv.notify_all();
        self.inner.notify.notify_waiters();
        previous
    }

    /// Atomically take the current frame, if any.
    pub fn take(&self) -> Option<Frame> {
        self.inner.state.lock().expect("frame slot poisoned").take()
    }

    /// Async wait that resolves as soon as a frame is present in the slot.
    pub async fn wait(&self) {
        loop {
            let notified = self.inner.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self
                .inner
                .state
                .lock()
                .expect("frame slot poisoned")
                .is_some()
            {
                return;
            }
            notified.await;
        }
    }

    /// Synchronous wait with a bounded timeout. Returns true if a frame is ready,
    /// false if the timeout elapsed first. Used by the pipeline OS thread.
    pub fn wait_blocking(&self) -> bool {
        let guard = self.inner.state.lock().expect("frame slot poisoned");
        if guard.is_some() {
            return true;
        }
        let (guard, result) = self
            .inner
            .cv
            .wait_timeout(guard, WAIT_BLOCKING_TIMEOUT)
            .expect("frame slot poisoned");
        if result.timed_out() {
            guard.is_some()
        } else {
            true
        }
    }
}

impl Default for FrameSlot {
    fn default() -> Self {
        Self::new()
    }
}

/// Production and test capture implementations both behind this trait so `app.rs`
/// is the only thing that depends on the concrete type. The trait is invoked once
/// per process lifetime, so the `#[async_trait]` heap allocation is irrelevant.
#[async_trait]
pub trait CaptureSource: Send + 'static {
    async fn run(
        self,
        pool: FramePool,
        slot: FrameSlot,
        metrics: crate::stats::Metrics,
        cancel: CancellationToken,
    ) -> anyhow::Result<()>;
}

/// Test capture source that replays a fixed sequence of frames into the slot,
/// recycling displaced buffers back into the pool, exactly the protocol the
/// real portal capture follows.
#[cfg(any(test, feature = "test-fakes"))]
pub struct FakeCapture {
    frames: Vec<FrameTemplate>,
    interval: Duration,
}

#[cfg(any(test, feature = "test-fakes"))]
#[derive(Debug, Clone)]
pub struct FrameTemplate {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: PixelFormat,
}

#[cfg(any(test, feature = "test-fakes"))]
impl FakeCapture {
    pub fn new(frames: Vec<FrameTemplate>, interval: Duration) -> Self {
        Self { frames, interval }
    }
}

#[cfg(any(test, feature = "test-fakes"))]
#[async_trait]
impl CaptureSource for FakeCapture {
    async fn run(
        self,
        pool: FramePool,
        slot: FrameSlot,
        metrics: crate::stats::Metrics,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        use std::sync::atomic::Ordering;
        for template in self.frames {
            if cancel.is_cancelled() {
                return Ok(());
            }
            if !self.interval.is_zero() {
                tokio::select! {
                    _ = tokio::time::sleep(self.interval) => {}
                    _ = cancel.cancelled() => return Ok(()),
                }
            }
            let mut buf = pool.acquire();
            buf.clear();
            buf.extend_from_slice(&template.pixels);
            let frame = Frame {
                buf,
                width: template.width,
                height: template.height,
                stride: template.stride,
                format: template.format,
            };
            if let Some(old) = slot.put(frame) {
                pool.release(old.buf);
            }
            metrics.frames_captured().fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }
}

/// Spawn `source` on the shared shutdown set under the task name `capture`. The
/// concrete `CaptureSource` impl is the only thing `app.rs` needs to know about
/// at the boundary between epics.
pub fn spawn<S: CaptureSource>(
    shutdown: &mut crate::shutdown::Shutdown,
    source: S,
    pool: FramePool,
    slot: FrameSlot,
    metrics: crate::stats::Metrics,
) {
    let cancel = shutdown.token();
    shutdown.spawn("capture", async move {
        source.run(pool, slot, metrics, cancel).await
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    fn red_frame(width: u32, height: u32) -> Frame {
        let stride = width * 4;
        let mut buf = vec![0u8; (stride * height) as usize];
        for px in buf.chunks_exact_mut(4) {
            // BGRA red
            px[0] = 0;
            px[1] = 0;
            px[2] = 255;
            px[3] = 255;
        }
        Frame {
            buf,
            width,
            height,
            stride,
            format: PixelFormat::Bgra,
        }
    }

    #[test]
    fn pool_acquire_reuses_preallocated_buffer() {
        let pool = FramePool::new(2, 64);
        assert_eq!(pool.len(), 2);
        let buf = pool.acquire();
        assert_eq!(pool.len(), 1);
        assert!(buf.capacity() >= 64);
        assert!(buf.is_empty());
        pool.release(buf);
        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn pool_acquire_when_empty_allocates_fresh() {
        let pool = FramePool::new(1, 32);
        let a = pool.acquire();
        let b = pool.acquire();
        assert_eq!(pool.len(), 0);
        pool.release(a);
        pool.release(b);
        assert_eq!(pool.len(), 1, "release beyond pool_size drops the buffer");
    }

    #[test]
    fn pool_release_clears_buffer_before_storing() {
        let pool = FramePool::new(1, 4);
        let mut buf = pool.acquire();
        buf.extend_from_slice(&[1, 2, 3, 4]);
        pool.release(buf);
        let buf = pool.acquire();
        assert!(buf.is_empty());
    }

    #[test]
    fn slot_preserves_every_pixel_format_variant() {
        for format in [
            PixelFormat::Bgra,
            PixelFormat::Rgba,
            PixelFormat::Bgrx,
            PixelFormat::Xrgb,
        ] {
            let slot = FrameSlot::new();
            slot.put(Frame {
                buf: vec![0u8; 4],
                width: 1,
                height: 1,
                stride: 4,
                format,
            });
            let round_tripped = slot.take().expect("frame present");
            assert_eq!(
                round_tripped.format, format,
                "format must round-trip unchanged"
            );
        }
    }

    #[test]
    fn slot_put_returns_none_when_empty() {
        let slot = FrameSlot::new();
        assert!(slot.put(red_frame(4, 4)).is_none());
    }

    #[test]
    fn slot_put_returns_displaced_frame() {
        let slot = FrameSlot::new();
        slot.put(red_frame(4, 4));
        let displaced = slot.put(red_frame(8, 8));
        let displaced = displaced.expect("first frame must be displaced");
        assert_eq!(displaced.width, 4);
    }

    #[test]
    fn slot_take_consumes_and_leaves_empty() {
        let slot = FrameSlot::new();
        slot.put(red_frame(2, 2));
        let taken = slot.take().expect("take returns put frame");
        assert_eq!(taken.width, 2);
        assert!(slot.take().is_none());
    }

    #[test]
    fn slot_wait_blocking_returns_true_when_frame_present() {
        let slot = FrameSlot::new();
        slot.put(red_frame(1, 1));
        assert!(slot.wait_blocking());
    }

    #[test]
    fn slot_wait_blocking_times_out_when_empty() {
        let slot = FrameSlot::new();
        let started = std::time::Instant::now();
        let ready = slot.wait_blocking();
        assert!(!ready);
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn slot_wait_blocking_wakes_on_put() {
        let slot = FrameSlot::new();
        let producer = slot.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            producer.put(red_frame(2, 2));
        });
        assert!(slot.wait_blocking());
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn slot_wait_returns_immediately_if_frame_present() {
        let slot = FrameSlot::new();
        slot.put(red_frame(1, 1));
        timeout(Duration::from_millis(50), slot.wait())
            .await
            .expect("wait must return immediately");
    }

    #[tokio::test]
    async fn slot_wait_resolves_when_put_happens() {
        let slot = FrameSlot::new();
        let producer = slot.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            producer.put(red_frame(2, 2));
        });
        timeout(Duration::from_millis(500), slot.wait())
            .await
            .expect("wait must resolve once frame is put");
    }

    #[tokio::test]
    async fn fake_capture_streams_frames_and_recycles_buffers() {
        let pool = FramePool::new(2, 16);
        let slot = FrameSlot::new();
        let cancel = CancellationToken::new();

        let templates = vec![
            FrameTemplate {
                pixels: vec![1; 16],
                width: 2,
                height: 2,
                stride: 8,
                format: PixelFormat::Bgra,
            },
            FrameTemplate {
                pixels: vec![2; 16],
                width: 2,
                height: 2,
                stride: 8,
                format: PixelFormat::Bgra,
            },
            FrameTemplate {
                pixels: vec![3; 16],
                width: 2,
                height: 2,
                stride: 8,
                format: PixelFormat::Bgra,
            },
        ];
        let fake = FakeCapture::new(templates, Duration::ZERO);
        let join = tokio::spawn({
            let pool = pool.clone();
            let slot = slot.clone();
            let cancel = cancel.clone();
            async move {
                fake.run(pool, slot, crate::stats::Metrics::new(), cancel)
                    .await
            }
        });

        join.await
            .expect("task joins")
            .expect("fake capture succeeds");

        let last = slot.take().expect("at least one frame ends up in the slot");
        assert_eq!(last.buf.first().copied(), Some(3));
        pool.release(last.buf);
        assert_eq!(
            pool.len(),
            2,
            "all displaced buffers should be back in the pool"
        );
    }

    #[tokio::test]
    async fn fake_capture_honours_cancellation() {
        let pool = FramePool::new(2, 16);
        let slot = FrameSlot::new();
        let cancel = CancellationToken::new();
        let templates = (0..10)
            .map(|i| FrameTemplate {
                pixels: vec![i as u8; 4],
                width: 1,
                height: 1,
                stride: 4,
                format: PixelFormat::Bgra,
            })
            .collect();
        let fake = FakeCapture::new(templates, Duration::from_millis(20));
        let join = tokio::spawn({
            let pool = pool.clone();
            let slot = slot.clone();
            let cancel = cancel.clone();
            async move {
                fake.run(pool, slot, crate::stats::Metrics::new(), cancel)
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(15)).await;
        cancel.cancel();
        let res = timeout(Duration::from_secs(1), join)
            .await
            .expect("task exits on cancel")
            .expect("join ok");
        res.expect("fake capture returns ok on cancellation");
    }
}
