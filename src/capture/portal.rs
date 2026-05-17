//! XDG ScreenCast portal + PipeWire capture source.
//!
//! Validation strategy: not unit-tested; smoke-tested manually against a real
//! Plasma Wayland session via the `portal_pipewire_smoke` example.

use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::desktop::PersistMode;
use ashpd::WindowIdentifier;
use async_trait::async_trait;
use pipewire as pw;
use pw::{properties::properties, spa};
use spa::pod::Pod;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::{CaptureSource, Frame, FramePool, FrameSlot, PixelFormat};

/// Reconnect policy: how the outer loop should react to an inner-session error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryDecision {
    /// Transient compositor / pipewire failure; back off and retry with the
    /// same restore token.
    Backoff,
    /// The portal told us the previously-selected monitor no longer exists.
    /// Drop the stored restore token so the next attempt re-prompts the user.
    ResetToken,
    /// User-driven refusal (permission revoked, request cancelled). Bail out
    /// of the loop entirely; retrying would just paper over the user's choice.
    Fatal,
}

/// Classify an `anyhow::Error` produced by the inner portal session into a
/// reconnect decision. Walks the error chain looking for known ashpd variants
/// so the wrapping `.context(...)` calls don't blind the classifier.
fn classify_error(err: &anyhow::Error) -> RetryDecision {
    for cause in err.chain() {
        if let Some(ashpd_err) = cause.downcast_ref::<ashpd::Error>() {
            return classify_ashpd(ashpd_err);
        }
        if let Some(response_err) = cause.downcast_ref::<ashpd::desktop::ResponseError>() {
            return classify_response(*response_err);
        }
        if let Some(portal_err) = cause.downcast_ref::<ashpd::PortalError>() {
            return classify_portal(portal_err);
        }
    }
    RetryDecision::Backoff
}

fn classify_ashpd(err: &ashpd::Error) -> RetryDecision {
    match err {
        ashpd::Error::Response(r) => classify_response(*r),
        ashpd::Error::Portal(p) => classify_portal(p),
        _ => RetryDecision::Backoff,
    }
}

fn classify_response(err: ashpd::desktop::ResponseError) -> RetryDecision {
    match err {
        // User pressed "deny" on the portal dialog; respect that.
        ashpd::desktop::ResponseError::Cancelled => RetryDecision::Fatal,
        ashpd::desktop::ResponseError::Other => RetryDecision::Backoff,
    }
}

fn classify_portal(err: &ashpd::PortalError) -> RetryDecision {
    match err {
        ashpd::PortalError::NotAllowed(_) => RetryDecision::Fatal,
        ashpd::PortalError::Cancelled(_) => RetryDecision::Fatal,
        // Stored restore token now refers to a monitor that no longer exists.
        ashpd::PortalError::NotFound(_) => RetryDecision::ResetToken,
        _ => RetryDecision::Backoff,
    }
}

/// Exponential-backoff bookkeeping for the reconnect loop. Pure state machine,
/// no I/O, so it can be unit-tested without touching D-Bus or PipeWire.
struct ReconnectController {
    backoff: Duration,
    attempts: u32,
}

const BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
const MAX_ATTEMPTS: u32 = 10;

impl ReconnectController {
    fn new() -> Self {
        Self {
            backoff: BACKOFF_INITIAL,
            attempts: 0,
        }
    }

    /// Record a failed session. Returns `Some(sleep_duration)` to back off
    /// before the next attempt, or `None` if the attempt budget is exhausted.
    fn note_failure(&mut self) -> Option<Duration> {
        self.attempts += 1;
        if self.attempts >= MAX_ATTEMPTS {
            return None;
        }
        let sleep = self.backoff;
        self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
        Some(sleep)
    }

    /// Record that the just-finished session delivered at least one frame.
    /// A reconnect that produced real frames before failing is good news:
    /// reset the counters so a daily compositor restart doesn't slowly burn
    /// the budget over a week of uptime.
    fn note_progress(&mut self) {
        self.attempts = 0;
        self.backoff = BACKOFF_INITIAL;
    }
}

