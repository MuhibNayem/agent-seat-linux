//! Frame capture for the agent seat.
//!
//! The proxied app renders into buffers it attaches to its surfaces. For
//! software-rendered apps (GDK_BACKEND/GSK_RENDERER forced to cairo) those are
//! `wl_shm` buffers: a shared-memory pool plus offset/stride/format. We read
//! the pixels straight out of the pool's file descriptor — no compositor
//! round-trip, no portal, no consent, and independent of which window is in
//! the foreground. Capture remains bound to the controlled app instead of the
//! ambient desktop.

use std::os::fd::AsRawFd;

use super::proxy::Conn;

/// wl_shm formats we can decode (little-endian, so ARGB8888 is B-G-R-A bytes).
const WL_SHM_FORMAT_ARGB8888: u32 = 0;
const WL_SHM_FORMAT_XRGB8888: u32 = 1;

/// One decoded frame plus the surface it came from.
#[derive(Debug)]
pub struct CapturedFrame {
    /// Decoded RGB application frame.
    pub image: image::DynamicImage,
    /// Buffer dimensions (also available via `image.width()/height()`).
    #[allow(dead_code)]
    pub width: u32,
    /// Captured buffer height in pixels.
    #[allow(dead_code)]
    pub height: u32,
}

/// Geometry + pool of an shm buffer (everything needed to read it back).
struct ShmFrameRef<'a> {
    fd: &'a std::os::fd::OwnedFd,
    pool_size: i32,
    offset: i32,
    width: i32,
    height: i32,
    stride: i32,
    format: u32,
}

/// Pick the surface most likely to be the app's main window: the largest
/// attached shm-backed buffer, using commit count only to break ties. Startup
/// icons and splash helpers can commit many more times than the eventual main
/// window, so commit count alone can permanently select a tiny
/// non-interactive surface.
///
/// Returns the shm fields directly (dmabuf buffers are skipped — they need a
/// GPU readback path).
fn primary_shm_frame(conn: &Conn) -> Option<ShmFrameRef<'_>> {
    let mut best: Option<(i64, u64, ShmFrameRef<'_>)> = None;
    for state in conn.surfaces.values() {
        let Some(attached) = state.attached.as_ref() else {
            continue;
        };
        let frame = ShmFrameRef {
            fd: &attached.fd,
            pool_size: attached.pool_size,
            offset: attached.offset,
            width: attached.width,
            height: attached.height,
            stride: attached.stride,
            format: attached.format,
        };
        let area = i64::from(attached.width.max(0)) * i64::from(attached.height.max(0));
        match &best {
            Some((best_area, best_commits, _))
                if (*best_area, *best_commits) >= (area, state.commit_count) => {}
            _ => best = Some((area, state.commit_count, frame)),
        }
    }
    best.map(|(_, _, frame)| frame)
}

/// Read the app's current frame from its attached shm buffer.
///
/// `Conn` is passed under its lock guard; this performs no locking itself.
pub(crate) fn capture_frame(conn: &Conn) -> Result<CapturedFrame, String> {
    let frame = primary_shm_frame(conn).ok_or_else(|| {
        "the app has no readable shm frame yet (it may use GPU buffers)".to_string()
    })?;
    let ShmFrameRef {
        fd,
        pool_size,
        offset,
        width,
        height,
        stride,
        format,
    } = frame;

    if offset < 0 || width <= 0 || height <= 0 || stride <= 0 || pool_size <= 0 {
        return Err(format!(
            "invalid shm frame offset={offset}, geometry={width}x{height}, stride={stride}, pool={pool_size}"
        ));
    }

    let need = (offset as i64) + (stride as i64) * (height as i64);
    if need > pool_size as i64 {
        return Err(format!(
            "frame ({need} bytes) exceeds shm pool size ({} bytes)",
            pool_size
        ));
    }

    // Map the pool read-only and copy the frame out.
    let fd = fd.as_raw_fd();
    // SAFETY: fd is an owned, open shm-pool descriptor and pool_size was
    // validated as positive. The returned mapping is checked before use.
    let mapped = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            pool_size as usize,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    if mapped == libc::MAP_FAILED {
        return Err(format!(
            "mmap of the shm pool failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    let result = decode_frame(mapped as *const u8, offset, width, height, stride, format);

    // SAFETY: mapped came from the successful mmap above and is unmapped once
    // with the identical length after decode_frame has finished reading it.
    unsafe {
        libc::munmap(mapped, pool_size as usize);
    }

    let image = result?;
    Ok(CapturedFrame {
        width: width as u32,
        height: height as u32,
        image,
    })
}

/// Decode B-G-R-(A/X) rows into an RGB image.
fn decode_frame(
    base: *const u8,
    offset: i32,
    width: i32,
    height: i32,
    stride: i32,
    format: u32,
) -> Result<image::DynamicImage, String> {
    match format {
        WL_SHM_FORMAT_ARGB8888 | WL_SHM_FORMAT_XRGB8888 => {}
        other => {
            return Err(format!(
                "unsupported wl_shm format {other:#x} (only ARGB8888/XRGB8888 are decoded)"
            ))
        }
    }

    let w = width as usize;
    let h = height as usize;
    let stride = stride as usize;
    let mut rgb = vec![0u8; w * h * 3];
    for y in 0..h {
        // SAFETY: the caller validated offset + stride * height against the
        // mapped pool size. y is below height and the slice is capped to one
        // validated row.
        let row = unsafe {
            std::slice::from_raw_parts(base.add(offset as usize + y * stride), stride.min(w * 4))
        };
        for x in 0..w {
            let px = x * 4;
            if px + 2 >= row.len() {
                break;
            }
            // Little-endian ARGB/XRGB => bytes are B, G, R, (A/X).
            let b = row[px];
            let g = row[px + 1];
            let r = row[px + 2];
            let dst = (y * w + x) * 3;
            rgb[dst] = r;
            rgb[dst + 1] = g;
            rgb[dst + 2] = b;
        }
    }
    let img = image::RgbImage::from_raw(w as u32, h as u32, rgb)
        .ok_or_else(|| "failed to assemble the captured frame".to_string())?;
    Ok(image::DynamicImage::ImageRgb8(img))
}
