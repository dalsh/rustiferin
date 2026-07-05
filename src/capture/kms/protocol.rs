//! Wire protocol for gpu-screen-recorder's `gsr-kms-server` helper.
//!
//! These structs mirror `kms/kms_shared.h` from gpu-screen-recorder **protocol
//! version 5** (as shipped in gsr 5.13.x) byte-for-byte: the server writes a
//! fixed-size `gsr_kms_response` to the socket via `iovec` and passes the
//! dma-buf fds out-of-band via `SCM_RIGHTS`. The layout is validated against the
//! real C ABI by the `const` size assertions below (probed on x86_64 with
//! libdrm's 32-byte `hdr_output_metadata`).
//!
//! We pin one protocol version deliberately: [`gsr_kms_client`] checks
//! `response.version` and fails fast on a mismatch (e.g. after a gsr upgrade),
//! rather than silently misparsing a changed layout.
//!
//! [`gsr_kms_client`]: super::client

use std::os::fd::RawFd;

/// gsr protocol version this client speaks. Must equal the server's, else the
/// struct layout may differ and we refuse to proceed.
pub const GSR_KMS_PROTOCOL_VERSION: u32 = 5;
pub const GSR_KMS_MAX_ITEMS: usize = 8;
pub const GSR_KMS_MAX_DMA_BUFS: usize = 4;

// gsr_kms_request_type
pub const KMS_REQUEST_TYPE_REPLACE_CONNECTION: i32 = 0;
pub const KMS_REQUEST_TYPE_GET_KMS: i32 = 1;

// gsr_kms_result
pub const KMS_RESULT_OK: i32 = 0;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KmsRequest {
    pub version: u32,
    pub type_: i32,
    pub new_connection_fd: i32,
}

