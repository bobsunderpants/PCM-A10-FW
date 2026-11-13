//! Firmware flashing: copy `UPG.IMG` to the recorder, then send the Sony
//! `START FW UPDATE` vendor command.
//!
//! Observed sequence (see `docs/PROTOCOL.md`):
//!
//! 1. delete an existing image at the destination;
//! 2. check free space;
//! 3. copy the image to the root of the mass-storage volume;
//! 4. report progress 19 (the DLL's `index * 10 + 0x13`);
//! 5. send START FW UPDATE and wait, reporting progress 40..95
//!    (`elapsed * 0x37 / timeout + 0x28`).

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::device::{self, Probe};
use crate::error::Error;
use crate::scsi::{self, Transport};
use crate::sha256;
use crate::update_xml::UpdatePackage;

/// Stage of a flash operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Copying the image to the mass-storage volume.
    Copy,
    /// Waiting for START FW UPDATE to complete.
    Update,
    /// Re-probing the device after the update.
    Verify,
}

impl Stage {
    /// Human-readable name.
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Copy => "copy",
            Stage::Update => "update",
            Stage::Verify => "verify",
        }
    }
}

/// One progress notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// Current stage.
    pub stage: Stage,
    /// Percentage, 0..=100.
    pub percent: u8,
}

/// Options for a flash operation.
#[derive(Debug, Clone)]
pub struct FlashOptions {
    /// Firmware image to copy.
    pub image: PathBuf,
    /// Parsed `SWUpdate.xml`, when available.
    pub package: Option<UpdatePackage>,
    /// Override for `<TimeOut>` (overall deadline).
    pub timeout: Option<Duration>,
    /// Override for `<Timer>` (SCSI command timeout).
    pub timer: Option<Duration>,
    /// Flash even when versions match or the package does not match.
    pub force: bool,
    /// Validate and report without changing anything.
    pub dry_run: bool,
    /// Remove `UPG.IMG` after a successful update.
    pub remove_image: bool,
    /// Re-probe after the update and compare the reported version.
    pub verify: bool,
    /// Reject images without the expected NuttX header.
    pub strict_image: bool,
    /// How long to wait for the device to come back during verification.
    pub verify_timeout: Duration,
}

impl Default for FlashOptions {
    fn default() -> Self {
        Self {
            image: PathBuf::from("UPG.IMG"),
            package: None,
            timeout: None,
            timer: None,
            force: false,
            dry_run: false,
            remove_image: false,
            verify: false,
            strict_image: false,
            verify_timeout: Duration::from_secs(120),
        }
    }
}

/// Everything that will happen, computed before any destructive action.
#[derive(Debug, Clone)]
pub struct FlashPlan {
    /// Source image.
    pub image: PathBuf,
    /// Source size in bytes.
    pub image_size: u64,
    /// SHA-256 of the source image.
    pub image_sha256: String,
    /// Destination on the mass-storage volume.
    pub destination: PathBuf,
    /// Mount root of the mass-storage volume.
    pub mount_root: PathBuf,
    /// SCSI pass-through path.
    pub device_path: PathBuf,
    /// INQUIRY product.
    pub device_product: String,
    /// DEVINFO `ProductId`.
    pub device_product_id: String,
    /// DEVINFO `ProductRevision`.
    pub device_revision: String,
    /// Package version, when an XML file was supplied.
    pub package_version: Option<String>,
    /// `Some(true)` when the package is newer, `Some(false)` when it is not,
    /// `None` when the versions cannot be compared.
    pub version_newer: Option<bool>,
    /// `Some(true)` when the package product matches the device's DEVINFO
    /// `ProductId`, `Some(false)` when it clearly does not, `None` when one of
    /// the two strings is empty.
    pub product_matches: Option<bool>,
    /// Overall update deadline (`<TimeOut>`).
    pub timeout: Duration,
    /// SCSI command timeout (`min(Timer, TimeOut)`).
    pub command_timeout: Duration,
    /// Free bytes on the volume, when known.
    pub free_bytes: Option<u64>,
    /// Non-fatal observations.
    pub warnings: Vec<String>,
}

