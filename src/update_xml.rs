//! Minimal parser for the updater's `SWUpdate.xml`.
//!
//! The shipped package uses a small, attribute-free XML dialect.  This parser
//! deliberately supports only that shape; it is not a general XML
//! implementation.  It is dependency-free so the crate builds offline.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::Error;

/// One `<Device>` entry from `<Devices>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceEntry {
    /// `<DevicePropertyProductInfo>` value, when present.
    pub product_info: Option<String>,
    /// `<SubIdentification>` value, when present.
    pub sub_identification: Option<String>,
}

/// Parsed `SWUpdate.xml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdatePackage {
    /// `<Category>` (e.g. `IC RECORDER`).
    pub category: Option<String>,
    /// `<Product>` (e.g. `PCM-A10`).
    pub product: String,
    /// `<Version>` (e.g. `1.01`).
    pub version: String,
    /// Basename of `<SWPackageWritePathName>` (e.g. `UPG.IMG`).
    pub write_path: String,
    /// `<Timer>`, parsed as minutes:seconds.
    pub timer: Option<Duration>,
    /// `<TimeOut>`, parsed as minutes:seconds.
    pub timeout: Option<Duration>,
    /// `<DisclaimerBox>`.
    pub disclaimer_box: bool,
    /// `<Devices><Device>...` entries.
    pub devices: Vec<DeviceEntry>,
    /// File the package was read from, when applicable.
    pub source: Option<PathBuf>,
}

impl UpdatePackage {
    /// Read and parse a package file.
    pub fn from_path(path: &Path) -> Result<Self, Error> {
        let bytes = fs::read(path)
            .map_err(|e| Error::Xml(format!("cannot read {}: {e}", path.display())))?;
        let text = String::from_utf8_lossy(&bytes);
        let mut package = parse_str(&text)?;
        package.source = Some(path.to_path_buf());
        Ok(package)
    }
}

/// Find the content of the first `<tag>...</tag>` element.
///
/// The character after the tag name must be `>` or whitespace, so `<Device>`
/// does not accidentally match `<Devices>` or `<DevicePropertyProductInfo>`.
fn find_tag<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut offset = 0;
    while let Some(relative) = text[offset..].find(&open) {
        let start = offset + relative;
        let after = &text[start + open.len()..];
        let gt = after.find('>')?;
        let attributes = &after[..gt];
        let valid =
            attributes.is_empty() || attributes.chars().next().is_some_and(|c| c.is_whitespace());
        if valid {
            let content_start = start + open.len() + gt + 1;
            let end = text[content_start..].find(&close)?;
            return Some(&text[content_start..content_start + end]);
        }
        offset = start + open.len();
    }
    None
}

/// Find every `<tag>...</tag>` element.
fn find_all<'a>(text: &'a str, tag: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut offset = 0;
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    while offset < text.len() {
        let Some(relative) = text[offset..].find(&open) else {
            break;
        };
        let start = offset + relative;
        let Some(content) = find_tag(&text[start..], tag) else {
            break;
        };
        out.push(content);
        let content_start = content.as_ptr() as usize - text.as_ptr() as usize;
        offset = content_start + content.len() + close.len();
    }
    out
}

