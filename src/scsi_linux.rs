//! Linux SCSI pass-through transport using the `SG_IO` ioctl.
//!
//! The device node is normally `/dev/sgN`; write access is required for
//! vendor commands, so the updater must run as root or with a suitable udev
//! rule.  The ioctl and structure definitions come from `<scsi/sg.h>`.

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::raw::{c_int, c_ulong};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::Error;
use crate::scsi::{parse_sense, DataDirection, ScsiStatus, Transport};

/// `SG_IO` request number.
const SG_IO: c_ulong = 0x2285;
/// `SG_DXFER_NONE` from `<scsi/sg.h>`.
const SG_DXFER_NONE: c_int = -1;
/// `SG_DXFER_TO_DEV`.
const SG_DXFER_TO_DEV: c_int = -2;
/// `SG_DXFER_FROM_DEV`.
const SG_DXFER_FROM_DEV: c_int = -3;
/// Sense buffer size; enough for the fixed-format response.
const SENSE_LEN: usize = 32;
/// `EINTR`.
const EINTR: i32 = 4;

#[repr(C)]
struct SgIoHdr {
    interface_id: c_int,
    dxfer_direction: c_int,
    cmd_len: u8,
    mx_sb_len: u8,
    iovec_count: u16,
    dxfer_len: u32,
    dxferp: *mut c_void,
    cmdp: *mut u8,
    sbp: *mut u8,
    timeout: u32,
    flags: u32,
    pack_id: i32,
    usr_ptr: *mut c_void,
    status: u8,
    masked_status: u8,
    msg_status: u8,
    sb_len_wr: u8,
    host_status: u16,
    driver_status: u16,
    resid: c_int,
    duration: u32,
    info: u32,
}

extern "C" {
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
}

/// Linux SG_IO transport.
pub struct LinuxTransport {
    file: File,
    path: PathBuf,
}

impl LinuxTransport {
    /// Open a SCSI generic device read-write.
    pub fn open(path: &Path) -> Result<Self, Error> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| {
                if e.kind() == io::ErrorKind::PermissionDenied {
                    Error::Io(io::Error::new(
                        e.kind(),
                        format!(
                            "cannot open {} read-write: {e}; run as root or add a udev rule",
                            path.display()
                        ),
                    ))
                } else {
                    Error::Io(io::Error::new(
                        e.kind(),
                        format!("cannot open {}: {e}", path.display()),
                    ))
                }
            })?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }
}

impl Transport for LinuxTransport {
    fn execute(
        &mut self,
        cdb: &[u8],
        data: Option<&mut [u8]>,
        direction: DataDirection,
        timeout: Duration,
    ) -> Result<ScsiStatus, Error> {
        if cdb.is_empty() || cdb.len() > 16 {
            return Err(Error::Usage(format!(
                "SCSI CDB length {} is outside 1..=16",
                cdb.len()
            )));
        }
        let (dxfer_direction, dxfer_len, dxferp) = match direction {
            DataDirection::None => (SG_DXFER_NONE, 0u32, std::ptr::null_mut()),
            DataDirection::FromDevice | DataDirection::ToDevice => {
                let Some(buf) = data.as_deref() else {
                    return Err(Error::Usage(
                        "a data direction was requested without a buffer".into(),
                    ));
                };
                let dir = if direction == DataDirection::FromDevice {
                    SG_DXFER_FROM_DEV
                } else {
                    SG_DXFER_TO_DEV
                };
                (dir, buf.len() as u32, buf.as_ptr() as *mut c_void)
            }
        };

        let mut sense = [0u8; SENSE_LEN];
        let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        let mut header = SgIoHdr {
            interface_id: b'S' as c_int,
            dxfer_direction,
            cmd_len: cdb.len() as u8,
            mx_sb_len: SENSE_LEN as u8,
            iovec_count: 0,
            dxfer_len,
            dxferp,
            cmdp: cdb.as_ptr() as *mut u8,
            sbp: sense.as_mut_ptr(),
            timeout: timeout_ms,
            flags: 0,
            pack_id: 0,
            usr_ptr: std::ptr::null_mut(),
            status: 0,
            masked_status: 0,
            msg_status: 0,
            sb_len_wr: 0,
            host_status: 0,
            driver_status: 0,
            resid: 0,
            duration: 0,
            info: 0,
        };

        loop {
            let rc = unsafe { ioctl(self.file.as_raw_fd(), SG_IO, &mut header as *mut SgIoHdr) };
            if rc == 0 {
                break;
            }
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(EINTR) {
                continue;
            }
            return Err(Error::Io(io::Error::new(
                err.kind(),
                format!("SG_IO on {} failed: {err}", self.path.display()),
            )));
        }

        let sense = parse_sense(&sense[..header.sb_len_wr as usize]);
        if header.host_status != 0 {
            return Err(Error::scsi(
                "SG_IO",
                header.status,
                sense,
                format!("host status {:#06x}", header.host_status),
            ));
        }
        if header.driver_status != 0 && (header.driver_status & 0x08) == 0 {
            return Err(Error::scsi(
                "SG_IO",
                header.status,
                sense,
                format!("driver status {:#06x}", header.driver_status),
            ));
        }

        let residual = header.resid.max(0) as u32;
        let transferred = dxfer_len.saturating_sub(residual) as usize;
        Ok(ScsiStatus {
            status: header.status,
            sense,
            transferred,
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}