/// Result of a completed flash.
#[derive(Debug, Clone)]
pub struct FlashOutcome {
    /// The plan that was executed.
    pub plan: FlashPlan,
    /// The image was copied.
    pub copied: bool,
    /// START FW UPDATE returned success.
    pub started: bool,
    /// Wall-clock duration of the copy plus update.
    pub elapsed: Duration,
    /// `UPG.IMG` was removed after the update.
    pub removed_image: bool,
    /// `Some(true)` when post-update verification matched the package.
    pub verified: Option<bool>,
}

/// Compute and validate a flash plan.
pub fn plan(probe: &Probe, options: &FlashOptions) -> Result<FlashPlan, Error> {
    let metadata = fs::metadata(&options.image)
        .map_err(|error| Error::InvalidImage(format!("{}: {error}", options.image.display())))?;
    if !metadata.is_file() {
        return Err(Error::InvalidImage(format!(
            "{} is not a regular file",
            options.image.display()
        )));
    }
    if metadata.len() == 0 {
        return Err(Error::InvalidImage(format!(
            "{} is empty",
            options.image.display()
        )));
    }

    let (image_size, digest) = sha256::hash_file(&options.image)?;
    let image_sha256 = sha256::to_hex(&digest);
    let mut warnings = Vec::new();

    // The shipped PCM-A10 image is a NuttX image with the magic at offset 8.
    // The DLL does not appear to validate this, so it is only a warning unless
    // --strict-image is used.
    let mut header = [0u8; 13];
    if let Ok(mut file) = File::open(&options.image) {
        if file.read(&mut header).unwrap_or(0) >= 13 && &header[8..13] != b"nuttx" {
            let message = "image does not contain the NuttX magic at offset 8".to_string();
            if options.strict_image {
                return Err(Error::InvalidImage(message));
            }
            warnings.push(message);
        }
    }

    let mount_root = probe.mount_root.clone().ok_or_else(|| {
        Error::NotFound(format!(
            "the mass-storage volume for {} is not mounted; mount it or pass --mount <dir>",
            probe.scsi_path.display()
        ))
    })?;
    let write_name = options
        .package
        .as_ref()
        .map(|package| package.write_path.clone())
        .unwrap_or_else(|| "UPG.IMG".to_string());
    let destination = mount_root.join(&write_name);

    let timer = options
        .timer
        .or_else(|| options.package.as_ref().and_then(|package| package.timer))
        .unwrap_or(Duration::from_secs(600));
    let timeout = options
        .timeout
        .or_else(|| options.package.as_ref().and_then(|package| package.timeout))
        .unwrap_or(scsi::DEFAULT_UPDATE_TIMEOUT);
    let command_timeout = timer.min(timeout);

    let free_bytes = device::available_bytes(&mount_root);
    match free_bytes {
        Some(free) if free < image_size => {
            return Err(Error::InvalidImage(format!(
                "not enough free space on {}: need {} bytes, have {} bytes",
                mount_root.display(),
                image_size,
                free
            )));
        }
        None => warnings.push(format!(
            "could not determine free space on {}",
            mount_root.display()
        )),
        _ => {}
    }

    let package_version = options
        .package
        .as_ref()
        .map(|package| package.version.clone());
    let version_newer = package_version.as_ref().and_then(|version| {
        crate::update_xml::version_is_newer(version, &probe.dev_info.product_revision)
    });
    match version_newer {
        Some(false) => warnings.push(format!(
            "device already reports version {}; package is {}",
            probe.dev_info.product_revision,
            package_version.as_deref().unwrap_or("?")
        )),
        None if package_version.is_some() => warnings.push(format!(
            "could not compare package version {:?} with device revision {:?}",
            package_version.as_deref().unwrap_or("?"),
            probe.dev_info.product_revision
        )),
        _ => {}
    }

    // The DLL front-end matches the package product against the connected
    // device before it offers the update.  Refuse an obvious mismatch unless
    // the user forces it; empty fields are treated as unknown.
    let product_matches = options.package.as_ref().and_then(|package| {
        let package_product = package.product.trim();
        let device_product = probe.dev_info.product_id.trim();
        if package_product.is_empty() || device_product.is_empty() {
            return None;
        }
        let package_upper = package_product.to_ascii_uppercase();
        let device_upper = device_product.to_ascii_uppercase();
        Some(device_upper.contains(&package_upper) || package_upper.contains(&device_upper))
    });
    if product_matches == Some(false) {
        warnings.push(format!(
            "package is for {:?} but the device reports {:?}",
            options
                .package
                .as_ref()
                .map(|package| package.product.as_str())
                .unwrap_or("?"),
            probe.dev_info.product_id
        ));
    }

    Ok(FlashPlan {
        image: options.image.clone(),
        image_size,
        image_sha256,
        destination,
        mount_root,
        device_path: probe.scsi_path.clone(),
        device_product: probe.inquiry.product.clone(),
        device_product_id: probe.dev_info.product_id.clone(),
        device_revision: probe.dev_info.product_revision.clone(),
        package_version,
        version_newer,
        product_matches,
        timeout,
        command_timeout,
        free_bytes,
        warnings,
    })
}

