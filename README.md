# pcm-a10-fw

A dependency-free Rust reimplementation of the firmware-update path used by
Sony's PCM-A10 updater (`PCM-A10_V101.exe`).

The shipped updater is a self-extracting archive containing an MFC front-end
(`SoftwareUpdateTool.exe`) and a COM DLL (`WmFwUpdater.dll`). The DLL performs
the actual update. This crate reimplements the **mass-storage / SCSI** path it
uses:

1. probe the recorder with standard `INQUIRY` and `TEST UNIT READY`;
2. read the Sony `REQUEST DEVINFO` vendor response;
3. copy the firmware image (`UPG.IMG`) to the root of the recorder's
   mass-storage volume;
4. send the Sony `START FW UPDATE` vendor command and wait for it to finish.

Reading firmware back (`pull`) is an **extension**. The shipped DLL contains no
firmware-read or upload command; see [Pull is an extension](#pull-is-an-extension).

> **Status:** the protocol was recovered from the shipped binaries, but this
> implementation has not been run against a physical PCM-A10. All
> hardware-dependent paths are untested. Read [Safety](#safety) before using it.

## Requirements

- Rust 1.85 or newer (developed with `rustc 1.99.1`). No external crates are
  used, so the crate builds offline.
- Linux: a SCSI generic node (`/dev/sgN`) with read-write access. Normally this
  means root or a udev rule.
- Windows: administrator privileges for SPTI (`IOCTL_SCSI_PASS_THROUGH_DIRECT`).
  The Windows transport is implemented but was not compiled or tested in this
  environment.

## Build

```sh
cargo build --release
```

The binary is `target/release/pcm-a10-fw`.

## Usage

```text
pcm-a10-fw <command> [options]

Commands:
  devices        List supported Sony recorders (alias: list)
  info           Show INQUIRY and DEVINFO for the connected recorder
  flash          Copy UPG.IMG and send START FW UPDATE (alias: update)
  pull           Copy UPG.IMG from the recorder to the host (extension)
  package-info   Show SWUpdate.xml metadata (alias: xml)
  help           Show help or help for one command
  version        Show the crate version
```

Run `pcm-a10-fw help <command>` for the full option list.

### Inspect a package

```sh
pcm-a10-fw package-info /path/to/Data/Device/SWUpdate.xml
pcm-a10-fw package-info /path/to/Data/Device --json
```

### Find and identify the recorder

```sh
pcm-a10-fw devices
pcm-a10-fw info --device /dev/sg2
pcm-a10-fw info --json
```

`devices` exits successfully with "No supported Sony recorder found." when
nothing is attached; `info`, `flash` and `pull` return an error in that case.

### Flash

```sh
# Validate and print the plan without writing anything.
pcm-a10-fw flash --image Data/Device/UPG.IMG --dry-run

# Flash, asking for confirmation.
pcm-a10-fw flash --image Data/Device/UPG.IMG

# Non-interactive flash with post-update verification.
pcm-a10-fw flash --image Data/Device/UPG.IMG --yes --verify
```

If a `SWUpdate.xml` sits next to the image, it is loaded automatically and
supplies the version, `<Timer>`, `<TimeOut>` and write path. Use `--xml` to
point at a different file.

Useful options:

| Option | Meaning |
|---|---|
| `--image <file>` | Firmware image (default `UPG.IMG` in the current directory) |
| `--xml <file>` | Explicit `SWUpdate.xml` |
| `--timeout <dur>` | Overall update deadline (`mm:ss`, `h:mm:ss`, or seconds) |
| `--timer <dur>` | SCSI command timeout (default `min(Timer, TimeOut)`) |
| `--dry-run` | Validate and print the plan, write nothing |
| `--force` | Flash even if the version is not newer or the product differs |
| `--verify` | Re-probe afterwards and compare the reported version |
| `--strict-image` | Reject images without the NuttX magic at offset 8 |
| `--remove-image` | Delete `UPG.IMG` from the device after success |
| `-y`, `--yes` | Skip the interactive confirmation |
| `--json` | Machine-readable output on stdout |
| `--mount <dir>` | Override the mass-storage mount root |
| `-d`, `--device <path>` | Explicit `/dev/sgN` or Windows `\\.\X:` |

### Pull (extension)

```sh
pcm-a10-fw pull --output recovered.img
pcm-a10-fw pull --sha256 <64-hex-digest>
```

## What the shipped updater actually does

The sequence recovered from `WmFwUpdater.dll` is:

1. Match the recorder by `INQUIRY`: vendor must be exactly `SONY    `, and the
   product must contain `WALKMAN`, `IC RECORDER`, `Smart B-Trainer` or
   `PERSONAL AUDIO`, or end in `/SU`.
2. Send `TEST UNIT READY`. Sense `02/3A/00` (NOT READY / MEDIUM NOT PRESENT) is
   tolerated because the recorder may still be booting.
3. Send Sony `REQUEST DEVINFO` and read the model, revision, FWID and HWID.
4. Delete any existing destination file, check free space, and copy the image
   to `X:\UPG.IMG` with `COpdFileHelper::FastCopy`.
5. Re-check `TEST UNIT READY`, then send Sony `START FW UPDATE` from a worker
   thread. The call blocks until the recorder finishes.
6. Report progress while waiting. The copy phase ends at 19 %; the update phase
   runs from 40 % to 95 %.

The exact command bytes, response layouts and disassembly addresses are in
[`docs/PROTOCOL.md`](docs/PROTOCOL.md).

## Observed behaviour vs. design choices

Everything below is a deliberate split between what the DLL does and what this
crate adds.

### Observed in the shipped binaries

- SCSI transport through `IOCTL_SCSI_PASS_THROUGH` / `_DIRECT` on Windows.
- `INQUIRY` with allocation length `0x60`.
- Vendor opcode `0xFC` with subsystem tag `dbmn`.
- `REQUEST DEVINFO` CDB `FC 00 20 64 62 6D 6E 00 80 00 00 00`.
- `START FW UPDATE` CDB `FC 00 04 64 62 6D 6E 00 00 00 00 00`.
- `TEST UNIT READY` as a 12-byte zeroed CDB buffer, with `02/3A/00` tolerated.
- DEVINFO layout: `"DEVINFO"`, info size `0x80`, vendor `"SONY    "`,
  `ProductId` at 24, `ProductRevision` at 40, FWID at 96, HWID at 97.
- Image copy to `%c:\%s`, deleting the old file first.
- Free-space check through `GetDiskFreeSpaceExW`.
- Progress formulas `index * 10 + 0x13` (copy) and
  `elapsed * 0x37 / timeout + 0x28` (update).
- A 1 s polling loop around a worker thread.
- Overall deadline from `<TimeOut>`; SCSI timeout
  `min(<Timer>, <TimeOut>)` in seconds.
- Fallback timeout constant `0x493e0` = 300 000 ms.
- `SWUpdate.xml` fields `Product`, `Version`, `SWPackageWritePathName`,
  `Timer`, `TimeOut`, `DisclaimerBox`.
- MTP/WPD path using vendor operations `0x9401` (DEVINFO) and `0x9402`
  (START FW UPDATE).

### Design choices in this crate

- **Linux `SG_IO` transport.** The DLL is Windows-only; this is the equivalent
  Linux interface. Windows SPTI is also implemented.
- **Pull.** Reading `UPG.IMG` back is not in the DLL. See below.
- **Product-mismatch refusal.** The front-end checks the package product; this
  crate compares the XML `<Product>` with DEVINFO `ProductId` and refuses an
  obvious mismatch unless `--force` is given. Empty fields are treated as
  unknown.
- **Version check.** A package that is not newer than the device revision is
  refused unless `--force` is given. The DLL's own decision logic was not fully
  recovered, so this is conservative.
- **64-bit free-space check.** The DLL only uses the low 32 bits of
  `GetDiskFreeSpaceExW`; this crate uses the full 64-bit value.
- **`--strict-image`.** The shipped DLL does not appear to validate the image
  header. The NuttX magic check is opt-in and otherwise only a warning.
- **Image removal is off by default.** Removal after a successful update was
  not fully verified, so `UPG.IMG` is left in place unless `--remove-image` is
  passed.
- **`--verify`.** Re-probing after the update is an added convenience.
- **Copy progress.** The DLL emits a single value (19) after `FastCopy`. This
  crate maps the byte count into 0..19 so the UI moves during a long copy.
- **JSON output and `--dry-run`.** Diagnostics added for scripting and safety.

## Safety

Flashing firmware can brick a device if it is interrupted or given the wrong
image.

- Use the image and `SWUpdate.xml` from the official package.
- Do not disconnect, power off, or reboot the recorder while `flash` is
  running.
- The recorder may disappear from the bus and re-enumerate after
  `START FW UPDATE`; this is expected.
- `--force` bypasses the version and product checks. Do not use it unless you
  know the image is correct.
- Keep the original package so you can recover.

## Pull is an extension

The shipped `WmFwUpdater.dll` only writes `UPG.IMG` to mass storage and starts
the update. It has no command that reads firmware from the recorder.

`pcm-a10-fw pull` therefore copies the `UPG.IMG` file that is present on the
recorder's mass-storage volume. It **cannot dump the recorder's internal
flash**, and on a recorder that has never received an update image there may be
nothing to pull.

## Platform notes

### Linux

- Enumerates `/sys/class/scsi_generic/sg*` and falls back to `/dev/sg*`.
- Resolves the mount root by matching `/sys/class/scsi_generic/sgN/device`
  against `/sys/class/block/<partition>/device` (walking up to the parent disk
  for partitions such as `sda1`).
- Uses `statvfs` for free space.

### Windows

- Enumerates visible drive letters with `GetLogicalDrives`, preferring
  removable drives, and opens `\\.\X:` for SPTI.
- Uses `GetDiskFreeSpaceExW` for free space.
- Requires administrator privileges.

## Tests

```sh
cargo test
cargo clippy --all-targets
cargo fmt --check
```

The unit tests cover the CDB bytes, sense parsing, DEVINFO parsing, the
`SWUpdate.xml` parser, progress formulas, SHA-256 and the CLI parser. They do
not exercise a real device.

## Evidence and provenance

Artifacts used for the analysis:

| Artifact | Size | SHA-256 |
|---|---:|---|
| `PCM-A10_V101.exe` (self-extracting package) | 14 277 416 | `d52bc806e3fbe4f811b5b1cf5d42f5714bbb8e0610531fe1b0dac91dc8eab259` |
| `SoftwareUpdateTool.exe` (front-end) | 2 310 640 | `d79d6bbb8d6e743ff84c92e26bfe0541b12abef6bac3258910aee18465a5673f` |
| `WmFwUpdater.dll` (protocol implementation) | 475 120 | `789d5df53f2bc9b8170518aa1bce83c292a35310dda4bda82e7e54e8e401250b` |
| `UPG.IMG` (firmware image) | 12 300 996 | `7b5b1f301177a20694c781728d96780603b68629e4e10f27fa3c9096fa1d6771` |

The self-extracting archive stores its CAB payload bitwise-inverted starting at
file offset `476862`. The extracted `SWUpdate.xml` used by the analysis is:

```xml
<?xml version="1.0" encoding="ISO-8859-1"?>
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
```

See [`docs/PROTOCOL.md`](docs/PROTOCOL.md) for the full evidence trail.

## Layout

```text
src/
  cli.rs           command line, output, progress
  device.rs        discovery and mount resolution
  dev_info.rs      MSC and MTP DEVINFO parsers
  error.rs         error and sense types
  flash.rs         flash plan/execute, copy, timeout, verify
  json.rs          minimal JSON writer
  pull.rs          read-back extension
  scsi.rs          CDBs, sense handling, transport trait
  scsi_linux.rs    SG_IO transport
  scsi_windows.rs  SPTI transport
  sha256.rs        dependency-free SHA-256
  update_xml.rs    SWUpdate.xml parser
docs/
  PROTOCOL.md      recovered protocol and evidence
```

## License

MIT OR Apache-2.0.
