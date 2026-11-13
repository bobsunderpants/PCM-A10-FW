# PCM-A10 firmware update protocol

This document records the protocol recovered from Sony's PCM-A10 v1.01
updater and maps it to the Rust implementation in this crate.

Evidence status is marked as:

- **Verified** — read directly from the shipped binaries (disassembly, strings
  or data).
- **Inferred** — a reasoned conclusion that was not proven byte-for-byte.
- **Extension** — not present in the shipped code; added by this crate.

The DLL image base is `0x10000000`; the front-end image base is `0x400000`.

## 1. Artifacts

| Artifact | Size | SHA-256 |
|---|---:|---|
| `PCM-A10_V101.exe` (self-extracting package) | 14 277 416 | `d52bc806e3fbe4f811b5b1cf5d42f5714bbb8e0610531fe1b0dac91dc8eab259` |
| `SoftwareUpdateTool.exe` (MFC front-end) | 2 310 640 | `d79d6bbb8d6e743ff84c92e26bfe0541b12abef6bac3258910aee18465a5673f` |
| `WmFwUpdater.dll` (COM updater) | 475 120 | `789d5df53f2bc9b8170518aa1bce83c292a35310dda4bda82e7e54e8e401250b` |
| `UPG.IMG` (firmware image) | 12 300 996 | `7b5b1f301177a20694c781728d96780603b68629e4e10f27fa3c9096fa1d6771` |

The self-extracting package stores its CAB payload bitwise-inverted starting at
file offset `476862`. Inverting the bytes from that offset produces a normal
CAB, which `7z` extracts into:

```text
SoftwareUpdateTool.exe
Data/App/WmFwUpdater.dll
Data/Device/SWUpdate.xml
Data/Device/UPG.IMG
```

**Verified** for the hashes and offsets above.

## 2. Shipped architecture

`SoftwareUpdateTool.exe` is an MFC application that loads the COM class
`WmFwUpdater.WmHwcManager` and drives it through `IWmHwcManager`:

- CLSID `{95C01955-6E48-4FF1-B7A9-B0EA7B1C6259}`
- class `WmFwUpdater.WmHwcManager`
- interface `IWmHwcManager`

The front-end reads `.\Data\Device\SWUpdate.xml`, displays the disclaimer and
progress, and calls the manager. The manager chooses between two transports:

- **MSC** — USB mass storage, SCSI pass-through. This is the path implemented
  by this crate.
- **MTP** — Windows WPD / MTP. Documented in section 10 but not implemented,
  because it depends on the Windows WPD API.

Relevant front-end strings include `E_FW_UPDATE_TIMEOUT`,
`E_FW_UPDATE_*` and `S_FW_UPDATE_COMPLETED`. **Verified** from strings.

`PA001` is the `CategoryId` passed to
`CWmHwcManager::GetDeviceSpecificString`, which selects the localized product
name (`WALKMAN` / `ウォークマン`). It is **not** a flash-protocol parameter.
**Verified** from the front-end/DLL string usage; the exact call graph for the
localization lookup was not reconstructed.

## 3. Transport

### Windows (shipped)

The DLL sends commands with `DeviceIoControl` using the SCSI pass-through
interface:

- `IOCTL_SCSI_PASS_THROUGH` = `0x0004D004`
- `IOCTL_SCSI_PASS_THROUGH_DIRECT` = `0x0004D014`
- CDB lengths from 1 to 16 bytes
- sense data parsed from the SCSI sense buffer
- error strings `SendInDataCmdDirect( ... )` and
  `SendNoDataCmdDirect( ... )`

**Verified** from imports, constants and strings. The direct/no-data helpers
are `FUN_1003d980` (`SendInDataCmdDirect`) and `FUN_1003d750`
(`SendNoDataCmdDirect`).

### Linux (this crate)

The equivalent interface is `SG_IO` on `/dev/sgN`:

