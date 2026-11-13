//! Reimplementation of the Sony PCM-A10 firmware update protocol.
//!
//! The protocol was recovered from the shipped updater
//! (`WmFwUpdater.dll`, SHA-256
//! `789d5df53f2bc9b8170518aa1bce83c292a35310dda4bda82e7e54e8e401250b`).
//! The implemented path is the mass-storage / SCSI one:
//!
//! 1. probe with INQUIRY, TEST UNIT READY and the Sony `REQUEST DEVINFO`
//!    vendor command;
//! 2. copy the firmware image (`UPG.IMG`) to the root of the recorder's
//!    mass-storage volume;
//! 3. send the Sony `START FW UPDATE` vendor command and wait for it to
//!    finish.
//!
//! The MTP/WPD path present in the DLL is documented but not implemented,
//! because it relies on the Windows WPD API.  Reading firmware back from the
//! device is an extension: the shipped DLL has no such command, so
//! [`pull`] only reads the image file from the mass-storage volume.
//!
//! See `README.md` and `docs/PROTOCOL.md` for the evidence trail and the
//! distinction between observed behaviour and design choices.

#![forbid(unsafe_op_in_unsafe_fn)]

pub mod cli;
pub mod dev_info;
pub mod device;
pub mod error;
pub mod flash;
pub mod json;
pub mod pull;
pub mod scsi;
pub mod sha256;
pub mod update_xml;

#[cfg(target_os = "linux")]
pub mod scsi_linux;
#[cfg(windows)]
pub mod scsi_windows;

pub use error::{Error, Sense};
pub use scsi::{DataDirection, Inquiry, ScsiStatus, Transport};