/// Production portal capture. `restore_token_path` is where the portal restore
/// token is read on startup (so the user is not re-prompted) and written after
/// each successful `Start`.
pub struct PortalCapture {
    restore_token_path: PathBuf,
    target_fps: u32,
}

impl PortalCapture {
    pub fn new(restore_token_path: PathBuf, target_fps: u32) -> Self {
        Self {
            restore_token_path,
            target_fps,
        }
    }

    fn load_restore_token(&self) -> Option<String> {
        match std::fs::read_to_string(&self.restore_token_path) {
            Ok(s) => {
                let token = s.trim().to_owned();
                if token.is_empty() {
                    None
                } else {
                    Some(token)
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                tracing::warn!(error = %e, path = %self.restore_token_path.display(), "could not read restore token");
                None
            }
        }
    }

    fn save_restore_token(&self, token: &str) {
        if let Some(parent) = self.restore_token_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!(error = %e, path = %parent.display(), "could not create restore token directory");
                return;
            }
        }
        if let Err(e) = std::fs::write(&self.restore_token_path, token) {
            tracing::warn!(error = %e, path = %self.restore_token_path.display(), "could not write restore token");
        }
    }

    /// Wipe the stored restore token so the next handshake re-prompts the user.
    /// Called when the portal reports that the previously-selected monitor no
    /// longer exists (e.g., hot-unplug, monitor reconfiguration).
    fn clear_restore_token(&self) {
        match std::fs::remove_file(&self.restore_token_path) {
            Ok(()) => {
                tracing::info!(
                    path = %self.restore_token_path.display(),
                    "cleared stale portal restore token"
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %self.restore_token_path.display(),
                    "could not clear restore token",
                );
            }
        }
    }

    /// One full portal handshake + PipeWire session. Returns `Ok(())` when the
    /// session ended cleanly (cancellation or compositor-initiated quit) and
    /// `Err(_)` on any failure the outer reconnect loop should react to.
    async fn inner_session(
        &self,
        pool: &FramePool,
        slot: &FrameSlot,
        metrics: &crate::stats::Metrics,
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        let proxy = Screencast::new().await.context("connecting to portal")?;
        let session = proxy
            .create_session()
            .await
            .context("portal create_session")?;
        let restore_token = self.load_restore_token();
        proxy
            .select_sources(
                &session,
                CursorMode::Embedded,
                SourceType::Monitor.into(),
                false,
                restore_token.as_deref(),
                PersistMode::ExplicitlyRevoked,
            )
            .await
            .context("portal select_sources")?
            .response()
            .context("portal select_sources response")?;
        let started = proxy
            .start(&session, &WindowIdentifier::default())
            .await
            .context("portal start")?
            .response()
            .context("portal start response")?;
        if let Some(token) = started.restore_token() {
            self.save_restore_token(token);
        }
        let stream_meta = started
            .streams()
            .first()
            .context("portal returned no streams")?;
        let node_id = stream_meta.pipe_wire_node_id();
        tracing::info!(node_id, size = ?stream_meta.size(), "portal stream selected");

        let portal_fd: OwnedFd = proxy
            .open_pipe_wire_remote(&session)
            .await
            .context("open_pipe_wire_remote")?;

        let (quit_tx, quit_rx) = pw::channel::channel::<()>();
        let (done_tx, done_rx) = oneshot::channel::<anyhow::Result<()>>();

        // Capture the OwnedFd by move so a spawn failure drops (and closes) it
        // rather than leaking the raw fd that an earlier into_raw_fd() would
        // have exposed.
        let thread_handle = std::thread::Builder::new()
            .name("rustiferin-pipewire".into())
            .spawn({
                let pool = pool.clone();
                let slot = slot.clone();
                let target_fps = self.target_fps;
                let metrics = metrics.clone();
                move || {
                    let result =
                        run_pipewire(portal_fd, node_id, target_fps, pool, slot, metrics, quit_rx);
                    let _ = done_tx.send(result);
                }
            })
            .context("spawning pipewire thread")?;

        let mut done_rx = done_rx;
        let result = tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("capture cancelled; asking pipewire loop to quit");
                let _ = quit_tx.send(());
                (&mut done_rx).await.unwrap_or_else(|_| Err(anyhow::anyhow!("pipewire thread dropped result channel")))
            }
            res = &mut done_rx => {
                res.unwrap_or_else(|_| Err(anyhow::anyhow!("pipewire thread dropped result channel")))
            }
        };

        if let Err(e) = thread_handle.join() {
            tracing::error!(panic = ?e, "pipewire thread panicked");
            return Err(anyhow::anyhow!("pipewire thread panicked"));
        }
        result
    }
}