- ioctl request `0x2285`
- `interface_id = 'S'`
- `SG_DXFER_NONE = -1`, `SG_DXFER_TO_DEV = -2`, `SG_DXFER_FROM_DEV = -3`
- `timeout` in milliseconds
- fixed-format sense: response code `0x70`/`0x71`, key in byte 2, ASC in
  byte 12, ASCQ in byte 13

The node must be opened read-write for vendor commands. **Extension** (the DLL
is Windows-only).

### Windows (this crate)

`IOCTL_SCSI_PASS_THROUGH_DIRECT` with a `SCSI_PASS_THROUGH_DIRECT` structure:

- `DataIn` = `SCSI_IOCTL_DATA_IN` (1), `SCSI_IOCTL_DATA_OUT` (0),
  `SCSI_IOCTL_DATA_UNSPECIFIED` (2)
- `TimeOutValue` in seconds
- sense buffer placed immediately after the structure

**Extension** relative to the DLL's own code, though it uses the same OS
interface. Not compiled or tested in this environment.

## 4. Command set

### 4.1 INQUIRY

Builder `FUN_10020428`; the caller writes allocation length `0x60` at
`0x10020d91`. Sender `FUN_10020c75`.

| CDB byte | Value |
|---:|---|
| 0 | `0x12` |
| 1 | `0x00` |
| 2 | `0x00` |
| 3 | `0x00` |
| 4 | `0x60` |
| 5 | `0x00` |

Hex: `12 00 00 00 60 00`.

Response is parsed as standard INQUIRY:

| Offset | Size | Field |
|---:|---:|---|
| 8 | 8 | Vendor identification |
| 16 | 16 | Product identification |
| 32 | 4 | Product revision |

The DLL requires the vendor field to be exactly `"SONY    "`. The product must
contain `WALKMAN`, `IC RECORDER`, `Smart B-Trainer` or `PERSONAL AUDIO`, or end
in `/SU` (case-insensitive). **Verified**.

### 4.2 TEST UNIT READY

Sender `FUN_10020a25`. The DLL passes a 12-byte zeroed buffer to
`SendNoDataCmdDirect`, i.e. opcode `0x00` plus zero padding.

Hex: `00 00 00 00 00 00 00 00 00 00 00 00`.

Completion handling is special: sense `0x23A00` — key `0x02`, ASC `0x3A`,
ASCQ `0x00` (NOT READY / MEDIUM NOT PRESENT) — is treated as success. Other
non-zero sense keys fail. **Verified**.

The DLL passes 180 s (`0xB4`) to its timeout setter in the probe/open paths
(`0x1001e807`, `0x100200e2`); this crate uses 180 s for TUR. **Verified** for
the `0xB4` call sites; the association with the readiness path is
**inferred**.

### 4.3 REQUEST DEVINFO

CDB builder `FUN_10020503`; sender `FUN_10020ead`.

| CDB byte | Value |
|---:|---|
| 0 | `0xFC` |
| 1 | `0x00` |
| 2 | `0x20` |
| 3 | `0x64` (`d`) |
| 4 | `0x62` (`b`) |
| 5 | `0x6D` (`m`) |
| 6 | `0x6E` (`n`) |
| 7 | `0x00` |
| 8 | `0x80` |
| 9 | `0x00` |
| 10 | `0x00` |
| 11 | `0x00` |

Hex: `FC 00 20 64 62 6D 6E 00 80 00 00 00`.

Data-in length is `0x80` bytes. **Verified**.

### 4.4 START FW UPDATE

CDB builder `FUN_1002053f`; actual sender `FUN_100214f4`, called from
`0x100220f5`.

| CDB byte | Value |
|---:|---|
| 0 | `0xFC` |
| 1 | `0x00` |
| 2 | `0x04` |
| 3 | `0x64` (`d`) |
| 4 | `0x62` (`b`) |
| 5 | `0x6D` (`m`) |
| 6 | `0x6E` (`n`) |
| 7–11 | `0x00` |

