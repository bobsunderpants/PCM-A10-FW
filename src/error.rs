//! Error types shared by the transport, device and command layers.

use std::fmt;
use std::io;

/// SCSI sense data in the fixed-format layout used by the Sony updater.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sense {
    /// Sense key (`0x0`..`0xF`).
    pub key: u8,
    /// Additional sense code.
    pub asc: u8,
    /// Additional sense code qualifier.
    pub ascq: u8,
}

impl Sense {
    /// The updater explicitly tolerates `NOT READY / MEDIUM NOT PRESENT`
    /// (`02/3A/00`) because the recorder may be rebooting.
    pub fn is_not_ready_no_medium(self) -> bool {
        self.key == 0x02 && self.asc == 0x3a && self.ascq == 0x00
    }
}

impl fmt::Display for Sense {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "sense {:#04x}/{:#04x}/{:#04x}",
            self.key, self.asc, self.ascq
        )
    }
}

/// All failures produced by this crate.
#[derive(Debug)]
pub enum Error {
    /// Local filesystem / OS error.
    Io(io::Error),
    /// Command line usage error.
    Usage(String),
    /// A requested feature is not present in the shipped protocol.
    Unsupported(String),
    /// No matching device was found.
    NotFound(String),
    /// More than one matching device was found and no `--device` was given.
    Ambiguous(Vec<String>),
    /// The firmware image is missing, empty or otherwise unusable.
    InvalidImage(String),
    /// The device answered, but not with the expected identity or data.
    InvalidDevice(String),
    /// INQUIRY returned a non-Sony vendor id.
    NotSonyDevice { vendor: String, product: String },
    /// INQUIRY returned a Sony device that is not an IC recorder / Walkman.
    UnsupportedDevice { product: String },
    /// A SCSI command completed with an error status or sense data.
    Scsi {
        operation: &'static str,
        status: u8,
        sense: Option<Sense>,
        detail: String,
    },
    /// An operation exceeded its deadline.
    Timeout(String),
    /// The user declined a destructive operation.
    Refused(String),
    /// `SWUpdate.xml` could not be parsed.
    Xml(String),
    /// Internal JSON serialisation problem (should not normally happen).
    Json(String),
}

impl Error {
    pub fn scsi(
        operation: &'static str,
        status: u8,
        sense: Option<Sense>,
        detail: impl Into<String>,
    ) -> Self {
        Self::Scsi {
            operation,
            status,
            sense,
            detail: detail.into(),
        }
    }

    pub fn is_permission_denied(&self) -> bool {
        matches!(self, Self::Io(e) if e.kind() == io::ErrorKind::PermissionDenied)
    }

    /// Process exit code: `2` for command-line usage errors, `1` otherwise.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) => 2,
            _ => 1,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Usage(msg) => write!(f, "{msg}"),
            Self::Unsupported(msg) => write!(f, "unsupported: {msg}"),
            Self::NotFound(msg) => write!(f, "device not found: {msg}"),
            Self::Ambiguous(paths) => write!(
                f,
                "more than one matching device was found; use --device <path>:\n  {}",
                paths.join("\n  ")
            ),
            Self::InvalidImage(msg) => write!(f, "invalid firmware image: {msg}"),
            Self::InvalidDevice(msg) => write!(f, "invalid device response: {msg}"),
            Self::NotSonyDevice { vendor, product } => write!(
                f,
                "device is not a Sony target (INQUIRY vendor={vendor:?}, product={product:?})"
            ),
            Self::UnsupportedDevice { product } => write!(
                f,
                "Sony device {product:?} is not one of the supported IC recorder / Walkman families"
            ),
            Self::Scsi {
                operation,
                status,
                sense,
                detail,
            } => {
                write!(f, "SCSI command {operation} failed (status {status:#04x}")?;
                if let Some(s) = sense {
                    write!(f, ", {s}")?;
                }
                if !detail.is_empty() {
                    write!(f, ", {detail}")?;
                }
                write!(f, ")")
            }
            Self::Timeout(msg) => write!(f, "timeout: {msg}"),
            Self::Refused(msg) => write!(f, "{msg}"),
            Self::Xml(msg) => write!(f, "SWUpdate.xml: {msg}"),
            Self::Json(msg) => write!(f, "JSON: {msg}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
