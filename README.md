# Rustiferin

Rust-native ambient lighting agent for [Glow Worm Luciferin][gw] firmware on
Plasma 6 Wayland. Captures the screen via the XDG ScreenCast portal + PipeWire,
reduces each frame to a few per-zone average colors, and publishes them to your
LED controller over MQTT.

This is a from-scratch reimplementation of the screen-capture half of
[Firefly Luciferin][firefly]. The protocol on the wire is the stock Glow Worm
`{topic_base}/set/stream` format, so any existing Glow-Worm-flashed ESP works
unmodified.

[gw]: https://github.com/sblantipodi/glow_worm_luciferin
[firefly]: https://github.com/sblantipodi/firefly_luciferin

**Disclaimer 1: Firefly Luciferin is a great project, I made that for my personal use and for fun. 
I'm not affiliated with the project in any way.**

**Disclaimer 2: This is mostly ai-engineered code.**

## Requirements

- Linux with a Plasma 6 Wayland session (other compositors that implement the
  XDG ScreenCast portal will likely work but are untested).
- PipeWire ≥ 0.3.
- An MQTT broker reachable from the host (mosquitto on the LAN works).
- A device with glow worm luciferin flashed on it, and connected to the same broker.

## Install

### Arch / AUR

```sh
yay -S rustiferin
```

(or your AUR helper of choice). The package ships an example config at
`/usr/share/doc/rustiferin/config.example.yaml`. Copy it to your XDG config
directory, edit the broker URL and zone layout, then enable the user unit:

```sh
mkdir -p ~/.config/rustiferin
cp /usr/share/doc/rustiferin/config.example.yaml ~/.config/rustiferin/config.yaml
$EDITOR ~/.config/rustiferin/config.yaml
systemctl --user enable --now rustiferin
```

### Build from source

```sh
git clone https://github.com/dalsh/rustiferin
cd rustiferin
cargo install --path . --locked
mkdir -p ~/.config/rustiferin ~/.config/systemd/user
cp dist/config.example.yaml ~/.config/rustiferin/config.yaml
$EDITOR ~/.config/rustiferin/config.yaml
cp dist/rustiferin.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now rustiferin
```

If you would rather use XDG autostart than systemd, drop
`dist/rustiferin.desktop` into `~/.config/autostart/` instead.

## First run

The portal dialog appears on the first run; pick the monitor you want to
mirror. The picked monitor is remembered via a restore token stored under
`$XDG_STATE_HOME/rustiferin/restore_token`, so subsequent runs are silent.
Expect a one- to two-second negotiation pause between launch and the first LED
update.

A minimal config:

```yaml
general:
  device_name: glow-worm-living-room
mqtt:
  broker_url: mqtt://10.0.0.10:1883
  topic_base: lights/glowwormluciferin
led_matrix:
  reference_width: 1920
  reference_height: 1080
  zones:
    - { x: 0, y: 0, w: 960, h: 1080 }   # left half
    - { x: 960, y: 0, w: 960, h: 1080 } # right half
```

Full schema reference: `src/config/schema.rs`.

## Troubleshooting

**The portal dialog appears every time I start the agent.**
The restore token under `$XDG_STATE_HOME/rustiferin/restore_token` isn't
being persisted. Verify the directory is writable and that the portal
implementation supports persistent tokens (it does on recent KDE; older
xdg-desktop-portal versions don't).

**The strip stays dark.**
Check that:

```sh
mosquitto_sub -h <broker> -t '#' -v
```

shows `lights/.../set/stream` traffic when rustiferin is running. If yes, the
firmware isn't switching into wireless-stream mode. Confirm it sees the
`set` state announcement with `effect: GlowWormWifi`. If no traffic at all,
check the journal:

```sh
journalctl --user -u rustiferin -f
```

**Tasks panicked / process exited.**
The default `EnvFilter` is `info,rustiferin=debug`. Set `RUST_LOG=debug` for
more detail, or `RUST_LOG=rustiferin::stats=debug` to see per-second
throughput.

## What does not work yet

- No hot-reload of `config.yaml`; restart the service after editing.
- No audio reactive mode, no Home Assistant discovery. Not planned for v1.

## License

GPL-3.0-or-later. See `LICENSE`.

## Credits

- [Glow Worm Luciferin][gw] (firmware) and [Firefly Luciferin][firefly]
  (reference agent) by Davide Perini.
- Plasma's xdg-desktop-portal-kde for the screen-capture plumbing.