/// Execute a flash plan.
///
/// `transport` is the already-probed pass-through handle.  `progress` is
/// called with copy and update percentages; it is never called from the
/// worker thread.
pub fn execute(
    _probe: Probe,
    mut transport: Box<dyn Transport>,
    plan: FlashPlan,
    options: &FlashOptions,
    progress: &mut dyn FnMut(Progress),
) -> Result<FlashOutcome, Error> {
    if options.dry_run {
        return Ok(FlashOutcome {
            plan,
            copied: false,
            started: false,
            elapsed: Duration::ZERO,
            removed_image: false,
            verified: None,
        });
    }

    if plan.product_matches == Some(false) && !options.force {
        let package_product = options
            .package
            .as_ref()
            .map(|package| package.product.as_str())
            .unwrap_or("?");
        return Err(Error::Refused(format!(
            "the package is for {package_product:?} but the device reports {:?}; use --force to flash anyway",
            plan.device_product_id
        )));
    }

    if plan.version_newer == Some(false) && !options.force {
        return Err(Error::Refused(format!(
            "device already reports version {} and the package is {}; use --force to flash anyway",
            plan.device_revision,
            plan.package_version.as_deref().unwrap_or("?")
        )));
    }

    let started_at = Instant::now();

    // The DLL deletes an existing destination before copying.
    match fs::remove_file(&plan.destination) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(Error::Io(io::Error::new(
                error.kind(),
                format!("cannot remove {}: {error}", plan.destination.display()),
            )))
        }
    }

    progress(Progress {
        stage: Stage::Copy,
        percent: 0,
    });
    if let Err(error) =
        copy_with_progress(&plan.image, &plan.destination, plan.image_size, progress)
    {
        let _ = fs::remove_file(&plan.destination);
        return Err(error);
    }
    progress(Progress {
        stage: Stage::Copy,
        percent: 19,
    });

    // The DLL's START worker re-checks TEST UNIT READY on the drive before it
    // sends the vendor command (`FUN_10021f48`), because the recorder can be
    // busy after receiving a large file.
    scsi::test_unit_ready(transport.as_mut())?;

    // Send START FW UPDATE on a worker thread so progress can be reported
    // while the ioctl blocks, mirroring the DLL's worker-thread design.
    let timeout = plan.timeout;
    let command_timeout = plan.command_timeout;
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut transport = transport;
        let result = scsi::start_fw_update(transport.as_mut(), command_timeout);
        let _ = sender.send(result);
    });

    let update_started = Instant::now();
    let mut worker = Some(worker);
    loop {
        match receiver.recv_timeout(Duration::from_millis(1000)) {
            Ok(result) => {
                result?;
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let elapsed = update_started.elapsed();
                if elapsed >= timeout {
                    // Drop the handle and detach the worker; the process can
                    // exit without waiting for a wedged ioctl.
                    worker.take();
                    return Err(Error::Timeout(format!(
                        "START FW UPDATE did not complete within {} s",
                        timeout.as_secs()
                    )));
                }
                progress(Progress {
                    stage: Stage::Update,
                    percent: update_progress(elapsed, timeout),
                });
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(Error::scsi(
                    "START FW UPDATE",
                    0,
                    None,
                    "worker thread ended without a result",
                ));
            }
        }
    }
    if let Some(worker) = worker.take() {
        let _ = worker.join();
    }
    progress(Progress {
        stage: Stage::Update,
        percent: 100,
    });

    let elapsed = started_at.elapsed();
    let mut removed_image = false;
    if options.remove_image {
        match fs::remove_file(&plan.destination) {
            Ok(()) => removed_image = true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => removed_image = true,
            Err(error) => {
                eprintln!(
                    "warning: could not remove {}: {error}",
                    plan.destination.display()
                );
            }
        }
    }

    let verified = if options.verify {
        Some(verify_after_update(&plan, options.verify_timeout))
    } else {
        None
    };

    Ok(FlashOutcome {
        plan,
        copied: true,
        started: true,
        elapsed,
        removed_image,
        verified,
    })
}