Hex: `FC 00 04 64 62 6D 6E 00 00 00 00 00`.

No data phase. The call blocks until the recorder finishes programming.
**Verified**.

The four-byte tag `dbmn` is shared by both vendor commands. Its meaning is
unknown; it is probably a subsystem identifier. **Inferred**.

## 5. Completion and sense rules

The DLL treats a command as failed when:

- the sense key is non-zero, or
- the SCSI status is non-zero and there is no sense key.

The exception is TUR, which accepts `02/3A/00`. **Verified** by the
`sk/asc/ascq` error strings and the TUR-specific comparison against `0x23A00`.

This crate implements the same rule in `scsi::check_completion`, with the TUR
exception in `scsi::test_unit_ready`.

## 6. DEVINFO layouts

### 6.1 MSC layout (implemented)

Recovered from `FUN_10020ead`:

| Offset | Size | Field |
|---:|---:|---|
| 0 | 7 | `"DEVINFO"` |
| 7 | 1 | info size, must be `0x80` |
| 16 | 8 | vendor, must be `"SONY    "` |
| 24 | 16 | `ProductId` |
| 40 | 4 | `ProductRevision` |
| 96 | 1 | FWID |
| 97 | 1 | HWID |

The DLL validates the property flag, info size and vendor in that order. FWID
and HWID are formatted with `%02x`. **Verified**.

### 6.2 MTP layout (documented, not implemented)

The MTP/WPD path uses different offsets:

| Offset | Size | Field |
|---:|---:|---|
| 16 | 16 | `ProductId` |
| 36 | 4 | `ProductRevision` |
| 112 | 1 | FWID |
| 113 | 1 | HWID |

The shift is because the MTP response does not have the MSC property-flag and
vendor prefix. **Verified** from the WPD parsing code.

## 7. Update sequence

Recovered from `FUN_100221a7` (orchestration), `FUN_1002180e` (copy) and
`FUN_10021f48` (START worker).

1. **Probe.** `INQUIRY` → vendor/product check → `TEST UNIT READY` →
   `REQUEST DEVINFO`.
2. **Prepare the volume.** Build the destination as `%c:\%s` (wide string),
   where `%c` is the drive letter and `%s` is the write path from
   `SWUpdate.xml` (`UPG.IMG`). Delete any existing file at that path.
3. **Check space.** `GetFileSize` for the image and `GetDiskFreeSpaceExW` for
   the volume. The DLL only checks the low 32 bits of the free-space value.
4. **Copy.** `COpdFileHelper::FastCopy` copies the image to the volume root.
   The DLL reports progress `index * 10 + 0x13`, i.e. `19` for the single
   FastCopy call.
5. **Re-check readiness.** The START worker re-opens the drive by index and
   sends `TEST UNIT READY` again, because the recorder can be busy after
   receiving a large file.
6. **Start the update.** The worker sets the SCSI timeout and calls
   `FUN_100214f4` with the `START FW UPDATE` CDB.
7. **Wait.** The orchestrator polls the worker every 1 s and reports progress
   until the worker returns or the overall deadline expires.
8. **Finish.** A successful START is reported as completion. The DLL's handling
   of `UPG.IMG` afterwards was not fully recovered; see section 12.

**Verified** for steps 1–7 at the function/address level. Step 8 is
**inferred**.

## 8. Timeouts and progress

### 8.1 Default timeout

The DLL contains a fallback constant `0x493E0` = 300 000 ms at `0x1001a53e`.
**Verified**.

### 8.2 Effective SCSI timeout

At `0x1002205f`–`0x10022082` the DLL computes:

```text
scsi_timeout_seconds = min(timer_ms, timeout_ms) / 1000
```

where the two values are read from the updater object at offsets `+0x58` and
`+0x54`. The result is passed to the timeout setter at `0x10022086`
(`FUN_100208ca`; nearby strings include `CWmFwScsiCmd::SetTimeOut`).
**Verified** for the computation; the setter name is **inferred** from strings.

