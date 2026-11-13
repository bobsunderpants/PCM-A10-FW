//! Device discovery and mass-storage mount resolution.

use std::fs;
use std::path::{Path, PathBuf};

use crate::dev_info::MscDevInfo;
use crate::error::Error;
use crate::scsi::{self, Inquiry, Transport};

/// A probed target: INQUIRY and DEVINFO succeeded, so this is a supported
/// Sony recorder.
#[derive(Debug, Clone)]
pub struct Probe {
    /// SCSI pass-through path (`/dev/sgN` or `\\.\X:`).
    pub scsi_path: PathBuf,
    /// Mass-storage mount root, when it could be resolved.
    pub mount_root: Option<PathBuf>,
    /// Standard INQUIRY response.
    pub inquiry: Inquiry,
    /// Sony DEVINFO response.
    pub dev_info: MscDevInfo,
}

/// A probed target together with its open SCSI pass-through handle.
pub type Target = (Probe, Box<dyn Transport>);

/// Open the platform SCSI pass-through transport for a path.
pub fn open_transport(path: &Path) -> Result<Box<dyn Transport>, Error> {
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(crate::scsi_linux::LinuxTransport::open(path)?))
    }
    #[cfg(windows)]
    {
        Ok(Box::new(crate::scsi_windows::WindowsTransport::open(path)?))
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = path;
        Err(Error::Unsupported(
            "SCSI pass-through is only implemented for Linux and Windows".into(),
        ))
    }
}

/// Probe one explicit device path.
///
/// This performs the same sequence as the DLL's device matcher: INQUIRY,
/// vendor/product checks, TEST UNIT READY and REQUEST DEVINFO.
pub fn probe_path(path: &Path, mount_override: Option<&Path>) -> Result<Target, Error> {
    let mut transport = open_transport(path)?;
    let inquiry = scsi::inquiry(transport.as_mut())?;
    scsi::check_supported(&inquiry)?;
    scsi::test_unit_ready(transport.as_mut())?;
    let dev_info = scsi::request_devinfo(transport.as_mut())?;
    let mount_root = mount_override
        .map(Path::to_path_buf)
        .or_else(|| mount_root_for(path));
    Ok((
        Probe {
            scsi_path: path.to_path_buf(),
            mount_root,
            inquiry,
            dev_info,
        },
        transport,
    ))
}

/// Enumerate candidate SCSI generic paths.
#[cfg(target_os = "linux")]
pub fn candidate_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/class/scsi_generic") {
        let mut names: Vec<String> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("sg"))
            .collect();
        names.sort_by_key(|name| {
            name.trim_start_matches("sg")
                .parse::<u32>()
                .unwrap_or(u32::MAX)
        });
        for name in names {
            let path = PathBuf::from("/dev").join(name);
            if path.exists() {
                out.push(path);
            }
        }
    }
    if out.is_empty() {
        if let Ok(entries) = fs::read_dir("/dev") {
            let mut names: Vec<String> = entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with("sg"))
                .collect();
            names.sort();
            for name in names {
                out.push(PathBuf::from("/dev").join(name));
            }
        }
    }
    out
}

/// Enumerate candidate SCSI paths on Windows.
#[cfg(windows)]
pub fn candidate_paths() -> Vec<PathBuf> {
    let mask = crate::scsi_windows::logical_drives();
    let mut removable = Vec::new();
    let mut fixed = Vec::new();
    for index in 0..26u8 {
        if mask & (1 << index) == 0 {
            continue;
        }
        let letter = (b'A' + index) as char;
        let root = format!("{letter}:\\");
        let path = PathBuf::from(format!(r"\\.\{letter}:"));
        match crate::scsi_windows::drive_type(&root) {
            2 => removable.push(path), // DRIVE_REMOVABLE
            3 => fixed.push(path),     // DRIVE_FIXED
            _ => {}
        }
    }
    removable.extend(fixed);
    removable
}

/// Enumerate candidates on unsupported platforms.
#[cfg(not(any(target_os = "linux", windows)))]
pub fn candidate_paths() -> Vec<PathBuf> {
    Vec::new()
}

