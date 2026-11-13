//! Windows SCSI pass-through transport using `IOCTL_SCSI_PASS_THROUGH_DIRECT`.
//!
//! The shipped updater uses the same SPTI interface.  Opening a volume for
//! pass-through requires administrator privileges.

use std::ffi::{c_void, OsStr};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::Error;
use crate::scsi::{parse_sense, DataDirection, ScsiStatus, Transport};

const IOCTL_SCSI_PASS_THROUGH_DIRECT: u32 = 0x0004d014;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_SHARE_READ: u32 = 0x0000_0001;
const FILE_SHARE_WRITE: u32 = 0x0000_0002;
const OPEN_EXISTING: u32 = 3;
const SCSI_IOCTL_DATA_OUT: u8 = 0;
const SCSI_IOCTL_DATA_IN: u8 = 1;
const SCSI_IOCTL_DATA_UNSPECIFIED: u8 = 2;
const SENSE_LEN: usize = 32;

const INVALID_HANDLE_VALUE: *mut c_void = usize::MAX as *mut c_void;

#[repr(C)]
struct ScsiPassThroughDirect {
    length: u16,
    scsi_status: u8,
    path_id: u8,
    target_id: u8,
    lun: u8,
    cdb_length: u8,
    sense_info_length: u8,
    data_in: u8,
    data_transfer_length: u32,
    timeout_value: u32,
    data_buffer: *mut c_void,
    sense_info_offset: u32,
    cdb: [u8; 16],
}

extern "system" {
    fn CreateFileW(
        file_name: *const u16,
        desired_access: u32,
        share_mode: u32,
        security_attributes: *mut c_void,
        creation_disposition: u32,
        flags_and_attributes: u32,
        template_file: *mut c_void,
    ) -> *mut c_void;
    fn DeviceIoControl(
        device: *mut c_void,
        control_code: u32,
        in_buffer: *mut c_void,
        in_buffer_size: u32,
        out_buffer: *mut c_void,
        out_buffer_size: u32,
        bytes_returned: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn GetLogicalDrives() -> u32;
    fn GetDriveTypeW(root_path_name: *const u16) -> u32;
    fn GetDiskFreeSpaceExW(
        directory_name: *const u16,
        free_bytes_available_to_caller: *mut u64,
        total_number_of_bytes: *mut u64,
        total_number_of_free_bytes: *mut u64,
    ) -> i32;
}

struct Handle(*mut c_void);

// The handle is only used from one thread at a time; moving it to the update
// worker thread is safe and matches the DLL's worker-thread design.
unsafe impl Send for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE && !self.0.is_null() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// Windows SPTI transport.
pub struct WindowsTransport {
    handle: Handle,
    path: PathBuf,
}

/// Encode a string as a NUL-terminated UTF-16 buffer.
pub(crate) fn wide_str(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Encode an `OsStr` as a NUL-terminated UTF-16 buffer.
pub(crate) fn wide_os(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

/// Normalize a drive path to the `\\.\X:` form used by SPTI.
pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text.starts_with(r"\\.\") {
        return path.to_path_buf();
    }
    let bytes = text.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        let letter = (bytes[0] as char).to_ascii_uppercase();
        return PathBuf::from(format!(r"\\.\{letter}:"));
    }
    path.to_path_buf()
}

/// Bitmask of currently visible drive letters.
pub(crate) fn logical_drives() -> u32 {
    unsafe { GetLogicalDrives() }
}

/// `GetDriveTypeW` for a root such as `D:\`.
pub(crate) fn drive_type(root: &str) -> u32 {
    let wide = wide_str(root);
    unsafe { GetDriveTypeW(wide.as_ptr()) }
}

/// Free bytes available to the caller for a directory.
pub(crate) fn available_bytes(path: &Path) -> Option<u64> {
    let wide = wide_os(path.as_os_str());
    let mut free = 0u64;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok != 0 {
        Some(free)
    } else {
        None
    }
}

impl WindowsTransport {
    /// Open a volume for SPTI.
    pub fn open(path: &Path) -> Result<Self, Error> {
        let normalized = normalize_path(path);
        let wide = wide_os(normalized.as_os_str());
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            let error = io::Error::last_os_error();
            return Err(Error::Io(io::Error::new(
                error.kind(),
                format!(
                    "CreateFileW({}) failed: {error}; administrator privileges are required for SPTI",
                    normalized.display()
                ),
            )));
        }
        Ok(Self {
            handle: Handle(handle),
            path: normalized,
        })
    }
}

impl Transport for WindowsTransport {
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
        let header_size = std::mem::size_of::<ScsiPassThroughDirect>();
        let total = header_size + SENSE_LEN;
        // `Vec<u64>` guarantees the alignment the structure needs.
        let words = (total + 7) / 8;
        let mut storage = vec![0u64; words];
        let base = storage.as_mut_ptr() as *mut u8;

        let data_len = data.as_ref().map_or(0, |buffer| buffer.len());
        let data_ptr = match direction {
            DataDirection::None => std::ptr::null_mut(),
            _ => data.as_ref().map_or(std::ptr::null_mut(), |buffer| {
                buffer.as_ptr() as *mut c_void
            }),
        };
        let data_in = match direction {
            DataDirection::FromDevice => SCSI_IOCTL_DATA_IN,
            DataDirection::ToDevice => SCSI_IOCTL_DATA_OUT,
            DataDirection::None => SCSI_IOCTL_DATA_UNSPECIFIED,
        };
        let timeout_seconds = timeout.as_secs().max(1).min(u32::MAX as u64) as u32;

        let mut header = ScsiPassThroughDirect {
            length: header_size as u16,
            scsi_status: 0,
            path_id: 0,
            target_id: 0,
            lun: 0,
            cdb_length: cdb.len() as u8,
            sense_info_length: SENSE_LEN as u8,
            data_in,
            data_transfer_length: data_len as u32,
            timeout_value: timeout_seconds,
            data_buffer: data_ptr,
            sense_info_offset: header_size as u32,
            cdb: [0u8; 16],
        };
        header.cdb[..cdb.len()].copy_from_slice(cdb);
        unsafe {
            std::ptr::write(base as *mut ScsiPassThroughDirect, header);
        }

        let mut returned = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                self.handle.0,
                IOCTL_SCSI_PASS_THROUGH_DIRECT,
                base as *mut c_void,
                total as u32,
                base as *mut c_void,
                total as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let error = io::Error::last_os_error();
            return Err(Error::Io(io::Error::new(
                error.kind(),
                format!(
                    "IOCTL_SCSI_PASS_THROUGH_DIRECT on {} failed: {error}",
                    self.path.display()
                ),
            )));
        }

        let header = unsafe { std::ptr::read(base as *const ScsiPassThroughDirect) };
        let sense_bytes = unsafe { std::slice::from_raw_parts(base.add(header_size), SENSE_LEN) };
        let sense = parse_sense(sense_bytes);
        let transferred = if header.scsi_status == 0 { data_len } else { 0 };
        Ok(ScsiStatus {
            status: header.scsi_status,
            sense,
            transferred,
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}