#[async_trait]
impl CaptureSource for PortalCapture {
    async fn run(
        self,
        pool: FramePool,
        slot: FrameSlot,
        metrics: crate::stats::Metrics,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        use std::sync::atomic::Ordering;

        let mut ctrl = ReconnectController::new();
        loop {
            if cancel.is_cancelled() {
                return Ok(());
            }

            let frames_before = metrics.frames_captured().load(Ordering::Relaxed);
            let session_result = self.inner_session(&pool, &slot, &metrics, &cancel).await;
            let frames_after = metrics.frames_captured().load(Ordering::Relaxed);

            let err = match session_result {
                Ok(()) => return Ok(()),
                Err(e) => e,
            };

            // A session that produced real frames before dying is not evidence
            // of a stuck environment, refund the attempt budget so a long
            // uptime survives the occasional compositor restart.
            if frames_after > frames_before {
                ctrl.note_progress();
            }

            match classify_error(&err) {
                RetryDecision::Fatal => {
                    tracing::error!(error = ?err, "portal session ended fatally; not retrying");
                    return Err(err.context("portal session unrecoverable"));
                }
                RetryDecision::ResetToken => {
                    tracing::warn!(error = ?err, "portal session ended; clearing restore token");
                    self.clear_restore_token();
                }
                RetryDecision::Backoff => {}
            }

            let Some(sleep) = ctrl.note_failure() else {
                return Err(err.context("portal session failed too many times"));
            };
            tracing::warn!(
                error = ?err,
                backoff_ms = sleep.as_millis() as u64,
                "portal session ended; retrying",
            );
            tokio::select! {
                _ = tokio::time::sleep(sleep) => {}
                _ = cancel.cancelled() => return Ok(()),
            }
        }
    }
}

/// State shared between PipeWire callbacks and used to translate the negotiated
/// pixel format into our [`PixelFormat`] tag. Held by the `process` closure via
/// the `UserData` struct.
struct StreamState {
    pixel_format: Option<PixelFormat>,
    width: u32,
    height: u32,
}

struct UserData {
    state: Arc<Mutex<StreamState>>,
    pool: FramePool,
    slot: FrameSlot,
    metrics: crate::stats::Metrics,
    /// One-shot guard for the per-frame buffer-type log. Kept outside
    /// `StreamState` so `process()` does not have to re-enter the format mutex
    /// for a flag flip that is otherwise concern-free.
    logged_buffer_type: std::sync::atomic::AtomicBool,
}

