//! Command-line interface.
//!
//! Commands mirror the two operations the shipped updater performs, plus a
//! read-back extension:
//!
//! * `devices` / `info` probe a connected recorder;
//! * `flash` copies `UPG.IMG` and sends `START FW UPDATE`;
//! * `pull` copies `UPG.IMG` back from the recorder (extension);
//! * `package-info` inspects `SWUpdate.xml`.

use std::ffi::OsString;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::device::{self, Probe};
use crate::error::Error;
use crate::flash::{self, FlashOptions, FlashOutcome, FlashPlan, Progress};
use crate::json::Json;
use crate::pull::{self, PullOptions, PullOutcome};
use crate::scsi::Transport;
use crate::update_xml::{self, UpdatePackage};

/// Subcommand selected on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Devices,
    Info,
    Flash,
    Pull,
    PackageInfo,
    Help,
    Version,
}

impl Command {
    fn from_name(name: &str) -> Result<Self, Error> {
        match name {
            "devices" | "list" | "ls" => Ok(Self::Devices),
            "info" | "status" => Ok(Self::Info),
            "flash" | "update" => Ok(Self::Flash),
            "pull" | "read" | "dump" => Ok(Self::Pull),
            "package-info" | "xml" => Ok(Self::PackageInfo),
            "help" => Ok(Self::Help),
            "version" => Ok(Self::Version),
            other => Err(Error::Usage(format!(
                "unknown command {other:?}; run `pcm-a10-fw help`"
            ))),
        }
    }
}

/// Parsed command line.
#[derive(Debug, Clone)]
struct Cli {
    command: Command,
    help_topic: Option<Command>,
    positional: Vec<String>,
    device: Option<PathBuf>,
    mount: Option<PathBuf>,
    image: Option<PathBuf>,
    xml: Option<PathBuf>,
    output: Option<PathBuf>,
    timeout: Option<Duration>,
    timer: Option<Duration>,
    sha256: Option<String>,
    json: bool,
    yes: bool,
    dry_run: bool,
    force: bool,
    verify: bool,
    strict_image: bool,
    remove_image: bool,
    verbose: bool,
    help: bool,
    version: bool,
}

impl Default for Cli {
    fn default() -> Self {
        Self {
            command: Command::Help,
            help_topic: None,
            positional: Vec::new(),
            device: None,
            mount: None,
            image: None,
            xml: None,
            output: None,
            timeout: None,
            timer: None,
            sha256: None,
            json: false,
            yes: false,
            dry_run: false,
            force: false,
            verify: false,
            strict_image: false,
            remove_image: false,
            verbose: false,
            help: false,
            version: false,
        }
    }
}

