//! Memory backends: default POSIX mmap shared memory, with an alternative
//! SharedArrayBuffer backend handled in the napi layer.
//!
//! Every backend yields a `base` pointer + `capacity`; the arena logic above is
//! backend-agnostic.

/// A mapped shared-memory region.
pub struct MappedRegion {
    pub base: *mut u8,
    pub capacity: usize,
    #[cfg(unix)]
    fd: i32,
    #[cfg(unix)]
    name: Option<String>,
}

// MappedRegion is intentionally shared across isolates; it is Send + Sync.
unsafe impl Send for MappedRegion {}
unsafe impl Sync for MappedRegion {}

#[cfg(unix)]
impl Drop for MappedRegion {
    fn drop(&mut self) {
        if !self.base.is_null() {
            unsafe {
                libc::munmap(self.base as *mut libc::c_void, self.capacity);
            }
        }
        if self.fd >= 0 {
            unsafe {
                libc::close(self.fd);
            }
        }
        // Deliberately do NOT shm_unlink here: a region may be shared by many
        // isolates/threads, and unlinking on a single Drop would break the
        // other holders. Unlinking is an explicit `unlink_backing()` (see the
        // `close()` method) performed after all workers have attached.
    }
}

impl MappedRegion {
    /// Remove the POSIX name so no further attachments can be made. Existing
    /// mmaps stay valid (kernel keeps the pages until the last map is closed).
    /// Safe; idempotent.
    #[cfg(unix)]
    pub fn unlink_backing(&mut self) {
        if let Some(name) = self.name.take() {
            let cname = std::ffi::CString::new(name).unwrap_or_default();
            unsafe {
                libc::shm_unlink(cname.as_ptr());
            }
        }
    }

    /// Non-unix no-op.
    #[cfg(not(unix))]
    pub fn unlink_backing(&mut self) {}
}

#[cfg(not(unix))]
impl Drop for MappedRegion {
    fn drop(&mut self) {}
}

fn os_error(ctx: &str) -> String {
    format!("{}: {}", ctx, std::io::Error::last_os_error())
}

fn shm_path(token: &str) -> Result<std::ffi::CString, String> {
    if token.is_empty() || token.contains('/') || token.len() > 60 {
        return Err("invalid id: must be a short alphanumeric token".to_string());
    }
    let name = format!("/shared_nothing_{}", token);
    let cname = std::ffi::CString::new(name.as_str()).map_err(|_| "invalid id".to_string())?;
    Ok(cname)
}

/// Map an existing or new (`create`) POSIX shared memory segment.
#[cfg(unix)]
unsafe fn map_shm(token: &str, size: usize, create: bool) -> std::io::Result<(i32, *mut u8)> {
    use std::io;
    let cname = shm_path(token).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let mut flags = libc::O_RDWR;
    if create {
        flags |= libc::O_CREAT | libc::O_EXCL;
    }
    let fd = libc::shm_open(cname.as_ptr() as *const _, flags, 0o600);
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    if create && libc::ftruncate(fd, size as libc::off_t) != 0 {
        let e = io::Error::last_os_error();
        libc::close(fd);
        return Err(e);
    }
    let ptr = libc::mmap(
        std::ptr::null_mut(),
        size,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_SHARED,
        fd,
        0,
    );
    if ptr == libc::MAP_FAILED {
        let e = io::Error::last_os_error();
        libc::close(fd);
        return Err(e);
    }
    Ok((fd, ptr as *mut u8))
}

/// Create a new region with the given size (default backend). Initializes it.
///
/// # Safety
/// Caller ensures the returned region is used on a matching OS.
#[cfg(unix)]
pub unsafe fn create_mmap(token: &str, size: usize) -> Result<MappedRegion, String> {
    let size = if size < 64 { 1 << 20 } else { size };
    let (fd, base) = map_shm(token, size, true).map_err(|_| os_error("mmap create"))?;
    crate::arena::ensure_init(base, size, 1);
    Ok(MappedRegion {
        base,
        capacity: size,
        fd,
        name: Some(format!("/shared_nothing_{}", token)),
    })
}

/// Attach (reopen) an existing region by its id. Doesn't reinitialize.
///
/// # Safety
/// The caller must have a matching id created earlier (same process).
#[cfg(unix)]
pub unsafe fn attach_mmap(token: &str) -> Result<MappedRegion, String> {
    // Determine size by stat the shm fd after open.
    let cname = shm_path(token).map_err(|e| e)?;
    let fd = libc::shm_open(cname.as_ptr() as *const _, libc::O_RDWR, 0o600);
    if fd < 0 {
        return Err(os_error("shm_open attach"));
    }
    let mut st: libc::stat = std::mem::zeroed();
    if libc::fstat(fd, &mut st) != 0 {
        let e = os_error("fstat");
        libc::close(fd);
        return Err(e);
    }
    let size = st.st_size as usize;
    let ptr = libc::mmap(
        std::ptr::null_mut(),
        size,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_SHARED,
        fd,
        0,
    );
    if ptr == libc::MAP_FAILED {
        let e = os_error("mmap attach");
        libc::close(fd);
        return Err(e);
    }
    Ok(MappedRegion {
        base: ptr as *mut u8,
        capacity: size,
        fd,
        name: Some(format!("/shared_nothing_{}", token)),
    })
}

#[cfg(not(unix))]
pub unsafe fn create_mmap(_token: &str, _size: usize) -> Result<MappedRegion, String> {
    Err("mmap backend currently requires a unix platform".to_string())
}
#[cfg(not(unix))]
pub unsafe fn attach_mmap(_token: &str) -> Result<MappedRegion, String> {
    Err("mmap backend currently requires a unix platform".to_string())
}