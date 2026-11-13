//! Sony vendor SCSI commands used by the PCM-A10 firmware updater.
//!
//! The shipped updater (`WmFwUpdater.dll`) talks to the recorder through the
//! mass-storage class.  It sends ordinary INQUIRY / TEST UNIT READY commands
//! plus two Sony vendor commands with opcode `0xFC` and the four-byte
//! subsystem tag `dbmn`.
//!
//! Every byte in this module was recovered from the DLL.  The disassembly
//! addresses and the exact CDB bytes are listed in `docs/PROTOCOL.md`.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use crate::dev_info::{self, MscDevInfo};
use crate::error::{Error, Sense};

/// Sony vendor-specific opcode used by both vendor commands.
pub const VENDOR_OPCODE: u8 = 0xfc;
/// Subsystem tag written to CDB bytes 3..7.
pub const VENDOR_SUBSYSTEM: [u8; 4] = *b"dbmn";
/// INQUIRY allocation length written by the caller at `0x10020d91`.
pub const INQUIRY_ALLOCATION: u8 = 0x60;
/// REQUEST DEVINFO allocation length (`0x80` bytes of data-in).
pub const DEVINFO_ALLOCATION: u8 = 0x80;
/// Size of the DEVINFO response buffer.
pub const DEVINFO_SIZE: usize = 0x80;

/// Timeout used for INQUIRY.  The DLL does not set a special timeout here.
pub const INQUIRY_TIMEOUT: Duration = Duration::from_secs(30);
/// Timeout used for TEST UNIT READY.  The DLL passes `0xB4` (180 s) to its
/// timeout setter in the probe/open paths (`0x1001e807`, `0x100200e2`).
pub const TUR_TIMEOUT: Duration = Duration::from_secs(180);
/// Timeout used for REQUEST DEVINFO.
pub const DEVINFO_TIMEOUT: Duration = Duration::from_secs(30);
/// Fallback update deadline; the DLL default is `300000` ms.
pub const DEFAULT_UPDATE_TIMEOUT: Duration = Duration::from_secs(300);

/// Direction of the data phase of a SCSI command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataDirection {
    /// No data phase.
    None,
    /// Data is written from the host to the device.
    ToDevice,
    /// Data is read from the device into the host buffer.
    FromDevice,
}

/// Result of one SCSI pass-through call.
#[derive(Debug, Clone, Copy)]
pub struct ScsiStatus {
    /// SCSI status byte (`0` is GOOD, `2` is CHECK CONDITION, ...).
    pub status: u8,
    /// Fixed-format sense data, when the transport returned any.
    pub sense: Option<Sense>,
    /// Number of bytes actually transferred in the data phase.
    pub transferred: usize,
}

/// A SCSI pass-through transport.
///
/// Implementations exist for Linux (`SG_IO`) and Windows
/// (`IOCTL_SCSI_PASS_THROUGH_DIRECT`).  The trait is `Send` so the blocking
/// START FW UPDATE call can be moved to a worker thread while the main thread
/// reports progress, matching the DLL's worker-thread design.
pub trait Transport: Send {
    /// Execute one command.
    fn execute(
        &mut self,
        cdb: &[u8],
        data: Option<&mut [u8]>,
        direction: DataDirection,
        timeout: Duration,
    ) -> Result<ScsiStatus, Error>;

    /// Path or handle name used for diagnostics.
    fn path(&self) -> &Path;
}

/// Standard INQUIRY CDB.  Recovered from `FUN_10020428`; the caller writes the
/// allocation length `0x60` at CDB byte 4.
pub fn inquiry_cdb() -> [u8; 6] {
    [0x12, 0x00, 0x00, 0x00, INQUIRY_ALLOCATION, 0x00]
}