impl Cli {
    fn parse(args: Vec<OsString>) -> Result<Self, Error> {
        let mut cli = Self::default();
        let mut command: Option<Command> = None;
        let mut iter = args.into_iter();

        while let Some(arg) = iter.next() {
            let text = arg.to_string_lossy().into_owned();

            if text == "--" {
                for rest in iter.by_ref() {
                    cli.positional.push(rest.to_string_lossy().into_owned());
                }
                break;
            }

            if let Some(rest) = text.strip_prefix("--") {
                let (name, inline) = match rest.split_once('=') {
                    Some((name, value)) => (name, Some(value.to_string())),
                    None => (rest, None),
                };
                match name {
                    "help" => {
                        reject_value(inline, "--help")?;
                        cli.help = true;
                    }
                    "version" => {
                        reject_value(inline, "--version")?;
                        cli.version = true;
                    }
                    "json" => {
                        reject_value(inline, "--json")?;
                        cli.json = true;
                    }
                    "yes" => {
                        reject_value(inline, "--yes")?;
                        cli.yes = true;
                    }
                    "dry-run" => {
                        reject_value(inline, "--dry-run")?;
                        cli.dry_run = true;
                    }
                    "force" => {
                        reject_value(inline, "--force")?;
                        cli.force = true;
                    }
                    "verify" => {
                        reject_value(inline, "--verify")?;
                        cli.verify = true;
                    }
                    "strict-image" => {
                        reject_value(inline, "--strict-image")?;
                        cli.strict_image = true;
                    }
                    "remove-image" => {
                        reject_value(inline, "--remove-image")?;
                        cli.remove_image = true;
                    }
                    "verbose" => {
                        reject_value(inline, "--verbose")?;
                        cli.verbose = true;
                    }
                    "device" => {
                        cli.device =
                            Some(PathBuf::from(take_value(inline, &mut iter, "--device")?));
                    }
                    "mount" => {
                        cli.mount = Some(PathBuf::from(take_value(inline, &mut iter, "--mount")?));
                    }
                    "image" => {
                        cli.image = Some(PathBuf::from(take_value(inline, &mut iter, "--image")?));
                    }
                    "xml" => {
                        cli.xml = Some(PathBuf::from(take_value(inline, &mut iter, "--xml")?));
                    }
                    "output" => {
                        cli.output =
                            Some(PathBuf::from(take_value(inline, &mut iter, "--output")?));
                    }
                    "sha256" => {
                        cli.sha256 = Some(take_value(inline, &mut iter, "--sha256")?);
                    }
                    "timeout" => {
                        let value = take_value(inline, &mut iter, "--timeout")?;
                        cli.timeout = Some(parse_cli_duration(&value, "--timeout")?);
                    }
                    "timer" => {
                        let value = take_value(inline, &mut iter, "--timer")?;
                        cli.timer = Some(parse_cli_duration(&value, "--timer")?);
                    }
                    other => {
                        return Err(Error::Usage(format!("unknown option --{other}")));
                    }
                }
                continue;
            }

            if text.starts_with('-') && text.len() > 1 {
                match text.as_str() {
                    "-h" => cli.help = true,
                    "-V" => cli.version = true,
                    "-y" => cli.yes = true,
                    "-v" => cli.verbose = true,
                    "-d" => {
                        cli.device = Some(PathBuf::from(take_value(None, &mut iter, "-d")?));
                    }
                    "-o" => {
                        cli.output = Some(PathBuf::from(take_value(None, &mut iter, "-o")?));
                    }
                    other => {
                        return Err(Error::Usage(format!("unknown option {other}")));
                    }
                }
                continue;
            }

            if command.is_none() {
                command = Some(Command::from_name(&text)?);
            } else {
                cli.positional.push(text);
            }
        }

        let mut command = command.unwrap_or(Command::Help);
        if cli.version {
            command = Command::Version;
        } else if cli.help {
            if command != Command::Help {
                cli.help_topic = Some(command);
            }
            command = Command::Help;
        }
        cli.command = command;
        Ok(cli)
    }
}

fn take_value(
    inline: Option<String>,
    iter: &mut impl Iterator<Item = OsString>,
    flag: &str,
) -> Result<String, Error> {
    match inline {
        Some(value) => Ok(value),
        None => iter
            .next()
            .map(|value| value.to_string_lossy().into_owned())
            .ok_or_else(|| Error::Usage(format!("{flag} requires a value"))),
    }
}

fn reject_value(inline: Option<String>, flag: &str) -> Result<(), Error> {
    if inline.is_some() {
        Err(Error::Usage(format!("{flag} does not take a value")))
    } else {
        Ok(())
    }
}

fn parse_cli_duration(value: &str, flag: &str) -> Result<Duration, Error> {
    update_xml::parse_duration(value).map_err(|_| {
        Error::Usage(format!(
            "{flag} expects mm:ss, h:mm:ss or a number of seconds, got {value:?}"
        ))
    })
}