With the shipped `SWUpdate.xml`:

- `<Timer>10:00</Timer>` → 600 s
- `<TimeOut>9:00</TimeOut>` → 540 s
- effective SCSI timeout → 540 s

The values are interpreted as `mm:ss`. This is an **interpretation** of the
shipped values; `9:00` as 540 s is consistent with the updater's update window
and progress model.

### 8.3 Overall deadline

The worker loop reads the deadline from `[obj+0x54]` and compares the elapsed
time against it (`0x1002243f`–`0x1002244c`). When the deadline is exceeded the
DLL reports `E_FW_UPDATE_TIMEOUT`. **Verified** for the comparison and string.

### 8.4 Progress formulas

Copy phase, at `0x10021c5c`/`0x10021c5f`:

```text
percent = index * 10 + 0x13
```

Update phase, at `0x10022494`/`0x1002249e`:

```text
percent = elapsed * 0x37 / timeout + 0x28
```

`0x37` = 55 and `0x28` = 40, so the update phase runs from 40 % to 95 %.
**Verified**.

This crate reproduces the update formula exactly. For the copy phase it emits
19 at completion, but interpolates 0..19 during the copy so the UI moves.

## 9. `SWUpdate.xml`

The shipped file:

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

The front-end strings confirm the element names `Product`, `Version`,
`SWPackageWritePathName`, `Timer`, `TimeOut`, `DisclaimerBox`,
`DevicePropertyProductInfo`, `SubIdentification` and `Category`. **Verified**
from the front-end and DLL strings.

`SWPackageWritePathName` is `\UPG.IMG`. This crate reduces it to a basename so
the write stays inside the mount root.

`Timer` and `TimeOut` are parsed as `mm:ss`; `h:mm:ss` and plain seconds are
also accepted as a convenience. **Extension** for the extra forms.

## 10. MTP / WPD path (not implemented)

The DLL has a second transport for MTP devices. It uses vendor operations:

| Operation | Code | Purpose |
|---|---:|---|
| DEVINFO | `0x9401` | Read device information |
| START FW UPDATE | `0x9402` | Start the update |

The START operation takes parameters `0x312e3030` (`"1.00"`) and
`0x64626D6E` (`"dbmn"`). **Verified** from the WPD code.

This crate does not implement MTP. A faithful implementation would need the
Windows WPD API (`IPortableDevice`, `IPortableDeviceValues`, vendor extended
commands), which is outside a dependency-free portable crate. On Linux, MTP
would require libmtp or a raw USB/MTP implementation.

## 11. `UPG.IMG`

- Size: 12 300 996 bytes.
- SHA-256: `7b5b1f301177a20694c781728d96780603b68629e4e10f27fa3c9096fa1d6771`.
- Contains the ASCII magic `nuttx` at offset 8, i.e. it is a NuttX firmware
  image.

**Verified** by inspecting the file. The DLL does not appear to validate this
magic before copying; this crate only warns unless `--strict-image` is used.

## 12. Behaviour after START FW UPDATE

The DLL blocks on the vendor command and reports progress while waiting. What
happens on the device after a successful return (reboot, USB re-enumeration,
volume disappearing, whether `UPG.IMG` is deleted) was not observed, because no
PCM-A10 was available.

Consequences for this crate:

- `UPG.IMG` is **not** removed by default. `--remove-image` is an explicit
  extension.
- `--verify` re-probes the device for up to 120 s, because the recorder may
  re-enumerate. It is an extension.
- The tool does not attempt to reset or re-open the device after START beyond
  what `--verify` does.

## 13. Rust mapping

