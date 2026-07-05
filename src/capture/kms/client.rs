//! Client for gpu-screen-recorder's `gsr-kms-server`.
//!
//! Replicates the v5 handshake from gsr's `kms_client.c`: bind a named unix
//! socket, spawn the (capability-holding) helper pointed at it, `accept` its
//! connection, then hand the server one end of a `socketpair` via `SCM_RIGHTS`
//! (`REPLACE_CONNECTION`) and use the other end for all `GET_KMS` calls. Each
//! `GET_KMS` returns the current scanout planes with their dma-buf fds passed
//! out-of-band via `SCM_RIGHTS`.

use std::io::{ErrorKind, IoSlice, IoSliceMut};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use nix::sys::socket::{
    recvmsg, sendmsg, socketpair, AddressFamily, ControlMessage, ControlMessageOwned, MsgFlags,
    SockFlag, SockType,
};

use super::protocol::*;

/// A live connection to a `gsr-kms-server` instance. Keeps the helper process
/// alive for its lifetime and kills it on drop.
pub struct GsrKmsClient {
    child: Child,
    /// Our end of the socketpair; all `GET_KMS` traffic flows over this.
    local: OwnedFd,
}

impl GsrKmsClient {
    /// Spawn `gsr-kms-server` for `card_path` (e.g. `/dev/dri/card0`) and
    /// complete the connection handshake. Fails fast if the helper is missing,
    /// dies, or speaks a different protocol version.
    pub fn spawn(card_path: &str) -> Result<Self> {
        let socket_path = unique_socket_path()?;
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("bind kms socket at {}", socket_path.display()))?;
        listener
            .set_nonblocking(true)
            .context("set kms socket non-blocking")?;

        let (local, remote) = socketpair(
            AddressFamily::Unix,
            SockType::Stream,
            None,
            SockFlag::empty(),
        )
        .context("create socketpair")?;

        let mut child = Command::new("gsr-kms-server")
            .arg(&socket_path)
            .arg(card_path)
            .spawn()
            .context("spawn gsr-kms-server (is the gpu-screen-recorder package installed?)")?;

        let handshake = (|| -> Result<()> {
            let initial = accept_with_timeout(&listener, &mut child, Duration::from_secs(5))?;
            // Give the server the remote socketpair end; the reply arrives on `local`.
            send_request(
                initial.as_raw_fd(),
                &KmsRequest::new(KMS_REQUEST_TYPE_REPLACE_CONNECTION, remote.as_raw_fd()),
                Some(remote.as_raw_fd()),
            )
            .context("send REPLACE_CONNECTION")?;
            let resp = recv_response(local.as_raw_fd()).context("recv REPLACE_CONNECTION reply")?;
            if resp.version != GSR_KMS_PROTOCOL_VERSION {
                bail!(
                    "gsr-kms-server protocol version {} != expected {} (gpu-screen-recorder \
                     changed its protocol; the rustiferin kms backend needs updating)",
                    resp.version,
                    GSR_KMS_PROTOCOL_VERSION
                );
            }
            Ok(())
        })();

        // The named socket is only needed for the initial connection.
        drop(listener);
        let _ = std::fs::remove_file(&socket_path);

        if let Err(e) = handshake {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
        // The server dup'd `remote`; our copy is no longer needed.
        drop(remote);

        tracing::info!(card = card_path, "gsr-kms-server connected");
        Ok(Self { child, local })
    }

    /// Fetch the current scanout planes. The returned response owns dma-buf fds;
    /// the caller must [`close_response_fds`] them after import.
    pub fn get_kms(&mut self) -> Result<KmsResponse> {
        if let Some(status) = self.child.try_wait().context("poll gsr-kms-server")? {
            bail!("gsr-kms-server exited: {status}");
        }
        send_request(
            self.local.as_raw_fd(),
            &KmsRequest::new(KMS_REQUEST_TYPE_GET_KMS, 0),
            None,
        )
        .context("send GET_KMS")?;
        let resp = recv_response(self.local.as_raw_fd()).context("recv GET_KMS")?;
        if resp.version != GSR_KMS_PROTOCOL_VERSION {
            bail!(
                "gsr-kms-server protocol version {} != expected {}",
                resp.version,
                GSR_KMS_PROTOCOL_VERSION
            );
        }
        if resp.result != KMS_RESULT_OK {
            bail!(
                "gsr-kms-server error (result {}): {}",
                resp.result,
                resp.err_msg_str()
            );
        }
        Ok(resp)
    }
}