/// Resolve the mount root of a SCSI generic device on Linux.
///
/// `/sys/class/scsi_generic/sgN/device` and `/sys/class/block/sdX/device`
/// both point at the same SCSI device directory.  Mount sources from
/// `/proc/self/mounts` are matched against that directory.
#[cfg(target_os = "linux")]
pub fn mount_root_for(scsi_path: &Path) -> Option<PathBuf> {
    let name = scsi_path.file_name()?.to_str()?;
    let device_link = PathBuf::from("/sys/class/scsi_generic")
        .join(name)
        .join("device");
    let scsi_device = fs::canonicalize(device_link).ok()?;
    let mounts = fs::read_to_string("/proc/self/mounts").ok()?;
    let mut candidates = Vec::new();
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let Some(source) = fields.next() else {
            continue;
        };
        let Some(target) = fields.next() else {
            continue;
        };
        if !source.starts_with("/dev/") {
            continue;
        }
        let Some(base) = Path::new(source).file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if block_device_matches(base, &scsi_device) {
            candidates.push(PathBuf::from(unescape_mount(target)));
        }
    }
    candidates.sort();
    candidates.into_iter().next()
}

/// Check whether a `/sys/class/block` name belongs to the given SCSI device.
///
/// A mounted mass-storage volume is usually a partition (`sda1`), whose
/// sysfs directory has no `device` link of its own.  Walk up one level to the
/// parent disk (`sda`) in that case.
#[cfg(target_os = "linux")]
fn block_device_matches(base: &str, scsi_device: &Path) -> bool {
    let Ok(block) = fs::canonicalize(PathBuf::from("/sys/class/block").join(base)) else {
        return false;
    };
    let mut candidates = vec![block.join("device")];
    if let Some(parent) = block.parent() {
        candidates.push(parent.join("device"));
    }
    candidates.into_iter().any(|candidate| {
        fs::canonicalize(candidate)
            .map(|device| device == scsi_device)
            .unwrap_or(false)
    })
}