| Protocol element | Rust location |
|---|---|
| CDB builders | `src/scsi.rs` (`inquiry_cdb`, `request_devinfo_cdb`, `start_fw_update_cdb`, `test_unit_ready_cdb`) |
| Sense parsing and completion rules | `src/scsi.rs` (`parse_sense`, `check_completion`, `test_unit_ready`) |
| DEVINFO parsers | `src/dev_info.rs` (`parse_msc`, `parse_mtp`) |
| Device discovery and mounts | `src/device.rs` |
| Flash plan/execute | `src/flash.rs` |
| Progress formulas | `src/flash.rs` (`copy_progress`, `update_progress`) |
| `SWUpdate.xml` parser | `src/update_xml.rs` |
| Linux SG_IO | `src/scsi_linux.rs` |
| Windows SPTI | `src/scsi_windows.rs` |
| Pull extension | `src/pull.rs` |
| CLI and JSON | `src/cli.rs`, `src/json.rs` |

## 14. Evidence index

| Address / item | Meaning |
|---|---|
| `0x10020428` | INQUIRY CDB builder |
| `0x10020d91` | Caller writes allocation length `0x60` |
| `0x10020c75` | INQUIRY sender |
| `0x10020503` | REQUEST DEVINFO CDB builder |
| `0x10020ead` | REQUEST DEVINFO sender and response parser |
| `0x1002053f` | START FW UPDATE CDB builder |
| `0x100214f4` | START FW UPDATE sender |
| `0x100220f5` | Call site of the START sender |
| `0x10020a25` | TEST UNIT READY sender |
| `0x10020b14` | TUR path: zeroes the 12-byte CDB and calls the no-data sender |
| `0x1002180e` | Copy-to-device routine |
| `0x10021c5c`, `0x10021c5f` | Copy progress `index * 10 + 0x13` |
| `0x100221a7` | Update orchestration and polling |
| `0x1002238c` | 1 s poll interval (`0x3E8`) |
| `0x1002243f`–`0x1002244c` | Deadline comparison |
| `0x10022494`, `0x1002249e` | Update progress `elapsed * 0x37 / timeout + 0x28` |
| `0x1002205f`–`0x10022082` | SCSI timeout `min(timer, timeout) / 1000` |
| `0x10022086` | Timeout setter call (`FUN_100208ca`) |
| `0x10021f48` | START worker (TUR, timeout, send) |
| `0x1001a53e` | Default timeout `0x493E0` = 300 000 ms |
| `0x1003d980` | `SendInDataCmdDirect` |
| `0x1003d750` | `SendNoDataCmdDirect` |
| `0x1004e128` | `"nTimeOut: %u"` |
| `0x1004e274` | `"nTimeOut <= 0."` |
| `0x1004f2e8` | `"TimeOut"` XML element |
| `0x1004ff48` | `"WmFwScsi.SetTimeOut() failed."` |
| `0x100599a8` | SPTI `DeviceIoControl` error format |
| `0x10049120` | `GetDiskFreeSpaceExW` import |
| `0x100490f8` | `GetTickCount` import used for elapsed time |

**Verified** for all rows unless noted in the surrounding text.

## 15. Unknowns and limitations

- **No hardware validation.** No PCM-A10 was attached, so no command in this
  crate has been observed on a real device.
- **Windows transport untested.** `scsi_windows.rs` was written against the
  documented SPTI ABI but was not compiled in this environment.
- **MTP not implemented.** Only the MSC/SCSI path is functional.
- **No internal flash read.** The DLL has no read/upload command; `pull` only
  reads `UPG.IMG` from mass storage.
- **Post-update behaviour.** Reboot timing, re-enumeration and image cleanup
  were not observed.
- **Front-end decision logic.** The exact package-product/version checks in the
  MFC front-end were not fully reconstructed; this crate uses conservative
  heuristics and `--force`.
- **`dbmn` tag meaning.** Unknown; it is reproduced verbatim.
- **`Timer`/`TimeOut` units.** Interpreted as `mm:ss`; the shipped values are
  consistent with that reading, but the DLL's parser was not reconstructed
  instruction by instruction.