impl KmsRequest {
    pub fn new(type_: i32, new_connection_fd: i32) -> Self {
        Self {
            version: GSR_KMS_PROTOCOL_VERSION,
            type_,
            new_connection_fd,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `#[repr(C)]` POD with no padding-dependent invariants; reading
        // its bytes for a socket write is well-defined.
        unsafe {
            std::slice::from_raw_parts(
                (self as *const Self).cast::<u8>(),
                std::mem::size_of::<Self>(),
            )
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KmsDmaBuf {
    pub fd: i32,
    pub pitch: u32,
    pub offset: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KmsResponseItem {
    pub dma_buf: [KmsDmaBuf; GSR_KMS_MAX_DMA_BUFS],
    pub num_dma_bufs: i32,
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
    pub modifier: u64,
    pub connector_id: u32,
    /// C `bool`; nonzero = true.
    pub is_cursor: u8,
    pub has_hdr_metadata: u8,
    pub rotation: i32,
    pub x: i32,
    pub y: i32,
    pub src_w: i32,
    pub src_h: i32,
    pub hdr_metadata: [u8; 32],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KmsResponse {
    pub version: u32,
    pub result: i32,
    pub err_msg: [u8; 128],
    pub items: [KmsResponseItem; GSR_KMS_MAX_ITEMS],
    pub num_items: i32,
}

// ABI lock: these must match the C `sizeof` probed against the installed helper.
// A mismatch here means the wire layout drifted and parsing would corrupt.
const _: () = assert!(std::mem::size_of::<KmsRequest>() == 12);
const _: () = assert!(std::mem::size_of::<KmsDmaBuf>() == 12);
const _: () = assert!(std::mem::size_of::<KmsResponseItem>() == 136);
const _: () = assert!(std::mem::size_of::<KmsResponse>() == 1232);

impl KmsResponse {
    /// A zeroed response, ready to be filled by a socket read.
    pub fn zeroed() -> Self {
        // SAFETY: all fields are integers/POD arrays; all-zero is a valid value.
        unsafe { std::mem::zeroed() }
    }

    pub fn as_mut_bytes(&mut self) -> &mut [u8] {
        // SAFETY: `#[repr(C)]` POD; filling its bytes from a socket read is valid.
        unsafe {
            std::slice::from_raw_parts_mut(
                (self as *mut Self).cast::<u8>(),
                std::mem::size_of::<Self>(),
            )
        }
    }

    pub fn err_msg_str(&self) -> String {
        let end = self.err_msg.iter().position(|&b| b == 0).unwrap_or(0);
        String::from_utf8_lossy(&self.err_msg[..end]).into_owned()
    }
}

impl KmsResponseItem {
    pub fn is_cursor(&self) -> bool {
        self.is_cursor != 0
    }
}

/// Distribute the `SCM_RIGHTS` fds across the response's dma-buf slots, in the
/// same order the server packed them: item-major, then dma-buf-minor, bounded by
/// each item's `num_dma_bufs`. Mirrors `recv_msg_from_server` in the C client.
///
/// Returns the number of fds consumed. Errors (returning the fds so the caller
/// can close them) if the response asks for more fds than were received.
pub fn assign_fds(response: &mut KmsResponse, fds: &[RawFd]) -> Result<usize, FdCountMismatch> {
    let needed: usize = response
        .items
        .iter()
        .take(response.num_items.max(0) as usize)
        .map(|it| it.num_dma_bufs.clamp(0, GSR_KMS_MAX_DMA_BUFS as i32) as usize)
        .sum();
    if fds.len() < needed {
        return Err(FdCountMismatch {
            needed,
            got: fds.len(),
        });
    }
    let mut k = 0usize;
    let num_items = response.num_items.max(0) as usize;
    for item in response.items.iter_mut().take(num_items) {
        let n = item.num_dma_bufs.clamp(0, GSR_KMS_MAX_DMA_BUFS as i32) as usize;
        for buf in item.dma_buf.iter_mut().take(n) {
            buf.fd = fds[k];
            k += 1;
        }
    }
    Ok(k)
}

#[derive(Debug, PartialEq, Eq)]
pub struct FdCountMismatch {
    pub needed: usize,
    pub got: usize,
}

/// Pick the framebuffer plane to sample for bias lighting: the largest
/// non-cursor plane, optionally restricted to a specific `connector_id` (0 in
/// the response means "unknown", so it never matches a filter). Returns the
/// index into `response.items`, or `None` if there is no usable plane.
pub fn select_primary_plane(response: &KmsResponse, connector_id: Option<u32>) -> Option<usize> {
    let num_items = response.num_items.max(0) as usize;
    response
        .items
        .iter()
        .take(num_items)
        .enumerate()
        .filter(|(_, it)| !it.is_cursor() && it.num_dma_bufs > 0)
        .filter(|(_, it)| match connector_id {
            Some(want) => it.connector_id == want,
            None => true,
        })
        .max_by_key(|(_, it)| (it.width as u64) * (it.height as u64))
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(
        width: u32,
        height: u32,
        is_cursor: bool,
        connector: u32,
        num_bufs: i32,
    ) -> KmsResponseItem {
        let mut it: KmsResponseItem = unsafe { std::mem::zeroed() };
        it.width = width;
        it.height = height;
        it.is_cursor = is_cursor as u8;
        it.connector_id = connector;
        it.num_dma_bufs = num_bufs;
        it
    }

    fn response(items: &[KmsResponseItem]) -> KmsResponse {
        let mut r = KmsResponse::zeroed();
        r.version = GSR_KMS_PROTOCOL_VERSION;
        r.result = KMS_RESULT_OK;
        r.num_items = items.len() as i32;
        for (dst, src) in r.items.iter_mut().zip(items) {
            *dst = *src;
        }
        r
    }

    #[test]
    fn request_bytes_have_c_layout() {
        let req = KmsRequest::new(KMS_REQUEST_TYPE_GET_KMS, 0);
        let bytes = req.as_bytes();
        assert_eq!(bytes.len(), 12);
        assert_eq!(&bytes[0..4], &GSR_KMS_PROTOCOL_VERSION.to_ne_bytes());
        assert_eq!(&bytes[4..8], &KMS_REQUEST_TYPE_GET_KMS.to_ne_bytes());
    }

    #[test]
    fn assign_fds_maps_item_major_then_dma_minor() {
        let mut r = response(&[item(2560, 1440, false, 1, 2), item(64, 64, true, 1, 1)]);
        // 2 bufs for the primary + 1 for the cursor = 3 fds.
        let used = assign_fds(&mut r, &[10, 11, 12]).expect("enough fds");
        assert_eq!(used, 3);
        assert_eq!(r.items[0].dma_buf[0].fd, 10);
        assert_eq!(r.items[0].dma_buf[1].fd, 11);
        assert_eq!(r.items[1].dma_buf[0].fd, 12);
    }

    #[test]
    fn assign_fds_rejects_too_few() {
        let mut r = response(&[item(2560, 1440, false, 1, 2)]);
        let err = assign_fds(&mut r, &[10]).expect_err("not enough fds");
        assert_eq!(err, FdCountMismatch { needed: 2, got: 1 });
    }

    #[test]
    fn select_primary_prefers_largest_non_cursor() {
        let r = response(&[
            item(64, 64, true, 1, 1),      // cursor
            item(1920, 1080, false, 1, 1), // overlay-ish, smaller
            item(2560, 1440, false, 1, 1), // primary, largest
        ]);
        assert_eq!(select_primary_plane(&r, None), Some(2));
    }

    #[test]
    fn select_primary_honors_connector_filter() {
        let r = response(&[
            item(2560, 1440, false, 1, 1),
            item(3840, 2160, false, 2, 1), // bigger, but different connector
        ]);
        assert_eq!(select_primary_plane(&r, Some(1)), Some(0));
    }

    #[test]
    fn select_primary_none_when_only_cursor() {
        let r = response(&[item(64, 64, true, 1, 1)]);
        assert_eq!(select_primary_plane(&r, None), None);
    }

    #[test]
    fn select_primary_skips_planes_without_dma_bufs() {
        let r = response(&[item(2560, 1440, false, 1, 0)]);
        assert_eq!(select_primary_plane(&r, None), None);
    }
}