fn handle_format_param(state: &Mutex<StreamState>, param: &Pod) {
    let Ok((media_type, media_subtype)) = spa::param::format_utils::parse_format(param) else {
        return;
    };
    if media_type != spa::param::format::MediaType::Video
        || media_subtype != spa::param::format::MediaSubtype::Raw
    {
        return;
    }
    let mut info = spa::param::video::VideoInfoRaw::default();
    if info.parse(param).is_err() {
        return;
    }
    let pixel_format = match info.format() {
        f if f == spa::param::video::VideoFormat::BGRA => Some(PixelFormat::Bgra),
        f if f == spa::param::video::VideoFormat::RGBA => Some(PixelFormat::Rgba),
        f if f == spa::param::video::VideoFormat::BGRx => Some(PixelFormat::Bgrx),
        f if f == spa::param::video::VideoFormat::xRGB => Some(PixelFormat::Xrgb),
        _ => None,
    };
    let Some(pixel_format) = pixel_format else {
        tracing::warn!(format = ?info.format(), "unsupported negotiated format");
        return;
    };
    let mut state = state.lock().expect("state poisoned");
    state.pixel_format = Some(pixel_format);
    state.width = info.size().width;
    state.height = info.size().height;
    tracing::info!(
        format = ?pixel_format,
        width = state.width,
        height = state.height,
        "negotiated video format"
    );
}

/// Log the negotiated `SPA_PARAM_BUFFERS_dataType` mask. Fires at negotiation
/// time so a compositor refusing the MemFd-only constraint still leaves a
/// diagnostic trail even when no frames ever flow.
fn log_buffers_param(param: &Pod) {
    let data_type =
        pw::spa::pod::deserialize::PodDeserializer::deserialize_any_from(param.as_bytes())
            .ok()
            .and_then(|(_, value)| match value {
                pw::spa::pod::Value::Object(obj) => Some(obj),
                _ => None,
            })
            .and_then(|obj| {
                obj.properties
                    .into_iter()
                    .find(|p| p.key == pw::spa::sys::SPA_PARAM_BUFFERS_dataType)
                    .map(|p| p.value)
            })
            .and_then(|value| match value {
                pw::spa::pod::Value::Int(i) => Some(i),
                pw::spa::pod::Value::Choice(pw::spa::pod::ChoiceValue::Int(c)) => match c.1 {
                    pw::spa::utils::ChoiceEnum::None(v) => Some(v),
                    pw::spa::utils::ChoiceEnum::Flags { default, .. }
                    | pw::spa::utils::ChoiceEnum::Enum { default, .. }
                    | pw::spa::utils::ChoiceEnum::Range { default, .. }
                    | pw::spa::utils::ChoiceEnum::Step { default, .. } => Some(default),
                },
                _ => None,
            });
    tracing::info!(
        data_type_mask = ?data_type,
        memfd_bit = 1i32 << pw::spa::sys::SPA_DATA_MemFd,
        "negotiated buffers param"
    );
}

