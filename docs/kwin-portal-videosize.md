# KWin pins screencast resolution to the source's native size

## What we wanted

Ask the compositor to deliver screen captures at a lower resolution
than the monitor's native one. For an ambient-light agent that reduces
each frame to ~100 averaged colors, even 640×360 is heavy
oversampling, and downscaling at the source would cut both KWin's
per-frame encode cost and our pipeline's pixel-scan cost.

## What KWin actually advertises

`kwin/src/plugins/screencast/screencaststream.cpp:772` (v6.6.4, same
on master at the time of writing):

```cpp
spa_pod_builder_add(b, SPA_FORMAT_VIDEO_size, SPA_POD_Rectangle(resolution), 0);
```

That's a **fixed `SPA_POD_Rectangle`**, not a `Choice<Range>` or
`Choice<Enum>`. `resolution` is built from `m_resolution.width()` /
`m_resolution.height()`, which is set from the source's native screen
geometry. KWin offers exactly one supported size per format: the
screen's native resolution. Take it or leave it.

This is a property of the **producer** (KWin), not the portal.
`xdg-desktop-portal-kde` itself doesn't touch SPA pod negotiation; it
delegates entirely to KWin via the
`zkde_screencast_unstable_v1` Wayland protocol. There is no `VideoSize`
mention anywhere in the `xdg-desktop-portal-kde` repo.

## Why our experiment failed

PipeWire format negotiation is set intersection: producer offer ∧
consumer accept. We sent a consumer pod with:

```text
VideoSize (Choice<Range, Rectangle>)
   default = {640, 360}
   min     = {1, 1}
   max     = {640, 360}
```

KWin offered the fixed rectangle `{2560, 1440}`. The intersection of
"the range [(1,1)-(640,360)]" and "the singleton {2560,1440}" is
empty. PipeWire surfaced `Error("no more input formats")`: the
generic "no compatible format was found" error.

The initial spike tried three variants of the same
bounded-below-native shape (different ranges, framerate property
swaps, property ordering). All three failed for the same structural
reason: there was no overlap between the consumer's range and KWin's
fixed point. None of the variants could have succeeded.

A variant with `max = {7680, 4320}` (a range containing the native
size) would have negotiated cleanly, but PipeWire would have resolved
the intersection to `{2560, 1440}`, which is what we get today by
omitting `VideoSize` entirely. Net behavior identical.

## What this is not

- **Not a portal bug.** The portal doesn't decide stream resolution.
- **Not a `VideoSize`-is-ignored bug.** KWin does advertise
  `VideoSize`; it's just a fixed value.
- **Not a `pipewire-rs` issue.** The negotiation behavior is what the
  SPA spec prescribes; the failing case was always going to fail.

## What it would take to lower capture resolution

KWin's screencast plugin would need to advertise `VideoSize` as a
`Choice<Range>` and implement a GPU downscale stage between the
compositor output and the PipeWire stream, render the screen to a
target texture, sample it down with a hardware-filtered pass, and feed
the smaller buffer into PipeWire. The relevant code paths are
`OutputScreenCastSource::frame()` and `ScreenCastStream::recordFrame()`
in `src/plugins/screencast/`.

That's a feature, not a fix. Worth filing on `bugs.kde.org` against
the `kwin` product if someone wants to push it. Reasonable use cases
beyond ambient lighting: ML / vision pipelines that don't need
pixel-accurate fidelity, screen-recording tools that want to record at
a chosen resolution rather than the monitor's, remote-desktop clients
that want bandwidth control.

## What we do instead

Accept the compositor's native frame size. The `capture.subsample`
config knob (default `4`) reads every Nth pixel inside each zone's
averaging loop, giving a 16× reduction in pipeline pixel work without
touching the capture rate. KWin's screencast cost remains the floor we
can't push from here; with a 30 fps cap (from `target_fps`), that
landed at ~29% of one core on a 2560×1440 monitor during testing.

If a future user complains about per-process CPU, the realistic next
step on our side is a 2×2 or 4×4 box-downsample in the pipeline before
`average_zones`, or a GPU-side downscale via wgpu, which costs us a
heavy dependency for a few percentage points of CPU. Neither addresses
KWin's share of the total cost.

## Source references

- KWin v6.6.4: `src/plugins/screencast/screencaststream.cpp:772`
  (`buildFormat`).
- KWin master at clone time (`737b4f1`): same line, same shape.
- `xdg-desktop-portal-kde` v6.6.4: no `VideoSize` mention; portal
  delegates to KWin via `Screencasting::createOutputStream` and the
  `zkde_screencast_unstable_v1` protocol.