/// Resolve the mount root of a Windows volume handle.
#[cfg(windows)]
pub fn mount_root_for(scsi_path: &Path) -> Option<PathBuf> {
    let text = scsi_path.to_string_lossy();
    let rest = text.strip_prefix(r"\\.\")?;
    let letter = rest.chars().next()?.to_ascii_uppercase();
    if letter.is_ascii_alphabetic() {
        Some(PathBuf::from(format!("{letter}:\\")))
    } else {
        None
    }
}

/// No mount resolution on unsupported platforms.
#[cfg(not(any(target_os = "linux", windows)))]
pub fn mount_root_for(_scsi_path: &Path) -> Option<PathBuf> {
    None
}

/// Decode the octal escapes used by `/proc/self/mounts`.
#[cfg(target_os = "linux")]
fn unescape_mount(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 3 < bytes.len() {
            let digits = &bytes[index + 1..index + 4];
            if digits.iter().all(|b| (b'0'..=b'7').contains(b)) {
                let value = (digits[0] - b'0') as u32 * 64
                    + (digits[1] - b'0') as u32 * 8
                    + (digits[2] - b'0') as u32;
                out.push(value as u8);
                index += 4;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Discover every supported device.
///
/// The returned transport is kept open so the caller can reuse it for
/// flashing.  Callers that only list devices can drop it.
pub fn discover(mount_override: Option<&Path>, verbose: bool) -> Result<Vec<Target>, Error> {
    let paths = candidate_paths();
    let mut found = Vec::new();
    let mut denied = Vec::new();
    let mut last_error: Option<Error> = None;

    for path in paths {
        let mut transport = match open_transport(&path) {
            Ok(transport) => transport,
            Err(error) => {
                if error.is_permission_denied() {
                    denied.push(path.clone());
                }
                if verbose {
                    eprintln!("skip {}: {error}", path.display());
                }
                last_error = Some(error);
                continue;
            }
        };

        let inquiry = match scsi::inquiry(transport.as_mut()) {
            Ok(inquiry) => inquiry,
            Err(error) => {
                if verbose {
                    eprintln!("skip {}: {error}", path.display());
                }
                last_error = Some(error);
                continue;
            }
        };
        match scsi::check_supported(&inquiry) {
            Ok(()) => {}
            Err(Error::NotSonyDevice { .. }) => continue,
            Err(error) => {
                if verbose {
                    eprintln!("skip {}: {error}", path.display());
                }
                continue;
            }
        }
        if let Err(error) = scsi::test_unit_ready(transport.as_mut()) {
            if verbose {
                eprintln!("skip {}: TEST UNIT READY: {error}", path.display());
            }
            continue;
        }
        let dev_info = match scsi::request_devinfo(transport.as_mut()) {
            Ok(info) => info,
            Err(error) => {
                if verbose {
                    eprintln!("skip {}: REQUEST DEVINFO: {error}", path.display());
                }
                continue;
            }
        };
        let mount_root = mount_override
            .map(Path::to_path_buf)
            .or_else(|| mount_root_for(&path));
        found.push((
            Probe {
                scsi_path: path,
                mount_root,
                inquiry,
                dev_info,
            },
            transport,
        ));
    }

    if found.is_empty() {
        if !denied.is_empty() {
            let list = denied
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(Error::Usage(format!(
                "permission denied opening {list}; run as root or add a udev rule granting access"
            )));
        }
        return Err(match last_error {
            Some(error) => Error::NotFound(format!("no supported Sony recorder found ({error})")),
            None => Error::NotFound("no supported Sony recorder found".into()),
        });
    }
    Ok(found)
}

/// Resolve one target, either from an explicit path or by discovery.
pub fn resolve(
    device: Option<&Path>,
    mount_override: Option<&Path>,
    verbose: bool,
) -> Result<Target, Error> {
    if let Some(path) = device {
        return probe_path(path, mount_override);
    }
    let mut targets = discover(mount_override, verbose)?;
    if targets.len() == 1 {
        Ok(targets.remove(0))
    } else {
        Err(Error::Ambiguous(
            targets
                .iter()
                .map(|(probe, _)| probe.scsi_path.display().to_string())
                .collect(),
        ))
    }
}

/// Free bytes available on the filesystem containing `path`.
#[cfg(target_os = "linux")]
pub fn available_bytes(path: &Path) -> Option<u64> {
    #[cfg(target_pointer_width = "64")]
    {
        use std::ffi::CString;
        use std::os::raw::{c_char, c_int};
        use std::os::unix::ffi::OsStrExt;

        #[repr(C)]
        struct StatVfs {
            f_bsize: u64,
            f_frsize: u64,
            f_blocks: u64,
            f_bfree: u64,
            f_bavail: u64,
            f_files: u64,
            f_ffree: u64,
            f_favail: u64,
            f_fsid: u64,
            f_flag: u64,
            f_namemax: u64,
            f_type: u32,
            f_spare: [i32; 5],
        }

        extern "C" {
            fn statvfs(path: *const c_char, buf: *mut StatVfs) -> c_int;
        }

        let path = CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut buffer = std::mem::MaybeUninit::<StatVfs>::zeroed();
        let result = unsafe { statvfs(path.as_ptr(), buffer.as_mut_ptr()) };
        if result != 0 {
            return None;
        }
        let buffer = unsafe { buffer.assume_init() };
        Some(buffer.f_bavail.saturating_mul(buffer.f_frsize))
    }
    #[cfg(not(target_pointer_width = "64"))]
    {
        let _ = path;
        None
    }
}

/// Free bytes available on Windows.
#[cfg(windows)]
pub fn available_bytes(path: &Path) -> Option<u64> {
    crate::scsi_windows::available_bytes(path)
}

/// Free-space probing is not implemented on other platforms.
#[cfg(not(any(target_os = "linux", windows)))]
pub fn available_bytes(_path: &Path) -> Option<u64> {
    None
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::unescape_mount;

    #[test]
    fn decodes_proc_mounts_escapes() {
        assert_eq!(unescape_mount(r"/media/My\040Disk"), "/media/My Disk");
        assert_eq!(unescape_mount(r"/run/media/a\134b"), r"/run/media/a\b");
        assert_eq!(unescape_mount("/plain"), "/plain");
    }
}