/// Sony `REQUEST DEVINFO` CDB, built by `FUN_10020503`.
///
/// Bytes: `FC 00 20 64 62 6D 6E 00 80 00 00 00`
pub fn request_devinfo_cdb() -> [u8; 12] {
    let mut cdb = [0u8; 12];
    cdb[0] = VENDOR_OPCODE;
    cdb[1] = 0x00;
    cdb[2] = 0x20;
    cdb[3..7].copy_from_slice(&VENDOR_SUBSYSTEM);
    cdb[7] = 0x00;
    cdb[8] = DEVINFO_ALLOCATION;
    cdb
}

/// Sony `START FW UPDATE` CDB, built by `FUN_1002053f`.
///
/// Bytes: `FC 00 04 64 62 6D 6E 00 00 00 00 00`
pub fn start_fw_update_cdb() -> [u8; 12] {
    let mut cdb = [0u8; 12];
    cdb[0] = VENDOR_OPCODE;
    cdb[1] = 0x00;
    cdb[2] = 0x04;
    cdb[3..7].copy_from_slice(&VENDOR_SUBSYSTEM);
    cdb
}

/// TEST UNIT READY as sent by the DLL: `FUN_10020b14` zeroes a 12-byte CDB
/// buffer and passes its length (`0x0C`) to the no-data sender.  Opcode `0x00`
/// is TEST UNIT READY; the remaining zero bytes are padding.
pub fn test_unit_ready_cdb() -> [u8; 12] {
    [0u8; 12]
}

/// Format a byte slice as lower-case hex, used by tests and diagnostics.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Parse fixed-format sense data (response code `0x70` or `0x71`).
///
/// Sense key is byte 2 (low nibble), ASC is byte 12 and ASCQ is byte 13.
/// The DLL represents the same triple as `key << 16 | asc << 8 | ascq`, e.g.
/// `0x23a00` for `NOT READY / MEDIUM NOT PRESENT`.
pub fn parse_sense(buf: &[u8]) -> Option<Sense> {
    if buf.len() < 14 {
        return None;
    }
    let response = buf[0] & 0x7f;
    if response != 0x70 && response != 0x71 {
        return None;
    }
    Some(Sense {
        key: buf[2] & 0x0f,
        asc: buf[12],
        ascq: buf[13],
    })
}

/// Apply the completion rule observed in the DLL.
///
/// * a non-zero sense key is a failure;
/// * a non-zero SCSI status with no sense key is a failure;
/// * `NOT READY / MEDIUM NOT PRESENT` is handled separately by
///   [`test_unit_ready`], because the DLL tolerates it only for TUR.
pub fn check_completion(
    operation: &'static str,
    status: u8,
    sense: Option<Sense>,
) -> Result<(), Error> {
    if let Some(sense) = sense {
        if sense.key != 0 {
            return Err(Error::scsi(
                operation,
                status,
                Some(sense),
                "device reported a non-zero sense key",
            ));
        }
    }
    if status != 0 {
        return Err(Error::scsi(
            operation,
            status,
            sense,
            "non-zero SCSI status",
        ));
    }
    Ok(())
}

