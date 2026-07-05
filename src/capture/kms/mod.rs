//! KMS capture backend: read the scanout framebuffer directly via
//! gpu-screen-recorder's `gsr-kms-server` helper, bypassing the compositor.
//!
//! This avoids the forced-composition cost of the portal path (which knocks a
//! fullscreen game off direct scanout), at the cost of depending on the
//! `gpu-screen-recorder` package for the capability-holding helper binary.
//!
//! Layers: [`protocol`] mirrors the helper's socket wire format; `client` (spawn
//! + handshake + GET_KMS) and `egl` (dma-buf import + zone readback) build on it.

pub mod capture;
pub mod client;
pub mod egl;
pub mod protocol;

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};

/// The DRM card name owning a connector sysfs entry, e.g. `card1-DP-1` -> `card1`.
/// Returns `None` for non-connector names (`renderD128`, bare `card1`, ...).
fn card_name_from_connector(entry: &str) -> Option<String> {
    let (card, rest) = entry.split_once('-')?;
    if card.starts_with("card") && !rest.is_empty() {
        Some(card.to_string())
    } else {
        None
    }
}

/// Find the DRM card driving a connected display, returned as `/dev/dri/cardN`.
/// Picks the lowest-numbered card that has at least one connected connector.
pub fn detect_capture_card() -> Result<String> {
    let mut cards: Vec<String> = Vec::new();
    for entry in std::fs::read_dir("/sys/class/drm").context("read /sys/class/drm")? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(card) = card_name_from_connector(&name) else {
            continue;
        };
        let status = std::fs::read_to_string(entry.path().join("status")).unwrap_or_default();
        if status.trim() == "connected" && !cards.contains(&card) {
            cards.push(card);
        }
    }
    cards.sort();
    let card = cards
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no DRM card with a connected display found"))?;
    Ok(format!("/dev/dri/{card}"))
}

/// Resolve the render node (`/dev/dri/renderD*`) that shares a GPU with `card`
/// (`/dev/dri/cardN`). The numbering is not aligned (card1 can pair with
/// renderD128), so match on the underlying PCI device via sysfs.
pub fn resolve_render_node(card: &str) -> Result<String> {
    let card_name = Path::new(card)
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("bad card path {card}"))?;
    let card_dev = std::fs::canonicalize(format!("/sys/class/drm/{card_name}/device"))
        .with_context(|| format!("resolve device for {card_name}"))?;
    for entry in std::fs::read_dir("/dev/dri").context("read /dev/dri")? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("renderD") {
            continue;
        }
        let render_dev = match std::fs::canonicalize(format!("/sys/class/drm/{name}/device")) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if render_dev == card_dev {
            return Ok(format!("/dev/dri/{name}"));
        }
    }
    bail!("no render node shares a GPU with {card}")
}

#[cfg(test)]
mod tests {
    use super::card_name_from_connector;

    #[test]
    fn connector_name_extracts_card() {
        assert_eq!(
            card_name_from_connector("card1-DP-1").as_deref(),
            Some("card1")
        );
        assert_eq!(
            card_name_from_connector("card0-HDMI-A-1").as_deref(),
            Some("card0")
        );
    }

    #[test]
    fn non_connector_names_ignored() {
        assert_eq!(card_name_from_connector("renderD128"), None);
        assert_eq!(card_name_from_connector("card1"), None);
        assert_eq!(card_name_from_connector("version"), None);
    }
}
