//! Device information returned by the Sony `REQUEST DEVINFO` command.
//!
//! Two layouts were recovered.  The mass-storage (MSC) layout is the one used
//! by the implemented transport.  The MTP/WPD layout is included for
//! completeness and is documented but not used, because the MTP path is a
//! Windows WPD-specific implementation in the shipped DLL.

use crate::error::Error;

/// First seven bytes of a mass-storage DEVINFO response.
pub const DEVINFO_MAGIC: &[u8; 7] = b"DEVINFO";
/// Value of the `InfoSize` byte at offset 7.
pub const DEVINFO_INFO_SIZE: u8 = 0x80;
/// Vendor identification field at offset 16.
pub const DEVINFO_VENDOR: &[u8; 8] = b"SONY    ";

/// Parsed mass-storage DEVINFO response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MscDevInfo {
    /// Vendor identification, raw eight bytes (`"SONY    "`).
    pub vendor_raw: [u8; 8],
    /// `ProductId`, 16 bytes at offset 24.
    pub product_id: String,
    /// `ProductRevision`, 4 bytes at offset 40.
    pub product_revision: String,
    /// Firmware id byte at offset 96.
    pub fw_id: u8,
    /// Hardware id byte at offset 97.
    pub hw_id: u8,
}

impl MscDevInfo {
    /// Firmware id rendered exactly like the DLL (`%02x`).
    pub fn fw_id_hex(&self) -> String {
        format!("{:02x}", self.fw_id)
    }

    /// Hardware id rendered exactly like the DLL (`%02x`).
    pub fn hw_id_hex(&self) -> String {
        format!("{:02x}", self.hw_id)
    }
}

fn fixed_string(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .rposition(|b| *b != 0 && *b != b' ')
        .map_or(0, |i| i + 1);
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Parse a mass-storage DEVINFO response.
///
/// The DLL validates the `DEVINFO` property flag, the `0x80` info size and the
/// `SONY    ` vendor field, in that order.  The offsets are:
///
/// | offset | size | field |
/// |--------|------|-------|
/// | 0      | 7    | `"DEVINFO"` |
/// | 7      | 1    | info size (`0x80`) |
/// | 16     | 8    | vendor (`"SONY    "`) |
/// | 24     | 16   | `ProductId` |
/// | 40     | 4    | `ProductRevision` |
/// | 96     | 1    | FWID |
/// | 97     | 1    | HWID |
pub fn parse_msc(buf: &[u8]) -> Result<MscDevInfo, Error> {
    if buf.len() < 98 {
        return Err(Error::InvalidDevice(format!(
            "DEVINFO response is {} bytes, need at least 98",
            buf.len()
        )));
    }
    if &buf[0..7] != DEVINFO_MAGIC {
        return Err(Error::InvalidDevice(format!(
            "DEVINFO PropertyFlag is invalid ({:?})",
            String::from_utf8_lossy(&buf[0..7])
        )));
    }
    if buf[7] != DEVINFO_INFO_SIZE {
        return Err(Error::InvalidDevice(format!(
            "DEVINFO InfoSize is invalid ({})",
            buf[7]
        )));
    }
    if &buf[16..24] != DEVINFO_VENDOR {
        return Err(Error::InvalidDevice(format!(
            "DEVINFO VendorIdentification is invalid ({:?})",
            String::from_utf8_lossy(&buf[16..24])
        )));
    }
    let mut vendor_raw = [0u8; 8];
    vendor_raw.copy_from_slice(&buf[16..24]);
    Ok(MscDevInfo {
        vendor_raw,
        product_id: fixed_string(&buf[24..40]),
        product_revision: fixed_string(&buf[40..44]),
        fw_id: buf[96],
        hw_id: buf[97],
    })
}

/// Parsed MTP/WPD DEVINFO response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MtpDevInfo {
    /// `ProductId`, 16 bytes at offset 16.
    pub product_id: String,
    /// `ProductRevision`, 4 bytes at offset 36.
    pub product_revision: String,
    /// Firmware id byte at offset 112.
    pub fw_id: u8,
    /// Hardware id byte at offset 113.
    pub hw_id: u8,
}

/// Parse an MTP/WPD DEVINFO response.
///
/// The MTP layout was recovered from the DLL's WPD path.  It is not used by
/// this crate's transports, but it is kept so the two layouts are not
/// confused.  Unlike the MSC layout, the MTP offsets shift `ProductId` to 16,
/// `ProductRevision` to 36, FWID to 112 and HWID to 113.
pub fn parse_mtp(buf: &[u8]) -> Result<MtpDevInfo, Error> {
    if buf.len() < 114 {
        return Err(Error::InvalidDevice(format!(
            "MTP DEVINFO response is {} bytes, need at least 114",
            buf.len()
        )));
    }
    Ok(MtpDevInfo {
        product_id: fixed_string(&buf[16..32]),
        product_revision: fixed_string(&buf[36..40]),
        fw_id: buf[112],
        hw_id: buf[113],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msc_sample() -> [u8; 128] {
        let mut buf = [0u8; 128];
        buf[0..7].copy_from_slice(b"DEVINFO");
        buf[7] = 0x80;
        buf[16..24].copy_from_slice(b"SONY    ");
        buf[24..40].copy_from_slice(b"PCM-A10         ");
        buf[40..44].copy_from_slice(b"1.01");
        buf[96] = 0xab;
        buf[97] = 0xcd;
        buf
    }

    #[test]
    fn parses_msc_devinfo() {
        let info = parse_msc(&msc_sample()).unwrap();
        assert_eq!(info.vendor_raw, *b"SONY    ");
        assert_eq!(info.product_id, "PCM-A10");
        assert_eq!(info.product_revision, "1.01");
        assert_eq!(info.fw_id, 0xab);
        assert_eq!(info.hw_id, 0xcd);
        assert_eq!(info.fw_id_hex(), "ab");
        assert_eq!(info.hw_id_hex(), "cd");
    }

    #[test]
    fn rejects_wrong_property_flag() {
        let mut buf = msc_sample();
        buf[0] = b'X';
        let err = parse_msc(&buf).unwrap_err();
        assert!(err.to_string().contains("PropertyFlag"));
    }

    #[test]
    fn rejects_wrong_info_size() {
        let mut buf = msc_sample();
        buf[7] = 0x40;
        let err = parse_msc(&buf).unwrap_err();
        assert!(err.to_string().contains("InfoSize"));
    }

    #[test]
    fn rejects_wrong_vendor() {
        let mut buf = msc_sample();
        buf[16..24].copy_from_slice(b"OTHER   ");
        let err = parse_msc(&buf).unwrap_err();
        assert!(err.to_string().contains("VendorIdentification"));
    }

    #[test]
    fn rejects_short_response() {
        assert!(parse_msc(&[0u8; 97]).is_err());
    }

    #[test]
    fn parses_mtp_layout() {
        let mut buf = [0u8; 128];
        buf[16..32].copy_from_slice(b"PCM-A10         ");
        buf[36..40].copy_from_slice(b"1.02");
        buf[112] = 0x11;
        buf[113] = 0x22;
        let info = parse_mtp(&buf).unwrap();
        assert_eq!(info.product_id, "PCM-A10");
        assert_eq!(info.product_revision, "1.02");
        assert_eq!(info.fw_id, 0x11);
        assert_eq!(info.hw_id, 0x22);
    }
}
