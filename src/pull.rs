//! Pulling the update image back from the recorder.
//!
//! **This is an extension, not part of the shipped protocol.**  The shipped
//! `WmFwUpdater.dll` only writes `UPG.IMG` to the mass-storage volume and
//! sends START FW UPDATE; it contains no firmware-read or upload command.
//! This module reads the `UPG.IMG` file that is present on the volume.  It
//! cannot dump the recorder's internal flash.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::device::Probe;
use crate::error::Error;
use crate::sha256::{self, Sha256};
use crate::update_xml::{self, UpdatePackage};

/// Options for a pull.
#[derive(Debug, Clone)]
pub struct PullOptions {
    /// Destination file.
    pub output: PathBuf,
    /// Overwrite an existing destination.
    pub force: bool,
    /// Expected SHA-256 of the image.
    pub expected_sha256: Option<String>,
}

impl Default for PullOptions {
    fn default() -> Self {
        Self {
            output: PathBuf::from("UPG.IMG"),
            force: false,
            expected_sha256: None,
        }
    }
}

/// A validated pull plan.
#[derive(Debug, Clone)]
pub struct PullPlan {
    /// Image on the device.
    pub source: PathBuf,
    /// Local destination.
    pub output: PathBuf,
    /// Source size in bytes.
    pub size: u64,
    /// Mount root of the mass-storage volume.
    pub mount_root: PathBuf,
    /// `SWUpdate.xml` from the device, when present.
    pub package: Option<UpdatePackage>,
    /// SCSI pass-through path.
    pub device_path: PathBuf,
    /// INQUIRY product.
    pub device_product: String,
    /// DEVINFO `ProductRevision`.
    pub device_revision: String,
}

/// Result of a pull.
#[derive(Debug, Clone)]
pub struct PullOutcome {
    /// The plan that was executed.
    pub plan: PullPlan,
    /// SHA-256 of the pulled bytes.
    pub sha256: String,
    /// Number of bytes written.
    pub bytes_written: u64,
    /// `Some(true)` when an expected hash was supplied and matched.
    pub verified: Option<bool>,
}

/// Compute and validate a pull plan.
pub fn plan(probe: &Probe, options: &PullOptions) -> Result<PullPlan, Error> {
    let mount_root = probe.mount_root.clone().ok_or_else(|| {
        Error::NotFound(format!(
            "the mass-storage volume for {} is not mounted; mount it or pass --mount <dir>",
            probe.scsi_path.display()
        ))
    })?;

    let xml_path = mount_root.join("SWUpdate.xml");
    let package = if xml_path.is_file() {
        Some(update_xml::UpdatePackage::from_path(&xml_path)?)
    } else {
        None
    };
    let name = package
        .as_ref()
        .map(|package| package.write_path.clone())
        .unwrap_or_else(|| "UPG.IMG".to_string());
    let source = mount_root.join(&name);

    let metadata = fs::metadata(&source).map_err(|error| {
        Error::NotFound(format!(
            "{} is not present on {}: {error}",
            name,
            mount_root.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(Error::NotFound(format!(
            "{} is not a regular file",
            source.display()
        )));
    }
    if metadata.len() == 0 {
        return Err(Error::InvalidImage(format!(
            "{} is empty",
            source.display()
        )));
    }

    if options.output.exists() && !options.force {
        return Err(Error::Refused(format!(
            "{} already exists; use --force to overwrite",
            options.output.display()
        )));
    }
    if let Some(parent) = options.output.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            return Err(Error::Usage(format!(
                "output directory {} does not exist",
                parent.display()
            )));
        }
    }
    if let Some(expected) = &options.expected_sha256 {
        if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::Usage(
                "--sha256 must be a 64-character hexadecimal digest".into(),
            ));
        }
    }

    Ok(PullPlan {
        source,
        output: options.output.clone(),
        size: metadata.len(),
        mount_root,
        package,
        device_path: probe.scsi_path.clone(),
        device_product: probe.inquiry.product.clone(),
        device_revision: probe.dev_info.product_revision.clone(),
    })
}

/// Copy the image from the device and verify its hash.
pub fn execute(
    plan: PullPlan,
    options: &PullOptions,
    progress: &mut dyn FnMut(u8),
) -> Result<PullOutcome, Error> {
    let mut input = File::open(&plan.source)?;
    let mut output = File::create(&plan.output).map_err(|error| {
        Error::Io(io::Error::new(
            error.kind(),
            format!("cannot create {}: {error}", plan.output.display()),
        ))
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut written = 0u64;
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        output.write_all(&buffer[..read])?;
        written += read as u64;
        let percent = written
            .saturating_mul(100)
            .checked_div(plan.size)
            .unwrap_or(100)
            .min(100) as u8;
        progress(percent);
    }
    output.flush()?;
    output.sync_all()?;
    let digest = hasher.finalize();
    let digest_hex = sha256::to_hex(&digest);

    if let Some(expected) = &options.expected_sha256 {
        if !digest_hex.eq_ignore_ascii_case(expected) {
            let _ = fs::remove_file(&plan.output);
            return Err(Error::InvalidImage(format!(
                "SHA-256 mismatch: expected {expected}, got {digest_hex}"
            )));
        }
    }
    if written != plan.size {
        let _ = fs::remove_file(&plan.output);
        return Err(Error::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("read {written} of {} bytes", plan.size),
        )));
    }

    Ok(PullOutcome {
        plan,
        sha256: digest_hex,
        bytes_written: written,
        verified: options.expected_sha256.as_ref().map(|_| true),
    })
}

/// Read the `UPG.IMG` currently present on a device without a full plan.
pub fn default_source(mount_root: &Path) -> PathBuf {
    mount_root.join("UPG.IMG")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_source_is_the_volume_root() {
        assert_eq!(
            default_source(Path::new("/media/PCM-A10")),
            PathBuf::from("/media/PCM-A10/UPG.IMG")
        );
    }
}