/// Progress percentage for the copy phase.
///
/// The DLL reports `index * 10 + 0x13`; with the single FastCopy call this is
/// 19.  We map the byte count into 0..=19 so the UI still moves during a long
/// copy, then emit 19 at completion.
pub fn copy_progress(copied: u64, total: u64) -> u8 {
    if total == 0 {
        return 0;
    }
    ((copied.min(total).saturating_mul(19) / total) as u8).min(19)
}

/// Progress percentage for the update phase.
///
/// The DLL uses `elapsed * 0x37 / timeout + 0x28`, i.e. 40..95.
pub fn update_progress(elapsed: Duration, timeout: Duration) -> u8 {
    let timeout_ms = timeout.as_millis().max(1) as u64;
    let elapsed_ms = elapsed.as_millis() as u64;
    (40u64 + (elapsed_ms.saturating_mul(55) / timeout_ms).min(55)).min(95) as u8
}

fn copy_with_progress(
    source: &Path,
    destination: &Path,
    total: u64,
    progress: &mut dyn FnMut(Progress),
) -> Result<(), Error> {
    let mut input = File::open(source).map_err(|error| {
        Error::Io(io::Error::new(
            error.kind(),
            format!("cannot open {}: {error}", source.display()),
        ))
    })?;
    let mut output = File::create(destination).map_err(|error| {
        Error::Io(io::Error::new(
            error.kind(),
            format!("cannot create {}: {error}", destination.display()),
        ))
    })?;
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut copied = 0u64;
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        output.write_all(&buffer[..read])?;
        copied += read as u64;
        progress(Progress {
            stage: Stage::Copy,
            percent: copy_progress(copied, total),
        });
    }
    output.flush()?;
    // Make sure the image is on the device before asking it to program flash.
    output.sync_all()?;
    if copied != total {
        return Err(Error::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("copied {copied} of {total} bytes"),
        )));
    }
    Ok(())
}

fn verify_after_update(plan: &FlashPlan, timeout: Duration) -> bool {
    let Some(expected) = plan.package_version.as_deref() else {
        return false;
    };
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        thread::sleep(Duration::from_secs(2));
        if let Ok((probe, _transport)) = device::probe_path(&plan.device_path, None) {
            if probe.dev_info.product_revision == expected {
                return true;
            }
        }
        if let Ok(targets) = device::discover(None, false) {
            for (probe, _transport) in targets {
                if probe.dev_info.product_revision == expected {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_progress_is_bounded() {
        assert_eq!(copy_progress(0, 100), 0);
        assert_eq!(copy_progress(50, 100), 9);
        assert_eq!(copy_progress(100, 100), 19);
        assert_eq!(copy_progress(200, 100), 19);
        assert_eq!(copy_progress(0, 0), 0);
    }

    #[test]
    fn update_progress_matches_the_dll_formula() {
        let timeout = Duration::from_secs(540);
        assert_eq!(update_progress(Duration::ZERO, timeout), 40);
        assert_eq!(update_progress(Duration::from_secs(270), timeout), 67);
        assert_eq!(update_progress(timeout, timeout), 95);
        assert_eq!(update_progress(Duration::from_secs(600), timeout), 95);
    }
}