/// Run the CLI.  Returns the process exit code on success.
pub fn run(args: Vec<OsString>) -> Result<i32, Error> {
    let cli = Cli::parse(args)?;
    match cli.command {
        Command::Help => {
            let topic = cli.help_topic.or_else(|| {
                cli.positional
                    .first()
                    .and_then(|name| Command::from_name(name).ok())
            });
            print_help(topic);
            Ok(0)
        }
        Command::Version => {
            println!("pcm-a10-fw {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        Command::Devices => cmd_devices(&cli),
        Command::Info => cmd_info(&cli),
        Command::Flash => cmd_flash(&cli),
        Command::Pull => cmd_pull(&cli),
        Command::PackageInfo => cmd_package_info(&cli),
    }
}

fn resolve(cli: &Cli) -> Result<device::Target, Error> {
    device::resolve(cli.device.as_deref(), cli.mount.as_deref(), cli.verbose)
}

fn cmd_devices(cli: &Cli) -> Result<i32, Error> {
    let targets = match device::discover(cli.mount.as_deref(), cli.verbose) {
        Ok(targets) => targets,
        Err(Error::NotFound(message)) => {
            if cli.json {
                let value = Json::object([
                    ("devices", Json::Array(Vec::new())),
                    ("note", Json::str(message)),
                ]);
                println!("{}", value.render());
            } else {
                println!("No supported Sony recorder found.");
                if cli.verbose {
                    println!("{message}");
                }
            }
            return Ok(0);
        }
        Err(error) => return Err(error),
    };

    if cli.json {
        let devices = targets
            .iter()
            .map(|(probe, _)| probe_json(probe))
            .collect::<Vec<_>>();
        println!(
            "{}",
            Json::object([("devices", Json::Array(devices))]).render()
        );
    } else {
        println!("{} device(s) found", targets.len());
        for (probe, _) in &targets {
            println!();
            print_probe_human(probe);
        }
    }
    Ok(0)
}

fn cmd_info(cli: &Cli) -> Result<i32, Error> {
    let (probe, _transport) = resolve(cli)?;
    if cli.json {
        println!("{}", probe_json(&probe).render());
    } else {
        print_probe_human(&probe);
    }
    Ok(0)
}

fn cmd_flash(cli: &Cli) -> Result<i32, Error> {
    let (probe, transport) = resolve(cli)?;
    let image = cli
        .image
        .clone()
        .unwrap_or_else(|| PathBuf::from("UPG.IMG"));
    let package = load_package(cli, &image)?;
    let options = FlashOptions {
        image,
        package,
        timeout: cli.timeout,
        timer: cli.timer,
        force: cli.force,
        dry_run: cli.dry_run,
        remove_image: cli.remove_image,
        verify: cli.verify,
        strict_image: cli.strict_image,
        verify_timeout: Duration::from_secs(120),
    };
    let plan = flash::plan(&probe, &options)?;

    if cli.json {
        if options.dry_run {
            let value = Json::object([
                ("dry_run", Json::Bool(true)),
                ("plan", flash_plan_json(&plan)),
            ]);
            println!("{}", value.render());
            return Ok(0);
        }
        if !cli.yes {
            return Err(Error::Refused(
                "refusing to flash without --yes when --json is used".into(),
            ));
        }
        let outcome = run_flash(probe, transport, plan, &options)?;
        let value = Json::object([
            ("plan", flash_plan_json(&outcome.plan)),
            ("result", flash_result_json(&outcome)),
        ]);
        println!("{}", value.render());
        return Ok(0);
    }

    print_flash_plan(&plan);
    if options.dry_run {
        println!("\nDry run: nothing was written.");
        return Ok(0);
    }
    confirm_flash(&plan, cli.yes)?;

    let outcome = run_flash(probe, transport, plan, &options)?;
    print_flash_outcome(&outcome);
    Ok(0)
}

fn run_flash(
    probe: Probe,
    transport: Box<dyn Transport>,
    plan: FlashPlan,
    options: &FlashOptions,
) -> Result<FlashOutcome, Error> {
    let mut status = StatusLine::new();
    let outcome = flash::execute(
        probe,
        transport,
        plan,
        options,
        &mut |progress: Progress| {
            status.report(progress.stage.as_str(), progress.percent);
        },
    )?;
    status.finish();
    Ok(outcome)
}

fn cmd_pull(cli: &Cli) -> Result<i32, Error> {
    let (probe, _transport) = resolve(cli)?;
    let output = cli
        .output
        .clone()
        .unwrap_or_else(|| PathBuf::from("UPG.IMG"));
    let options = PullOptions {
        output,
        force: cli.force,
        expected_sha256: cli.sha256.clone(),
    };
    let plan = pull::plan(&probe, &options)?;

    if !cli.json {
        println!("Device:  {}", plan.device_path.display());
        println!(
            "Model:   {} ({})",
            plan.device_product, plan.device_revision
        );
        println!("Source:  {}", plan.source.display());
        println!("Output:  {}", plan.output.display());
        println!("Size:    {} bytes", plan.size);
        if let Some(package) = &plan.package {
            println!("Package: {} {}", package.product, package.version);
        }
    }

    let mut status = StatusLine::new();
    let outcome = pull::execute(plan, &options, &mut |percent| {
        status.report("pull", percent);
    })?;
    status.finish();

    if cli.json {
        println!("{}", pull_outcome_json(&outcome).render());
    } else {
        println!();
        println!(
            "Wrote {} bytes to {}",
            outcome.bytes_written,
            outcome.plan.output.display()
        );
        println!("SHA-256: {}", outcome.sha256);
        if let Some(verified) = outcome.verified {
            println!("Verified: {}", if verified { "yes" } else { "no" });
        }
    }
    Ok(0)
}

fn cmd_package_info(cli: &Cli) -> Result<i32, Error> {
    let path = cli
        .positional
        .first()
        .map(PathBuf::from)
        .or_else(|| cli.xml.clone())
        .unwrap_or_else(|| PathBuf::from("SWUpdate.xml"));
    let path = if path.is_dir() {
        path.join("SWUpdate.xml")
    } else {
        path
    };
    let package = UpdatePackage::from_path(&path)?;
    if cli.json {
        println!("{}", package_json(&package).render());
    } else {
        print_package_human(&package);
    }
    Ok(0)
}

fn load_package(cli: &Cli, image: &Path) -> Result<Option<UpdatePackage>, Error> {
    if let Some(xml) = &cli.xml {
        return Ok(Some(UpdatePackage::from_path(xml)?));
    }
    let parent = image
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let sibling = parent.join("SWUpdate.xml");
    if sibling.is_file() {
        return Ok(Some(UpdatePackage::from_path(&sibling)?));
    }
    Ok(None)
}

fn confirm_flash(plan: &FlashPlan, yes: bool) -> Result<(), Error> {
    if yes {
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        return Err(Error::Refused(
            "refusing to flash without --yes (stdin is not a terminal)".into(),
        ));
    }
    eprintln!();
    eprintln!(
        "This will write {} to {}",
        plan.image.display(),
        plan.destination.display()
    );
    eprintln!("and then send START FW UPDATE. Do not disconnect the recorder.");
    eprint!("Proceed? [y/N] ");
    io::stderr().flush().ok();
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    let answer = line.trim().to_ascii_lowercase();
    if answer == "y" || answer == "yes" {
        Ok(())
    } else {
        Err(Error::Refused("aborted by user".into()))
    }
}

fn print_probe_human(probe: &Probe) {
    println!("Device:   {}", probe.scsi_path.display());
    println!(
        "Inquiry:  {} {} ({})",
        probe.inquiry.vendor, probe.inquiry.product, probe.inquiry.revision
    );
    println!("Model:    {}", probe.dev_info.product_id);
    println!(
        "Firmware: {} (FWID {}, HWID {})",
        probe.dev_info.product_revision,
        probe.dev_info.fw_id_hex(),
        probe.dev_info.hw_id_hex()
    );
    match &probe.mount_root {
        Some(root) => println!("Mount:    {}", root.display()),
        None => println!("Mount:    <not mounted>"),
    }
}

fn print_flash_plan(plan: &FlashPlan) {
    println!("Device:    {}", plan.device_path.display());
    println!(
        "Model:     {} (INQUIRY: {})",
        plan.device_product_id, plan.device_product
    );
    println!("Mount:     {}", plan.mount_root.display());
    println!(
        "Image:     {} ({} bytes)",
        plan.image.display(),
        plan.image_size
    );
    println!("SHA-256:   {}", plan.image_sha256);
    println!("Write to:  {}", plan.destination.display());
    match (&plan.package_version, &plan.version_newer) {
        (Some(package), Some(true)) => {
            println!("Version:   {} -> {} (newer)", plan.device_revision, package);
        }
        (Some(package), Some(false)) => {
            println!(
                "Version:   {} -> {} (not newer)",
                plan.device_revision, package
            );
        }
        (Some(package), None) => {
            println!(
                "Version:   {} -> {} (not comparable)",
                plan.device_revision, package
            );
        }
        (None, _) => {
            println!("Version:   {} (no package metadata)", plan.device_revision);
        }
    }
    println!(
        "Deadline:  {} s total, {} s SCSI timeout",
        plan.timeout.as_secs(),
        plan.command_timeout.as_secs()
    );
    match plan.free_bytes {
        Some(free) => println!("Free:      {} bytes", free),
        None => println!("Free:      unknown"),
    }
    if !plan.warnings.is_empty() {
        println!("Warnings:");
        for warning in &plan.warnings {
            println!("  - {warning}");
        }
    }
}

fn print_flash_outcome(outcome: &FlashOutcome) {
    println!();
    println!("Image copied:    {}", yes_no(outcome.copied));
    println!("Update started:  {}", yes_no(outcome.started));
    println!("Elapsed:         {:.1} s", outcome.elapsed.as_secs_f64());
    if outcome.removed_image {
        println!("Image removed:   yes");
    }
    if let Some(verified) = outcome.verified {
        if verified {
            println!("Verified:        yes");
        } else {
            println!("Verified:        no (device did not report the package version)");
        }
    }
}

fn print_package_human(package: &UpdatePackage) {
    if let Some(source) = &package.source {
        println!("Source:     {}", source.display());
    }
    println!("Product:    {}", package.product);
    if let Some(category) = &package.category {
        println!("Category:   {category}");
    }
    println!("Version:    {}", package.version);
    println!("Image:      {}", package.write_path);
    if let Some(timer) = package.timer {
        println!(
            "Timer:      {} s ({})",
            timer.as_secs(),
            format_duration(timer)
        );
    }
    if let Some(timeout) = package.timeout {
        println!(
            "TimeOut:    {} s ({})",
            timeout.as_secs(),
            format_duration(timeout)
        );
    }
    println!(
        "Disclaimer: {}",
        if package.disclaimer_box { "yes" } else { "no" }
    );
    if !package.devices.is_empty() {
        println!("Devices:");
        for device in &package.devices {
            let product = device.product_info.as_deref().unwrap_or("?");
            match &device.sub_identification {
                Some(sub) => println!("  - {product} ({sub})"),
                None => println!("  - {product}"),
            }
        }
    }
}

fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    format!("{}:{:02}", total / 60, total % 60)
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

fn probe_json(probe: &Probe) -> Json {
    Json::object([
        (
            "scsi_path",
            Json::str(probe.scsi_path.display().to_string()),
        ),
        (
            "mount",
            Json::opt_str(probe.mount_root.as_ref().map(|p| p.display().to_string())),
        ),
        (
            "inquiry",
            Json::object([
                ("vendor", Json::str(probe.inquiry.vendor.as_str())),
                ("product", Json::str(probe.inquiry.product.as_str())),
                ("revision", Json::str(probe.inquiry.revision.as_str())),
                (
                    "vendor_raw",
                    Json::str(crate::scsi::hex(&probe.inquiry.vendor_raw)),
                ),
                (
                    "product_raw",
                    Json::str(crate::scsi::hex(&probe.inquiry.product_raw)),
                ),
                (
                    "revision_raw",
                    Json::str(crate::scsi::hex(&probe.inquiry.revision_raw)),
                ),
            ]),
        ),
        (
            "dev_info",
            Json::object([
                ("product_id", Json::str(probe.dev_info.product_id.as_str())),
                (
                    "product_revision",
                    Json::str(probe.dev_info.product_revision.as_str()),
                ),
                ("fw_id", Json::str(probe.dev_info.fw_id_hex())),
                ("hw_id", Json::str(probe.dev_info.hw_id_hex())),
                ("fw_id_raw", Json::UInt(probe.dev_info.fw_id as u64)),
                ("hw_id_raw", Json::UInt(probe.dev_info.hw_id as u64)),
            ]),
        ),
    ])
}

fn flash_plan_json(plan: &FlashPlan) -> Json {
    Json::object([
        ("device", Json::str(plan.device_path.display().to_string())),
        ("device_product", Json::str(plan.device_product.as_str())),
        (
            "device_product_id",
            Json::str(plan.device_product_id.as_str()),
        ),
        ("device_revision", Json::str(plan.device_revision.as_str())),
        ("mount", Json::str(plan.mount_root.display().to_string())),
        ("image", Json::str(plan.image.display().to_string())),
        ("image_size", Json::UInt(plan.image_size)),
        ("image_sha256", Json::str(plan.image_sha256.as_str())),
        (
            "destination",
            Json::str(plan.destination.display().to_string()),
        ),
        (
            "package_version",
            Json::opt_str(plan.package_version.as_deref()),
        ),
        ("version_newer", optional_bool(plan.version_newer)),
        ("product_matches", optional_bool(plan.product_matches)),
        ("timeout_seconds", Json::UInt(plan.timeout.as_secs())),
        (
            "command_timeout_seconds",
            Json::UInt(plan.command_timeout.as_secs()),
        ),
        ("free_bytes", optional_uint(plan.free_bytes)),
        (
            "warnings",
            Json::array(plan.warnings.iter().map(|w| Json::str(w.as_str()))),
        ),
    ])
}

fn flash_result_json(outcome: &FlashOutcome) -> Json {
    Json::object([
        ("copied", Json::Bool(outcome.copied)),
        ("started", Json::Bool(outcome.started)),
        ("elapsed_seconds", Json::UInt(outcome.elapsed.as_secs())),
        ("removed_image", Json::Bool(outcome.removed_image)),
        ("verified", optional_bool(outcome.verified)),
    ])
}

fn pull_outcome_json(outcome: &PullOutcome) -> Json {
    Json::object([
        (
            "source",
            Json::str(outcome.plan.source.display().to_string()),
        ),
        (
            "output",
            Json::str(outcome.plan.output.display().to_string()),
        ),
        (
            "device",
            Json::str(outcome.plan.device_path.display().to_string()),
        ),
        (
            "device_product",
            Json::str(outcome.plan.device_product.as_str()),
        ),
        (
            "device_revision",
            Json::str(outcome.plan.device_revision.as_str()),
        ),
        ("size", Json::UInt(outcome.bytes_written)),
        ("sha256", Json::str(outcome.sha256.as_str())),
        ("verified", optional_bool(outcome.verified)),
        (
            "package_version",
            Json::opt_str(
                outcome
                    .plan
                    .package
                    .as_ref()
                    .map(|package| package.version.as_str()),
            ),
        ),
    ])
}

fn package_json(package: &UpdatePackage) -> Json {
    Json::object([
        (
            "source",
            Json::opt_str(package.source.as_ref().map(|p| p.display().to_string())),
        ),
        ("category", Json::opt_str(package.category.as_deref())),
        ("product", Json::str(package.product.as_str())),
        ("version", Json::str(package.version.as_str())),
        ("write_path", Json::str(package.write_path.as_str())),
        ("timer_seconds", optional_duration(package.timer)),
        ("timeout_seconds", optional_duration(package.timeout)),
        ("disclaimer_box", Json::Bool(package.disclaimer_box)),
        (
            "devices",
            Json::array(package.devices.iter().map(|device| {
                Json::object([
                    (
                        "product_info",
                        Json::opt_str(device.product_info.as_deref()),
                    ),
                    (
                        "sub_identification",
                        Json::opt_str(device.sub_identification.as_deref()),
                    ),
                ])
            })),
        ),
    ])
}

fn optional_bool(value: Option<bool>) -> Json {
    match value {
        Some(value) => Json::Bool(value),
        None => Json::Null,
    }
}

fn optional_uint(value: Option<u64>) -> Json {
    match value {
        Some(value) => Json::UInt(value),
        None => Json::Null,
    }
}

fn optional_duration(value: Option<Duration>) -> Json {
    optional_uint(value.map(|duration| duration.as_secs()))
}

/// Progress display on stderr.
///
/// On a terminal the line is rewritten in place; when stderr is redirected a
/// new line is printed every ten percent so logs stay readable.
struct StatusLine {
    interactive: bool,
    printed: bool,
    last_stage: &'static str,
    last_percent: Option<u8>,
}

impl StatusLine {
    fn new() -> Self {
        Self {
            interactive: io::stderr().is_terminal(),
            printed: false,
            last_stage: "",
            last_percent: None,
        }
    }

    fn report(&mut self, stage: &'static str, percent: u8) {
        if self.interactive {
            eprint!("\r{stage:>8} {percent:3}%");
            io::stderr().flush().ok();
            self.printed = true;
        } else {
            let due = self.last_stage != stage
                || self
                    .last_percent
                    .is_none_or(|last| percent >= last.saturating_add(10) || percent == 100);
            if due {
                eprintln!("{stage:>8} {percent:3}%");
                self.last_stage = stage;
                self.last_percent = Some(percent);
            }
        }
    }

    fn finish(&mut self) {
        if self.printed {
            eprintln!();
            self.printed = false;
        }
    }
}

fn print_help(topic: Option<Command>) {
    match topic {
        Some(Command::Devices) => println!(
            "\
Usage: pcm-a10-fw devices [options]

List supported Sony recorders.  Each candidate SCSI generic device is
probed with INQUIRY, TEST UNIT READY and REQUEST DEVINFO.

Options:
  -d, --device <path>   Probe one path instead of enumerating
      --mount <dir>     Override the mass-storage mount root
      --json            Emit JSON on stdout
  -v, --verbose         Explain skipped devices"
        ),
        Some(Command::Info) => println!(
            "\
Usage: pcm-a10-fw info [options]

Show the INQUIRY identity and the Sony DEVINFO fields (model, firmware
revision, FWID, HWID) for the connected recorder.

Options:
  -d, --device <path>   SCSI generic path (/dev/sgN) or Windows drive
      --mount <dir>     Override the mass-storage mount root
      --json            Emit JSON on stdout
  -v, --verbose         Explain skipped devices"
        ),
        Some(Command::Flash) => println!(
            "\
Usage: pcm-a10-fw flash [options]

Copy the firmware image to the root of the recorder's mass-storage
volume, then send the Sony START FW UPDATE vendor command
(FC 00 04 64 62 6D 6E ...).  This is the sequence implemented by
WmFwUpdater.dll.

Options:
      --image <file>    Firmware image (default: UPG.IMG)
      --xml <file>      SWUpdate.xml (default: SWUpdate.xml beside the image)
      --timeout <dur>   Overall update deadline (mm:ss, h:mm:ss or seconds)
      --timer <dur>     SCSI command timeout (default: min(Timer, TimeOut))
      --verify          Re-probe and compare the reported version afterwards
      --strict-image    Reject images without the NuttX magic at offset 8
      --remove-image    Delete UPG.IMG from the device after success
      --dry-run         Validate and print the plan without writing
      --force           Flash even if versions match or the product differs
  -d, --device <path>   SCSI generic path (/dev/sgN) or Windows drive
      --mount <dir>     Override the mass-storage mount root
  -y, --yes             Do not ask for confirmation
      --json            Emit JSON on stdout (requires --yes)
  -v, --verbose         Explain skipped devices"
        ),
        Some(Command::Pull) => println!(
            "\
Usage: pcm-a10-fw pull [options]

Extension: the shipped DLL has no firmware-read command.  This copies the
UPG.IMG file from the recorder's mass-storage volume to the host; it
cannot dump the recorder's internal flash.

Options:
  -o, --output <file>   Destination (default: UPG.IMG)
      --sha256 <hex>    Verify the pulled image against this digest
      --force           Overwrite an existing destination
  -d, --device <path>   SCSI generic path (/dev/sgN) or Windows drive
      --mount <dir>     Override the mass-storage mount root
      --json            Emit JSON on stdout
  -v, --verbose         Explain skipped devices"
        ),
        Some(Command::PackageInfo) => println!(
            "\
Usage: pcm-a10-fw package-info [path] [options]

Show the metadata in SWUpdate.xml.  `path` may be the XML file itself or
a directory containing it.

Options:
      --xml <file>      SWUpdate.xml path
      --json            Emit JSON on stdout"
        ),
        Some(Command::Help) | Some(Command::Version) | None => println!(
            "\
pcm-a10-fw - Sony PCM-A10 firmware updater (MSC/SCSI)

Usage:
  pcm-a10-fw <command> [options]

Commands:
  devices        List supported Sony recorders (alias: list)
  info           Show INQUIRY and DEVINFO for the connected recorder
  flash          Copy UPG.IMG and send START FW UPDATE (alias: update)
  pull           Copy UPG.IMG from the recorder to the host (extension)
  package-info   Show SWUpdate.xml metadata (alias: xml)
  help           Show this help or help for one command
  version        Show the crate version

Global options:
  -d, --device <path>   SCSI generic path (/dev/sgN) or Windows drive
      --mount <dir>     Mass-storage mount root (override autodetection)
      --json            Emit machine-readable JSON on stdout
  -v, --verbose         Explain skipped devices
  -h, --help            Show help
  -V, --version         Show version

Run `pcm-a10-fw help <command>` for command-specific options."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, Error> {
        Cli::parse(args.iter().map(OsString::from).collect())
    }

    #[test]
    fn parses_flash_options() {
        let cli = parse(&[
            "flash",
            "--image",
            "fw.img",
            "--timeout=9:00",
            "--timer",
            "10:00",
            "--verify",
            "--remove-image",
            "--yes",
        ])
        .unwrap();
        assert_eq!(cli.command, Command::Flash);
        assert_eq!(cli.image, Some(PathBuf::from("fw.img")));
        assert_eq!(cli.timeout, Some(Duration::from_secs(540)));
        assert_eq!(cli.timer, Some(Duration::from_secs(600)));
        assert!(cli.verify);
        assert!(cli.remove_image);
        assert!(cli.yes);
    }

    #[test]
    fn aliases_map_to_commands() {
        assert_eq!(parse(&["list"]).unwrap().command, Command::Devices);
        assert_eq!(parse(&["update"]).unwrap().command, Command::Flash);
        assert_eq!(parse(&["read"]).unwrap().command, Command::Pull);
        assert_eq!(parse(&["xml"]).unwrap().command, Command::PackageInfo);
    }

    #[test]
    fn help_wins_over_command() {
        let cli = parse(&["flash", "--help"]).unwrap();
        assert_eq!(cli.command, Command::Help);
        assert_eq!(cli.help_topic, Some(Command::Flash));
    }

    #[test]
    fn rejects_unknown_options_and_missing_values() {
        assert!(parse(&["info", "--nope"]).is_err());
        assert!(parse(&["flash", "--image"]).is_err());
        assert!(parse(&["flash", "--image=x", "--yes=true"]).is_err());
        assert!(parse(&["nope"]).is_err());
    }

    #[test]
    fn parses_short_options() {
        let cli = parse(&["pull", "-o", "out.img", "-d", "/dev/sg2", "-y"]).unwrap();
        assert_eq!(cli.output, Some(PathBuf::from("out.img")));
        assert_eq!(cli.device, Some(PathBuf::from("/dev/sg2")));
        assert!(cli.yes);
    }

    #[test]
    fn duration_errors_are_usage_errors() {
        assert!(matches!(
            parse(&["flash", "--timeout", "nope"]),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn double_dash_collects_positionals() {
        let cli = parse(&["package-info", "--", "-weird-name.xml"]).unwrap();
        assert_eq!(cli.command, Command::PackageInfo);
        assert_eq!(cli.positional, vec!["-weird-name.xml"]);
    }
}