#[allow(clippy::too_many_arguments)]
fn run_pipewire(
    portal_fd: OwnedFd,
    node_id: u32,
    target_fps: u32,
    pool: FramePool,
    slot: FrameSlot,
    metrics: crate::stats::Metrics,
    quit_rx: pw::channel::Receiver<()>,
) -> anyhow::Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).context("pipewire mainloop")?;
    let context = pw::context::ContextRc::new(&mainloop, None).context("pipewire context")?;
    // SAFETY: PipeWire takes ownership of the fd. If it returns an error we never
    // reach a state where two owners exist; on success libpipewire closes it.
    let core = context
        .connect_fd_rc(portal_fd, None)
        .context("connect_fd_rc")?;

    let _quit_attached = quit_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });

    let stream = pw::stream::StreamBox::new(
        &core,
        "rustiferin-capture",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .context("create stream")?;

    let state = Arc::new(Mutex::new(StreamState {
        pixel_format: None,
        width: 0,
        height: 0,
    }));
    let user_data = UserData {
        state: state.clone(),
        pool: pool.clone(),
        slot: slot.clone(),
        metrics: metrics.clone(),
        logged_buffer_type: std::sync::atomic::AtomicBool::new(false),
    };

    let _listener = stream
        .add_local_listener_with_user_data(user_data)
        .state_changed(|_, _, old, new| {
            tracing::info!(?old, ?new, "pipewire stream state");
        })
        .param_changed(|_, user_data, id, param| {
            let Some(param) = param else { return };
            match id {
                x if x == spa::param::ParamType::Format.as_raw() => {
                    handle_format_param(&user_data.state, param);
                }
                x if x == spa::param::ParamType::Buffers.as_raw() => {
                    log_buffers_param(param);
                }
                _ => {}
            }
        })
        .process(|stream, user_data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                tracing::warn!("pipewire process: out of buffers");
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data = &mut datas[0];
            // The Buffers branch of `param_changed` is the preferred site for
            // this log because it fires before streaming starts, but KWin's
            // portal flow treats the buffers pod as a client-side constraint
            // and never echoes it back. Log here too so we always confirm the
            // actual frame-time buffer type on at least one compositor.
            if !user_data
                .logged_buffer_type
                .swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                tracing::info!(
                    buffer_type = ?data.type_(),
                    "first frame buffer data type"
                );
            }
            let chunk_size = data.chunk().size() as usize;
            let chunk_stride = data.chunk().stride() as u32;
            let Some(src) = data.data() else {
                return;
            };
            let (pixel_format, width, height) = {
                let s = user_data.state.lock().expect("state poisoned");
                match s.pixel_format {
                    Some(f) => (f, s.width, s.height),
                    None => return,
                }
            };
            let mut buf = user_data.pool.acquire();
            buf.clear();
            buf.extend_from_slice(&src[..chunk_size.min(src.len())]);
            let frame = Frame {
                buf,
                width,
                height,
                stride: chunk_stride,
                format: pixel_format,
            };
            if let Some(displaced) = user_data.slot.put(frame) {
                user_data.pool.release(displaced.buf);
            }
            user_data
                .metrics
                .frames_captured()
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        })
        .register()
        .context("register stream listener")?;

    let obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaType,
            Id,
            pw::spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaSubtype,
            Id,
            pw::spa::param::format::MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            pw::spa::param::video::VideoFormat::BGRA,
            pw::spa::param::video::VideoFormat::BGRA,
            pw::spa::param::video::VideoFormat::RGBA,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::xRGB,
        ),
        // KWin's xdg-desktop-portal does not honor `VideoSize` hints; adding the
        // property here makes negotiation fail with "no more input formats". So
        // we accept the native screen resolution from the compositor and
        // downsample inside the pipeline instead (see `pipeline::zones`).
        // Cap the frame rate the compositor produces. Choice<Range> default =
        // max = `target_fps`, min = 0 so any rate ≤ target_fps is acceptable.
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            pw::spa::utils::Fraction {
                num: target_fps.max(1),
                denom: 1
            },
            pw::spa::utils::Fraction { num: 0, denom: 1 },
            pw::spa::utils::Fraction {
                num: target_fps.max(1),
                denom: 1
            }
        ),
    );
    let format_bytes: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .map_err(|e| anyhow::anyhow!("serialize format pod: {e}"))?
    .0
    .into_inner();

    // Constrain buffer type to MemFd. Without this, compositors that default
    // to DMA-BUF (Mutter, sway) negotiate a buffer kind our `process()`
    // callback cannot read..
    let memfd_mask: i32 = 1 << pw::spa::sys::SPA_DATA_MemFd;
    let buffers_obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: pw::spa::param::ParamType::Buffers.as_raw(),
        properties: vec![pw::spa::pod::Property {
            key: pw::spa::sys::SPA_PARAM_BUFFERS_dataType,
            flags: pw::spa::pod::PropertyFlags::empty(),
            value: pw::spa::pod::Value::Choice(pw::spa::pod::ChoiceValue::Int(
                pw::spa::utils::Choice(
                    pw::spa::utils::ChoiceFlags::empty(),
                    pw::spa::utils::ChoiceEnum::Flags {
                        default: memfd_mask,
                        flags: vec![memfd_mask],
                    },
                ),
            )),
        }],
    };
    let buffers_bytes: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(buffers_obj),
    )
    .map_err(|e| anyhow::anyhow!("serialize buffers pod: {e}"))?
    .0
    .into_inner();

    let mut params = [
        Pod::from_bytes(&format_bytes).context("Pod::from_bytes format")?,
        Pod::from_bytes(&buffers_bytes).context("Pod::from_bytes buffers")?,
    ];

    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .context("stream connect")?;

    mainloop.run();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_controller_starts_with_one_second_backoff() {
        let mut c = ReconnectController::new();
        assert_eq!(c.note_failure(), Some(Duration::from_secs(1)));
    }

    #[test]
    fn reconnect_controller_doubles_backoff_each_failure() {
        let mut c = ReconnectController::new();
        assert_eq!(c.note_failure(), Some(Duration::from_secs(1)));
        assert_eq!(c.note_failure(), Some(Duration::from_secs(2)));
        assert_eq!(c.note_failure(), Some(Duration::from_secs(4)));
        assert_eq!(c.note_failure(), Some(Duration::from_secs(8)));
        assert_eq!(c.note_failure(), Some(Duration::from_secs(16)));
    }

    #[test]
    fn reconnect_controller_caps_backoff_at_thirty_seconds() {
        let mut c = ReconnectController::new();
        for _ in 0..6 {
            let _ = c.note_failure();
        }
        // After six failures (1, 2, 4, 8, 16, 32->30), the next reported sleep
        // must be the cap.
        assert_eq!(c.note_failure(), Some(BACKOFF_MAX));
        assert_eq!(c.note_failure(), Some(BACKOFF_MAX));
    }

    #[test]
    fn reconnect_controller_returns_none_when_attempt_budget_exhausted() {
        let mut c = ReconnectController::new();
        // The Nth failure (1-indexed) returns a sleep; the MAX_ATTEMPTS-th
        // returns None to break the loop.
        for _ in 0..(MAX_ATTEMPTS - 1) {
            assert!(c.note_failure().is_some());
        }
        assert_eq!(c.note_failure(), None);
    }

    #[test]
    fn reconnect_controller_resets_on_progress() {
        let mut c = ReconnectController::new();
        c.note_failure();
        c.note_failure();
        c.note_failure();
        c.note_progress();
        assert_eq!(c.note_failure(), Some(Duration::from_secs(1)));
    }

    #[test]
    fn classify_user_cancelled_response_is_fatal() {
        let err: anyhow::Error =
            ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled).into();
        assert_eq!(classify_error(&err), RetryDecision::Fatal);
    }

    #[test]
    fn classify_portal_not_allowed_is_fatal() {
        let err: anyhow::Error =
            ashpd::Error::Portal(ashpd::PortalError::NotAllowed("permission revoked".into()))
                .into();
        assert_eq!(classify_error(&err), RetryDecision::Fatal);
    }

    #[test]
    fn classify_portal_cancelled_is_fatal() {
        let err: anyhow::Error =
            ashpd::Error::Portal(ashpd::PortalError::Cancelled("user denied".into())).into();
        assert_eq!(classify_error(&err), RetryDecision::Fatal);
    }

    #[test]
    fn classify_portal_not_found_resets_token() {
        let err: anyhow::Error =
            ashpd::Error::Portal(ashpd::PortalError::NotFound("no such source".into())).into();
        assert_eq!(classify_error(&err), RetryDecision::ResetToken);
    }

    #[test]
    fn classify_other_response_error_backs_off() {
        let err: anyhow::Error =
            ashpd::Error::Response(ashpd::desktop::ResponseError::Other).into();
        assert_eq!(classify_error(&err), RetryDecision::Backoff);
    }

    #[test]
    fn classify_generic_error_backs_off() {
        let err = anyhow::anyhow!("pipewire mainloop died unexpectedly");
        assert_eq!(classify_error(&err), RetryDecision::Backoff);
    }

    #[test]
    fn classify_sees_through_wrapping_context() {
        let inner: anyhow::Error =
            ashpd::Error::Portal(ashpd::PortalError::NotFound("monitor gone".into())).into();
        let wrapped = inner.context("portal select_sources response");
        assert_eq!(classify_error(&wrapped), RetryDecision::ResetToken);
    }
}