fn optional(text: &str, tag: &str) -> Option<String> {
    find_tag(text, tag)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn required(text: &str, tag: &str) -> Result<String, Error> {
    optional(text, tag).ok_or_else(|| Error::Xml(format!("missing <{tag}>")))
}

/// Parse an `SWUpdate.xml` document.
pub fn parse_str(text: &str) -> Result<UpdatePackage, Error> {
    let product = required(text, "Product")?;
    let version = required(text, "Version")?;
    let write_path = normalize_write_path(&required(text, "SWPackageWritePathName")?)?;

    let timer = optional(text, "Timer")
        .map(|value| parse_duration(&value))
        .transpose()?;
    let timeout = optional(text, "TimeOut")
        .map(|value| parse_duration(&value))
        .transpose()?;
    let disclaimer_box = optional(text, "DisclaimerBox")
        .map(|value| value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let mut devices = Vec::new();
    for block in find_all(text, "Device") {
        devices.push(DeviceEntry {
            product_info: optional(block, "DevicePropertyProductInfo"),
            sub_identification: optional(block, "SubIdentification"),
        });
    }

    Ok(UpdatePackage {
        category: optional(text, "Category"),
        product,
        version,
        write_path,
        timer,
        timeout,
        disclaimer_box,
        devices,
        source: None,
    })
}

/// Reduce `SWPackageWritePathName` to a safe basename.
///
/// The shipped value is `\UPG.IMG`; the DLL writes the image to the root of the
/// mass-storage volume.  Rejecting directory components keeps the write inside
/// that root.
fn normalize_write_path(raw: &str) -> Result<String, Error> {
    let unified = raw.trim().replace('\\', "/");
    let name = unified.rsplit('/').next().unwrap_or("").trim();
    if name.is_empty() || name == "." || name == ".." || name.contains('\0') {
        return Err(Error::Xml(format!(
            "invalid SWPackageWritePathName {raw:?}"
        )));
    }
    Ok(name.to_string())
}

/// Parse the updater's duration syntax.
///
/// The shipped file uses `10:00` and `9:00`, which the DLL interprets as
/// minutes:seconds (600 s and 540 s).  For convenience this parser also
/// accepts `h:mm:ss` and a plain integer number of seconds, but the primary
/// form is `mm:ss`.
pub fn parse_duration(value: &str) -> Result<Duration, Error> {
    let value = value.trim();
    let invalid = || Error::Xml(format!("invalid duration {value:?}"));
    let parts: Vec<&str> = value.split(':').collect();
    let number = |part: &str| part.trim().parse::<u64>().map_err(|_| invalid());
    match parts.as_slice() {
        [minutes, seconds] => {
            let minutes = number(minutes)?;
            let seconds = number(seconds)?;
            if seconds >= 60 {
                return Err(Error::Xml(format!(
                    "invalid duration {value:?}: seconds must be less than 60"
                )));
            }
            Ok(Duration::from_secs(
                minutes.saturating_mul(60).saturating_add(seconds),
            ))
        }
        [hours, minutes, seconds] => {
            let hours = number(hours)?;
            let minutes = number(minutes)?;
            let seconds = number(seconds)?;
            if minutes >= 60 || seconds >= 60 {
                return Err(Error::Xml(format!(
                    "invalid duration {value:?}: minutes and seconds must be less than 60"
                )));
            }
            Ok(Duration::from_secs(
                hours
                    .saturating_mul(3600)
                    .saturating_add(minutes.saturating_mul(60))
                    .saturating_add(seconds),
            ))
        }
        [seconds] => Ok(Duration::from_secs(number(seconds)?)),
        _ => Err(invalid()),
    }
}

/// Parse a dotted version such as `1.01` into `(major, minor)`.
pub fn parse_version(value: &str) -> Option<(u64, u64)> {
    let value = value.trim();
    let mut parts = value.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = match parts.next() {
        Some(minor) => minor.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor))
}

/// Compare a package version with the revision reported by the device.
///
/// Returns `None` when either string is not a dotted numeric version.
pub fn version_is_newer(candidate: &str, current: &str) -> Option<bool> {
    let candidate = parse_version(candidate)?;
    let current = parse_version(current)?;
    Some(candidate > current)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="ISO-8859-1"?>
<SWUpdate>
  <Package>
    <Category>IC RECORDER</Category>
    <Product>PCM-A10</Product>
  </Package>
  <Common>
    <Version>1.01</Version>
    <SWPackageWritePathName>\UPG.IMG</SWPackageWritePathName>
    <Timer>10:00</Timer>
    <TimeOut>9:00</TimeOut>
    <DisclaimerBox>true</DisclaimerBox>
  </Common>
  <Devices>
    <Device>
      <DevicePropertyProductInfo>PCM-A10</DevicePropertyProductInfo>
      <SubIdentification>00000000</SubIdentification>
    </Device>
  </Devices>
</SWUpdate>
"#;

    #[test]
    fn parses_the_shipped_package() {
        let package = parse_str(SAMPLE).unwrap();
        assert_eq!(package.category.as_deref(), Some("IC RECORDER"));
        assert_eq!(package.product, "PCM-A10");
        assert_eq!(package.version, "1.01");
        assert_eq!(package.write_path, "UPG.IMG");
        assert_eq!(package.timer, Some(Duration::from_secs(600)));
        assert_eq!(package.timeout, Some(Duration::from_secs(540)));
        assert!(package.disclaimer_box);
        assert_eq!(package.devices.len(), 1);
        assert_eq!(package.devices[0].product_info.as_deref(), Some("PCM-A10"));
        assert_eq!(
            package.devices[0].sub_identification.as_deref(),
            Some("00000000")
        );
    }

    #[test]
    fn duration_is_minutes_and_seconds() {
        assert_eq!(parse_duration("9:00").unwrap(), Duration::from_secs(540));
        assert_eq!(parse_duration("10:00").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("0:05").unwrap(), Duration::from_secs(5));
        assert_eq!(
            parse_duration("1:02:03").unwrap(),
            Duration::from_secs(3723)
        );
        assert_eq!(parse_duration("42").unwrap(), Duration::from_secs(42));
        assert!(parse_duration("9:60").is_err());
        assert!(parse_duration("x:y").is_err());
    }

    #[test]
    fn write_path_is_reduced_to_a_basename() {
        assert_eq!(normalize_write_path("\\UPG.IMG").unwrap(), "UPG.IMG");
        assert_eq!(normalize_write_path("\\sub\\UPG.IMG").unwrap(), "UPG.IMG");
        assert!(normalize_write_path("\\").is_err());
        assert!(normalize_write_path("..").is_err());
    }

    #[test]
    fn version_comparison() {
        assert_eq!(version_is_newer("1.01", "1.00"), Some(true));
        assert_eq!(version_is_newer("1.01", "1.01"), Some(false));
        assert_eq!(version_is_newer("1.00", "1.01"), Some(false));
        assert_eq!(version_is_newer("2", "1.99"), Some(true));
        assert_eq!(version_is_newer("abc", "1.00"), None);
    }

    #[test]
    fn missing_required_tags_are_errors() {
        let err = parse_str("<SWUpdate></SWUpdate>").unwrap_err();
        assert!(err.to_string().contains("Product"));
    }

    #[test]
    fn device_block_does_not_match_devices_or_product_info() {
        let blocks = find_all(SAMPLE, "Device");
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].contains("DevicePropertyProductInfo"));
        assert!(!blocks[0].contains("Devices"));
    }
}
