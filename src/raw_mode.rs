use anyhow::{Result, anyhow};

#[cfg(unix)]
pub struct RawModeGuard {
    fd: libc::c_int,
    original: libc::termios,
}

#[cfg(unix)]
impl RawModeGuard {
    pub fn enter(fd: libc::c_int) -> Result<Self> {
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: tcgetattr initializes the provided termios pointer on success.
        if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
            return Err(anyhow!(
                "tcgetattr failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: tcgetattr succeeded, so original is initialized.
        let original = unsafe { original.assume_init() };
        let mut raw = original;
        // SAFETY: raw points to a valid local termios value.
        unsafe { libc::cfmakeraw(std::ptr::addr_of_mut!(raw)) };
        // SAFETY: fd is supplied by the caller and raw is initialized.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, std::ptr::addr_of!(raw)) } != 0 {
            return Err(anyhow!(
                "tcsetattr failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self { fd, original })
    }
}

#[cfg(unix)]
impl Drop for RawModeGuard {
    fn drop(&mut self) {
        // SAFETY: original was captured from this fd and remains initialized.
        let _ =
            unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, std::ptr::addr_of!(self.original)) };
    }
}

#[cfg(unix)]
pub fn terminal_size(fd: libc::c_int) -> (u16, u16) {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: size is a valid writable winsize pointer for this ioctl.
    let result = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) };
    if result == 0 && size.ws_row > 0 && size.ws_col > 0 {
        (size.ws_row, size.ws_col)
    } else {
        (24, 80)
    }
}