/// Trim trailing NULs and spaces from a fixed-width SCSI string.
pub fn trim_fixed(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .rposition(|b| *b != 0 && *b != b' ')
        .map_or(0, |i| i + 1);
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Parsed standard INQUIRY response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inquiry {
    /// Raw 8-byte vendor identification field.
    pub vendor_raw: [u8; 8],
    /// Raw 16-byte product identification field.
    pub product_raw: [u8; 16],
    /// Raw 4-byte product revision field.
    pub revision_raw: [u8; 4],
    /// Trimmed vendor string.
    pub vendor: String,
    /// Trimmed product string.
    pub product: String,
    /// Trimmed revision string.
    pub revision: String,
}

impl Inquiry {
    /// Parse the first 36 bytes of an INQUIRY response.
    pub fn parse(buf: &[u8]) -> Result<Self, Error> {
        if buf.len() < 36 {
            return Err(Error::InvalidDevice(format!(
                "INQUIRY returned {} bytes, need at least 36",
                buf.len()
            )));
        }
        let mut vendor_raw = [0u8; 8];
        let mut product_raw = [0u8; 16];
        let mut revision_raw = [0u8; 4];
        vendor_raw.copy_from_slice(&buf[8..16]);
        product_raw.copy_from_slice(&buf[16..32]);
        revision_raw.copy_from_slice(&buf[32..36]);
        Ok(Self {
            vendor_raw,
            product_raw,
            revision_raw,
            vendor: trim_fixed(&vendor_raw),
            product: trim_fixed(&product_raw),
            revision: trim_fixed(&revision_raw),
        })
    }
}

/// Check the vendor and product rules recovered from the DLL.
///
/// The vendor field must be exactly `"SONY    "`.  The product must contain
/// `WALKMAN`, `IC RECORDER`, `Smart B-Trainer` or `PERSONAL AUDIO`, or end in
/// `/SU` (case-insensitive).
pub fn check_supported(inquiry: &Inquiry) -> Result<(), Error> {
    if inquiry.vendor_raw != *b"SONY    " {
        return Err(Error::NotSonyDevice {
            vendor: inquiry.vendor.clone(),
            product: inquiry.product.clone(),
        });
    }
    let product = inquiry.product.to_ascii_uppercase();
    let supported = product.contains("WALKMAN")
        || product.contains("IC RECORDER")
        || product.contains("SMART B-TRAINER")
        || product.contains("PERSONAL AUDIO")
        || product.ends_with("/SU");
    if !supported {
        return Err(Error::UnsupportedDevice {
            product: inquiry.product.clone(),
        });
    }
    Ok(())
}

/// Send INQUIRY and parse the response.
pub fn inquiry(transport: &mut dyn Transport) -> Result<Inquiry, Error> {
    let mut buf = [0u8; INQUIRY_ALLOCATION as usize];
    let status = transport.execute(
        &inquiry_cdb(),
        Some(&mut buf),
        DataDirection::FromDevice,
        INQUIRY_TIMEOUT,
    )?;
    check_completion("INQUIRY", status.status, status.sense)?;
    Inquiry::parse(&buf)
}

/// Send TEST UNIT READY.
///
/// The DLL treats `02/3A/00` (NOT READY / MEDIUM NOT PRESENT) as success,
/// because the recorder may still be booting.  Every other non-zero sense key
/// is a failure.
pub fn test_unit_ready(transport: &mut dyn Transport) -> Result<(), Error> {
    let status = transport.execute(
        &test_unit_ready_cdb(),
        None,
        DataDirection::None,
        TUR_TIMEOUT,
    )?;
    if let Some(sense) = status.sense {
        if sense.key != 0 {
            if sense.is_not_ready_no_medium() {
                return Ok(());
            }
            return Err(Error::scsi(
                "TEST UNIT READY",
                status.status,
                Some(sense),
                "device is not ready",
            ));
        }
    }
    if status.status != 0 {
        return Err(Error::scsi(
            "TEST UNIT READY",
            status.status,
            status.sense,
            "non-zero SCSI status",
        ));
    }
    Ok(())
}

/// Send the Sony `REQUEST DEVINFO` command and parse the mass-storage layout.
pub fn request_devinfo(transport: &mut dyn Transport) -> Result<MscDevInfo, Error> {
    let mut buf = [0u8; DEVINFO_SIZE];
    let status = transport.execute(
        &request_devinfo_cdb(),
        Some(&mut buf),
        DataDirection::FromDevice,
        DEVINFO_TIMEOUT,
    )?;
    check_completion("REQUEST DEVINFO", status.status, status.sense)?;
    dev_info::parse_msc(&buf)
}

/// Send the Sony `START FW UPDATE` command.
///
/// The caller is responsible for copying the firmware image to the
/// mass-storage volume first.
pub fn start_fw_update(transport: &mut dyn Transport, timeout: Duration) -> Result<(), Error> {
    let status = transport.execute(&start_fw_update_cdb(), None, DataDirection::None, timeout)?;
    check_completion("START FW UPDATE", status.status, status.sense)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cdb_bytes_match_the_dll() {
        assert_eq!(inquiry_cdb(), [0x12, 0x00, 0x00, 0x00, 0x60, 0x00]);
        assert_eq!(
            request_devinfo_cdb(),
            [0xfc, 0x00, 0x20, 0x64, 0x62, 0x6d, 0x6e, 0x00, 0x80, 0x00, 0x00, 0x00]
        );
        assert_eq!(
            start_fw_update_cdb(),
            [0xfc, 0x00, 0x04, 0x64, 0x62, 0x6d, 0x6e, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
        assert_eq!(test_unit_ready_cdb(), [0u8; 12]);
    }

    #[test]
    fn cdb_hex_is_stable() {
        assert_eq!(hex(&inquiry_cdb()), "120000006000");
        assert_eq!(hex(&request_devinfo_cdb()), "fc002064626d6e0080000000");
        assert_eq!(hex(&start_fw_update_cdb()), "fc000464626d6e0000000000");
    }

    #[test]
    fn parses_fixed_format_sense() {
        let mut buf = [0u8; 18];
        buf[0] = 0x70;
        buf[2] = 0x02;
        buf[12] = 0x3a;
        buf[13] = 0x00;
        let sense = parse_sense(&buf).unwrap();
        assert_eq!(sense.key, 0x02);
        assert_eq!(sense.asc, 0x3a);
        assert_eq!(sense.ascq, 0x00);
        assert!(sense.is_not_ready_no_medium());
        assert_eq!(sense.to_string(), "sense 0x02/0x3a/0x00");
    }

    #[test]
    fn ignores_non_sense_buffers() {
        assert!(parse_sense(&[0u8; 18]).is_none());
        assert!(parse_sense(&[0x70, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x3a]).is_none());
    }

    #[test]
    fn completion_rules() {
        assert!(check_completion("X", 0, None).is_ok());
        assert!(check_completion(
            "X",
            2,
            Some(Sense {
                key: 0x02,
                asc: 0x3a,
                ascq: 0
            })
        )
        .is_err());
        assert!(check_completion("X", 2, None).is_err());
        assert!(check_completion(
            "X",
            0,
            Some(Sense {
                key: 0,
                asc: 0,
                ascq: 0
            })
        )
        .is_ok());
    }

    #[test]
    fn inquiry_strings_are_trimmed() {
        let mut buf = [0u8; 96];
        buf[8..16].copy_from_slice(b"SONY    ");
        buf[16..32].copy_from_slice(b"IC RECORDER     ");
        buf[32..36].copy_from_slice(b"1.01");
        let inquiry = Inquiry::parse(&buf).unwrap();
        assert_eq!(inquiry.vendor, "SONY");
        assert_eq!(inquiry.product, "IC RECORDER");
        assert_eq!(inquiry.revision, "1.01");
        assert!(check_supported(&inquiry).is_ok());
    }

    #[test]
    fn supported_product_rules() {
        let mut buf = [0u8; 96];
        buf[8..16].copy_from_slice(b"SONY    ");
        for product in [
            b"WALKMAN         ".as_slice(),
            b"IC RECORDER     ".as_slice(),
            b"Smart B-Trainer ".as_slice(),
            b"PERSONAL AUDIO  ".as_slice(),
            b"NW-A100/SU      ".as_slice(),
        ] {
            buf[16..32].fill(b' ');
            buf[16..16 + product.len()].copy_from_slice(product);
            let inquiry = Inquiry::parse(&buf).unwrap();
            assert!(check_supported(&inquiry).is_ok(), "{product:?}");
        }
        buf[16..32].copy_from_slice(b"USB DISK        ");
        let inquiry = Inquiry::parse(&buf).unwrap();
        assert!(check_supported(&inquiry).is_err());
    }
}