impl Drop for GsrKmsClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Close every dma-buf fd in a response. Call once the planes have been imported.
pub fn close_response_fds(response: &KmsResponse) {
    let num_items = response.num_items.max(0) as usize;
    for item in response.items.iter().take(num_items) {
        let n = item.num_dma_bufs.clamp(0, GSR_KMS_MAX_DMA_BUFS as i32) as usize;
        for buf in item.dma_buf.iter().take(n) {
            if buf.fd >= 0 {
                // SAFETY: fds owned by this response, received via SCM_RIGHTS,
                // not used after this call.
                unsafe {
                    nix::libc::close(buf.fd);
                }
            }
        }
    }
}

fn accept_with_timeout(
    listener: &UnixListener,
    child: &mut Child,
    timeout: Duration,
) -> Result<UnixStream> {
    let start = Instant::now();
    loop {
        match listener.accept() {
            Ok((stream, _)) => return Ok(stream),
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                if let Some(status) = child.try_wait().context("poll gsr-kms-server")? {
                    bail!("gsr-kms-server exited before connecting: {status}");
                }
                if start.elapsed() > timeout {
                    bail!("timed out waiting for gsr-kms-server to connect");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(e).context("accept gsr-kms-server connection"),
        }
    }
}

fn send_request(fd: RawFd, req: &KmsRequest, pass_fd: Option<RawFd>) -> Result<()> {
    let iov = [IoSlice::new(req.as_bytes())];
    let sent = match pass_fd {
        Some(f) => {
            let fds = [f];
            let cmsgs = [ControlMessage::ScmRights(&fds)];
            sendmsg::<()>(fd, &iov, &cmsgs, MsgFlags::empty(), None)
        }
        None => sendmsg::<()>(fd, &iov, &[], MsgFlags::empty(), None),
    }
    .context("sendmsg")?;
    if sent != req.as_bytes().len() {
        bail!("short sendmsg: {sent} of {}", req.as_bytes().len());
    }
    Ok(())
}

fn recv_response(fd: RawFd) -> Result<KmsResponse> {
    let mut resp = KmsResponse::zeroed();
    let mut cmsg_buf = nix::cmsg_space!([RawFd; GSR_KMS_MAX_ITEMS * GSR_KMS_MAX_DMA_BUFS]);
    let (nbytes, fds) = {
        let mut iov = [IoSliceMut::new(resp.as_mut_bytes())];
        let msg = recvmsg::<()>(fd, &mut iov, Some(&mut cmsg_buf), MsgFlags::empty())
            .context("recvmsg")?;
        let mut fds = Vec::new();
        for cmsg in msg.cmsgs().context("parse cmsgs")? {
            if let ControlMessageOwned::ScmRights(f) = cmsg {
                fds.extend(f);
            }
        }
        (msg.bytes, fds)
    };
    if nbytes == 0 {
        // Any fds that slipped through before EOF must not leak.
        for fd in fds {
            unsafe { nix::libc::close(fd) };
        }
        bail!("gsr-kms-server closed the connection");
    }
    if let Err(e) = assign_fds(&mut resp, &fds) {
        for fd in fds {
            unsafe { nix::libc::close(fd) };
        }
        bail!("dma-buf fd count mismatch from gsr-kms-server: {e:?}");
    }
    Ok(resp)
}

fn unique_socket_path() -> Result<PathBuf> {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME not set"))?;
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = format!(".gsr-kms-socket-rustiferin-{}-{}", std::process::id(), n);
    Ok(Path::new(&home).join(name))
}
