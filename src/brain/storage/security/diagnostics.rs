#![cfg(test)]

use std::cell::RefCell;
use std::ffi::CStr;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(target_os = "linux")]
use std::ffi::OsStr;
#[cfg(target_os = "linux")]
use std::io::Read;
#[cfg(target_os = "linux")]
use std::path::Path;
#[cfg(target_os = "linux")]
use std::time::Instant;

use super::{EntryMetadata, SecureDatabaseDirectory, metadata_at};

const DEFAULT_REPORT_PARENT: &str = "/tmp";
const DIAGNOSTIC_PREFIX: &str = "cbrain-staging-sidecar-diagnostic-";
const REPORT_NAME: &str = "report.txt";
const REPORT_SCHEMA_VERSION: u32 = 1;
static REPORT_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static CAPTURE_TOKEN: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
struct CaptureConfig {
    parent: PathBuf,
    descriptor_limit: usize,
    deadline: Duration,
    after_read_dir: Option<fn(&SecureDatabaseDirectory)>,
    diagnostic_sink: Option<Rc<RefCell<Box<dyn Write>>>>,
}

struct CaptureFrame {
    token: u64,
    config: CaptureConfig,
    report: Option<PathBuf>,
}

thread_local! {
    static CAPTURE: RefCell<Vec<CaptureFrame>> = const { RefCell::new(Vec::new()) };
}

pub(crate) struct SidecarDiagnosticGuard {
    token: u64,
}

impl SidecarDiagnosticGuard {
    pub(crate) fn arm() -> Self {
        Self::arm_with(CaptureConfig {
            parent: PathBuf::from(DEFAULT_REPORT_PARENT),
            descriptor_limit: 4_096,
            deadline: Duration::from_secs(1),
            after_read_dir: None,
            diagnostic_sink: None,
        })
    }

    pub(crate) fn arm_at(parent: PathBuf) -> Self {
        Self::arm_with(CaptureConfig {
            parent,
            descriptor_limit: 4_096,
            deadline: Duration::from_secs(1),
            after_read_dir: None,
            diagnostic_sink: None,
        })
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn arm_with_limits(
        parent: PathBuf,
        descriptor_limit: usize,
        deadline: Duration,
    ) -> Self {
        Self::arm_with(CaptureConfig {
            parent,
            descriptor_limit,
            deadline,
            after_read_dir: None,
            diagnostic_sink: None,
        })
    }

    fn arm_with(config: CaptureConfig) -> Self {
        let token = CAPTURE_TOKEN.fetch_add(1, Ordering::Relaxed);
        CAPTURE.with(|stack| {
            stack.borrow_mut().push(CaptureFrame {
                token,
                config,
                report: None,
            });
        });
        Self { token }
    }

    pub(crate) fn take_report(&self) -> Option<PathBuf> {
        CAPTURE.with(|stack| {
            stack
                .borrow_mut()
                .iter_mut()
                .find(|frame| frame.token == self.token)
                .and_then(|frame| frame.report.take())
        })
    }
}

impl Drop for SidecarDiagnosticGuard {
    fn drop(&mut self) {
        CAPTURE.with(|stack| {
            let mut stack = stack.borrow_mut();
            if let Some(index) = stack.iter().position(|frame| frame.token == self.token) {
                stack.remove(index);
            }
        });
    }
}

pub(super) fn capture_sidecar_rejection(
    directory: &SecureDatabaseDirectory,
    database_name: &CStr,
    sidecar_name: &CStr,
    rejected: EntryMetadata,
) {
    let armed = CAPTURE.with(|stack| {
        stack
            .borrow()
            .last()
            .map(|frame| (frame.token, frame.config.clone()))
    });
    let Some((token, config)) = armed else {
        return;
    };
    match capture_inner(&config, directory, database_name, sidecar_name, rejected) {
        Ok(report) => {
            CAPTURE.with(|stack| {
                if let Some(frame) = stack
                    .borrow_mut()
                    .iter_mut()
                    .find(|frame| frame.token == token)
                {
                    frame.report = Some(report.clone());
                }
            });
            write_diagnostic(
                &config,
                format_args!("cbrain staging sidecar diagnostic: {}", report.display()),
            );
        }
        Err(error) => write_diagnostic(
            &config,
            format_args!("cbrain staging sidecar diagnostic capture failed: {error}"),
        ),
    }
}

fn write_diagnostic(config: &CaptureConfig, message: std::fmt::Arguments<'_>) {
    if let Some(sink) = &config.diagnostic_sink {
        let _ = writeln!(&mut **sink.borrow_mut(), "{message}");
    } else {
        let stderr = io::stderr();
        let _ = writeln!(stderr.lock(), "{message}");
    }
}

fn capture_inner(
    config: &CaptureConfig,
    directory: &SecureDatabaseDirectory,
    database_name: &CStr,
    sidecar_name: &CStr,
    rejected: EntryMetadata,
) -> io::Result<PathBuf> {
    let temporary = tempfile::Builder::new()
        .prefix(DIAGNOSTIC_PREFIX)
        .tempdir_in(&config.parent)?;
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))?;
    verify_mode(temporary.path().metadata()?.permissions().mode(), 0o700)?;

    let sequence = REPORT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let scan_started = timestamp_nanos();
    let rejected_gid = diagnostic_gid_at(&directory.descriptor, sidecar_name, rejected);
    let mut entries = fs::read_dir(directory.path())?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by(|left, right| {
        left.file_name()
            .as_bytes()
            .cmp(right.file_name().as_bytes())
    });
    if let Some(after_read_dir) = config.after_read_dir {
        after_read_dir(directory);
    }

    let mut report = String::new();
    writeln!(report, "report_schema_version={REPORT_SCHEMA_VERSION}").unwrap();
    writeln!(report, "pid={}", std::process::id()).unwrap();
    writeln!(report, "sequence={sequence}").unwrap();
    writeln!(
        report,
        "database_name_hex={}",
        hex(database_name.to_bytes())
    )
    .unwrap();
    writeln!(report, "sidecar_name_hex={}", hex(sidecar_name.to_bytes())).unwrap();
    write_metadata(&mut report, "rejected", rejected, rejected_gid);
    writeln!(
        report,
        "directory_path_hex={}",
        hex(directory.path().as_os_str().as_bytes())
    )
    .unwrap();
    writeln!(report, "scan_started_unix_nanos={scan_started}").unwrap();
    for entry in entries {
        let name = entry.file_name();
        writeln!(report, "entry_name_hex={}", hex(name.as_bytes())).unwrap();
        match entry.path().symlink_metadata() {
            Ok(metadata) => write_metadata(
                &mut report,
                "entry",
                EntryMetadata::from(&metadata),
                Some(metadata.gid()),
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                writeln!(report, "entry_metadata=unavailable:not_found").unwrap();
            }
            Err(error) => {
                writeln!(report, "entry_metadata=unavailable:{:?}", error.kind()).unwrap();
            }
        }
    }
    writeln!(report, "scan_ended_unix_nanos={}", timestamp_nanos()).unwrap();
    let pathname_identity = match metadata_at(&directory.descriptor, sidecar_name) {
        Ok(later) if later == rejected => "same",
        Ok(_) => "replaced",
        Err(error) if error.kind() == io::ErrorKind::NotFound => "missing",
        Err(error) => return Err(error),
    };
    writeln!(report, "pathname_identity={pathname_identity}").unwrap();
    #[cfg(target_os = "linux")]
    write_linux_descriptor_evidence(
        &mut report,
        directory.path(),
        rejected,
        config.descriptor_limit,
        config.deadline,
    );
    #[cfg(not(target_os = "linux"))]
    writeln!(report, "descriptor_scan=unsupported").unwrap();
    writeln!(report, "descriptor_limit={}", config.descriptor_limit).unwrap();
    writeln!(
        report,
        "descriptor_deadline_millis={}",
        config.deadline.as_millis()
    )
    .unwrap();

    let report_path = temporary.path().join(REPORT_NAME);
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&report_path)?;
    output.write_all(report.as_bytes())?;
    output.sync_all()?;
    verify_mode(output.metadata()?.permissions().mode(), 0o600)?;
    File::open(temporary.path())?.sync_all()?;
    let retained_directory = temporary.keep();
    File::open(&config.parent)?.sync_all()?;
    Ok(retained_directory.join(REPORT_NAME))
}

#[cfg(target_os = "linux")]
#[derive(Debug, PartialEq, Eq)]
enum LinuxStatParseError {
    Newline,
    MissingPid,
    MissingOpeningParenthesis,
    MissingClosingParenthesis,
    InvalidState,
    MissingFields,
    InvalidField,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LinuxStat {
    ppid: u32,
    start_time: u64,
}

#[cfg(target_os = "linux")]
fn parse_linux_stat(record: &str) -> Result<LinuxStat, LinuxStatParseError> {
    if record.contains(['\n', '\r']) {
        return Err(LinuxStatParseError::Newline);
    }
    let opening = record
        .find(" (")
        .ok_or(LinuxStatParseError::MissingOpeningParenthesis)?;
    record[..opening]
        .parse::<u32>()
        .map_err(|_| LinuxStatParseError::MissingPid)?;
    let closing = record
        .rfind(')')
        .filter(|closing| *closing > opening + 1)
        .ok_or(LinuxStatParseError::MissingClosingParenthesis)?;
    let suffix = record
        .get(closing + 1..)
        .and_then(|suffix| suffix.strip_prefix(' '))
        .ok_or(LinuxStatParseError::MissingClosingParenthesis)?;
    let fields = suffix.split_ascii_whitespace().collect::<Vec<_>>();
    if fields.len() < 20 {
        return Err(LinuxStatParseError::MissingFields);
    }
    let state = fields[0].as_bytes();
    if state.len() != 1
        || !matches!(
            state[0],
            b'R' | b'S' | b'D' | b'Z' | b'T' | b't' | b'X' | b'x' | b'K' | b'W' | b'P' | b'I'
        )
    {
        return Err(LinuxStatParseError::InvalidState);
    }
    let ppid = fields[1]
        .parse::<u32>()
        .map_err(|_| LinuxStatParseError::InvalidField)?;
    for field in &fields[2..=5] {
        field
            .parse::<i32>()
            .map_err(|_| LinuxStatParseError::InvalidField)?;
    }
    fields[6]
        .parse::<u32>()
        .map_err(|_| LinuxStatParseError::InvalidField)?;
    for field in &fields[7..=12] {
        field
            .parse::<u64>()
            .map_err(|_| LinuxStatParseError::InvalidField)?;
    }
    for field in &fields[13..=18] {
        field
            .parse::<i64>()
            .map_err(|_| LinuxStatParseError::InvalidField)?;
    }
    let start_time = fields[19]
        .parse::<u64>()
        .map_err(|_| LinuxStatParseError::InvalidField)?;
    Ok(LinuxStat { ppid, start_time })
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScanStop {
    Limit,
    Deadline,
    OperationOverrun,
}

#[cfg(target_os = "linux")]
struct ScanBudget {
    started: Instant,
    deadline: Duration,
    remaining_links: usize,
    delay: Option<BudgetDelay>,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BudgetHookPoint {
    DirectoryAdvance,
    AfterDirectoryAdvance,
    Sort,
}

#[cfg(target_os = "linux")]
struct BudgetDelay {
    point: BudgetHookPoint,
    duration: Duration,
    consumed: bool,
}

#[cfg(target_os = "linux")]
impl ScanBudget {
    fn new(deadline: Duration, remaining_links: usize) -> Self {
        Self {
            started: Instant::now(),
            deadline,
            remaining_links,
            delay: None,
        }
    }

    fn new_with_delay(
        deadline: Duration,
        remaining_links: usize,
        point: BudgetHookPoint,
        duration: Duration,
    ) -> Self {
        Self {
            delay: Some(BudgetDelay {
                point,
                duration,
                consumed: false,
            }),
            ..Self::new(deadline, remaining_links)
        }
    }

    fn delay_at(&mut self, point: BudgetHookPoint) {
        let Some(delay) = &mut self.delay else {
            return;
        };
        if delay.point == point && !delay.consumed {
            delay.consumed = true;
            std::thread::sleep(delay.duration);
        }
    }

    fn before_operation(&self) -> Result<(), ScanStop> {
        if self.started.elapsed() >= self.deadline {
            Err(ScanStop::Deadline)
        } else {
            Ok(())
        }
    }

    fn after_operation(&self) -> Result<(), ScanStop> {
        if self.started.elapsed() >= self.deadline {
            Err(ScanStop::OperationOverrun)
        } else {
            Ok(())
        }
    }

    fn charge(&mut self) -> Result<(), ScanStop> {
        self.before_operation()?;
        if self.remaining_links == 0 {
            return Err(ScanStop::Limit);
        }
        self.remaining_links -= 1;
        Ok(())
    }

    fn operation<T>(
        &mut self,
        operation: impl FnOnce() -> io::Result<T>,
    ) -> Result<io::Result<T>, ScanStop> {
        self.before_operation()?;
        let result = operation();
        self.after_operation()?;
        Ok(result)
    }
}

#[cfg(target_os = "linux")]
struct DescriptorOwner {
    pid: u32,
    ppid: u32,
    start_time_before: u64,
    start_time_after: u64,
    executable: PathBuf,
    descriptor: u32,
    target: PathBuf,
    deleted: bool,
    dev: u64,
    ino: u64,
    fdinfo: Vec<(&'static str, Vec<u8>)>,
}

#[cfg(target_os = "linux")]
fn write_linux_descriptor_evidence(
    report: &mut String,
    directory: &Path,
    rejected: EntryMetadata,
    descriptor_limit: usize,
    deadline: Duration,
) {
    let mut budget = ScanBudget::new(deadline, descriptor_limit);
    let mut owners = Vec::new();
    let result = scan_linux_descriptors(directory, rejected, &mut budget, &mut owners);
    for owner in owners {
        writeln!(report, "owner_pid={}", owner.pid).unwrap();
        writeln!(report, "owner_ppid={}", owner.ppid).unwrap();
        writeln!(
            report,
            "owner_start_time_before={}",
            owner.start_time_before
        )
        .unwrap();
        writeln!(report, "owner_start_time_after={}", owner.start_time_after).unwrap();
        writeln!(report, "process_identity=stable").unwrap();
        writeln!(
            report,
            "owner_executable_hex={}",
            hex(owner.executable.as_os_str().as_bytes())
        )
        .unwrap();
        writeln!(report, "owner_descriptor={}", owner.descriptor).unwrap();
        writeln!(
            report,
            "owner_target_hex={}",
            hex(owner.target.as_os_str().as_bytes())
        )
        .unwrap();
        writeln!(report, "owner_deleted={}", owner.deleted).unwrap();
        writeln!(report, "owner_dev={}", owner.dev).unwrap();
        writeln!(report, "owner_ino={}", owner.ino).unwrap();
        for (key, value) in owner.fdinfo {
            writeln!(report, "owner_fdinfo_{key}_hex={}", hex(&value)).unwrap();
        }
    }
    write_descriptor_scan_result(report, result);
}

#[cfg(target_os = "linux")]
fn write_descriptor_scan_result(report: &mut String, result: Result<(), ScanStop>) {
    match result {
        Ok(()) => writeln!(report, "descriptor_scan=complete").unwrap(),
        Err(ScanStop::Limit) => writeln!(report, "descriptor_scan=truncated limit").unwrap(),
        Err(ScanStop::Deadline) => writeln!(report, "descriptor_scan=truncated deadline").unwrap(),
        Err(ScanStop::OperationOverrun) => {
            writeln!(report, "descriptor_scan=truncated operation-overrun").unwrap()
        }
    }
}

#[cfg(target_os = "linux")]
fn scan_linux_descriptors(
    directory: &Path,
    rejected: EntryMetadata,
    budget: &mut ScanBudget,
    owners: &mut Vec<DescriptorOwner>,
) -> Result<(), ScanStop> {
    scan_linux_descriptors_at(
        Path::new("/proc"),
        directory,
        rejected,
        budget,
        owners,
        None,
    )
}

#[cfg(target_os = "linux")]
fn scan_linux_descriptors_at(
    proc_root: &Path,
    directory: &Path,
    rejected: EntryMetadata,
    budget: &mut ScanBudget,
    owners: &mut Vec<DescriptorOwner>,
    before_final_stat: Option<&dyn Fn(&Path)>,
) -> Result<(), ScanStop> {
    if budget.remaining_links == 0 {
        return Err(ScanStop::Limit);
    }
    let pids = match numeric_directory_entries(proc_root, budget)? {
        Ok(pids) => pids,
        Err(_) => return Ok(()),
    };
    let effective_uid = unsafe { libc::geteuid() };
    for pid in pids {
        let process = proc_root.join(pid.to_string());
        match budget.operation(|| process.metadata())? {
            Ok(metadata) if metadata.uid() == effective_uid => {}
            Ok(_) | Err(_) => continue,
        }
        let initial = match read_linux_stat_checked(&process.join("stat"), budget)? {
            Some(stat) => stat,
            None => continue,
        };
        let descriptors = match numeric_directory_entries(&process.join("fd"), budget)? {
            Ok(descriptors) => descriptors,
            Err(_) => continue,
        };

        for descriptor in descriptors {
            budget.charge()?;
            let fd_path = process.join("fd").join(descriptor.to_string());
            let target = match budget.operation(|| fs::read_link(&fd_path))? {
                Ok(target) => target,
                Err(_) => continue,
            };
            let (comparison_target, deleted) = strip_deleted_suffix(&target);
            if !comparison_target.starts_with(directory) {
                continue;
            }
            let metadata = match budget.operation(|| fd_path.metadata())? {
                Ok(metadata)
                    if metadata.dev() == rejected.dev && metadata.ino() == rejected.ino =>
                {
                    metadata
                }
                Ok(_) | Err(_) => continue,
            };
            let fdinfo = match budget.operation(|| {
                read_selected_fdinfo(&process.join("fdinfo").join(descriptor.to_string()))
            })? {
                Ok(fdinfo) => fdinfo,
                Err(_) => continue,
            };
            let executable = match budget.operation(|| fs::read_link(process.join("exe")))? {
                Ok(executable) => executable,
                Err(_) => continue,
            };
            if let Some(before_final_stat) = before_final_stat {
                before_final_stat(&process);
            }
            let later = match read_linux_stat_checked(&process.join("stat"), budget)? {
                Some(stat) if stat.start_time == initial.start_time => stat,
                Some(_) | None => continue,
            };
            owners.push(DescriptorOwner {
                pid,
                ppid: initial.ppid,
                start_time_before: initial.start_time,
                start_time_after: later.start_time,
                executable,
                descriptor,
                target,
                deleted,
                dev: metadata.dev(),
                ino: metadata.ino(),
                fdinfo,
            });
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn numeric_directory_entries(
    path: &Path,
    budget: &mut ScanBudget,
) -> Result<io::Result<Vec<u32>>, ScanStop> {
    let mut directory = match budget.operation(|| fs::read_dir(path))? {
        Ok(directory) => directory,
        Err(error) => return Ok(Err(error)),
    };
    let mut numbers = Vec::new();
    loop {
        budget.before_operation()?;
        budget.delay_at(BudgetHookPoint::DirectoryAdvance);
        let entry = directory.next();
        budget.after_operation()?;
        let Some(entry) = entry else {
            break;
        };
        if let Ok(entry) = entry
            && let Ok(name) = std::str::from_utf8(entry.file_name().as_bytes())
            && let Ok(number) = name.parse::<u32>()
        {
            numbers.push(number);
        }
        budget.delay_at(BudgetHookPoint::AfterDirectoryAdvance);
    }
    budget.before_operation()?;
    budget.delay_at(BudgetHookPoint::Sort);
    numbers.sort_unstable();
    budget.after_operation()?;
    Ok(Ok(numbers))
}

#[cfg(target_os = "linux")]
fn read_linux_stat_checked(
    path: &Path,
    budget: &mut ScanBudget,
) -> Result<Option<LinuxStat>, ScanStop> {
    let record = match budget.operation(|| fs::read_to_string(path))? {
        Ok(record) => record,
        Err(_) => return Ok(None),
    };
    let record = record.strip_suffix('\n').unwrap_or(&record);
    Ok(parse_linux_stat(record).ok())
}

#[cfg(target_os = "linux")]
fn strip_deleted_suffix(target: &Path) -> (&Path, bool) {
    const DELETED: &[u8] = b" (deleted)";
    let bytes = target.as_os_str().as_bytes();
    match bytes.strip_suffix(DELETED) {
        Some(path) => (Path::new(OsStr::from_bytes(path)), true),
        None => (target, false),
    }
}

#[cfg(target_os = "linux")]
fn read_selected_fdinfo(path: &Path) -> io::Result<Vec<(&'static str, Vec<u8>)>> {
    let mut bytes = Vec::new();
    File::open(path)?.take(4_096).read_to_end(&mut bytes)?;
    let mut selected = Vec::new();
    let complete_length = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    for line in bytes[..complete_length].split(|byte| *byte == b'\n') {
        let Some(colon) = line.iter().position(|byte| *byte == b':') else {
            continue;
        };
        let key = match &line[..colon] {
            b"pos" => "pos",
            b"flags" => "flags",
            b"mnt_id" => "mnt_id",
            b"ino" => "ino",
            _ => continue,
        };
        selected.push((key, line[colon + 1..].to_vec()));
    }
    Ok(selected)
}

#[allow(clippy::unnecessary_cast)]
fn diagnostic_gid_at(directory: &File, name: &CStr, rejected: EntryMetadata) -> Option<u32> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        return None;
    }
    let stat = unsafe { stat.assume_init() };
    let observed = EntryMetadata {
        mode: stat.st_mode as u32,
        uid: stat.st_uid as u32,
        nlink: stat.st_nlink as u64,
        dev: stat.st_dev as u64,
        ino: stat.st_ino as u64,
        size: stat.st_size as u64,
        modified_seconds: stat.st_mtime,
        modified_nanoseconds: stat.st_mtime_nsec,
    };
    (observed == rejected).then_some(stat.st_gid as u32)
}

fn write_metadata(report: &mut String, prefix: &str, metadata: EntryMetadata, gid: Option<u32>) {
    writeln!(report, "{prefix}_type={}", file_type(metadata.mode)).unwrap();
    writeln!(report, "{prefix}_mode={:#o}", metadata.mode).unwrap();
    writeln!(report, "{prefix}_uid={}", metadata.uid).unwrap();
    match gid {
        Some(gid) => writeln!(report, "{prefix}_diagnostic_gid={gid}").unwrap(),
        None => writeln!(report, "{prefix}_diagnostic_gid=unavailable").unwrap(),
    }
    writeln!(report, "{prefix}_dev={}", metadata.dev).unwrap();
    writeln!(report, "{prefix}_ino={}", metadata.ino).unwrap();
    writeln!(report, "{prefix}_nlink={}", metadata.nlink).unwrap();
    writeln!(report, "{prefix}_length={}", metadata.size).unwrap();
}

#[allow(clippy::unnecessary_cast)] // libc mode constants vary across Unix targets.
fn file_type(mode: u32) -> &'static str {
    match mode & libc::S_IFMT as u32 {
        value if value == libc::S_IFREG as u32 => "regular",
        value if value == libc::S_IFDIR as u32 => "directory",
        value if value == libc::S_IFLNK as u32 => "symlink",
        value if value == libc::S_IFIFO as u32 => "fifo",
        value if value == libc::S_IFSOCK as u32 => "socket",
        value if value == libc::S_IFCHR as u32 => "character",
        value if value == libc::S_IFBLK as u32 => "block",
        _ => "unknown",
    }
}

fn verify_mode(actual: u32, expected: u32) -> io::Result<()> {
    if actual & 0o777 == expected {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "diagnostic artifact mode was {:#o}, expected {expected:#o}",
                actual & 0o777
            ),
        ))
    }
}

fn timestamp_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[cfg(target_os = "linux")]
    use std::io::{BufRead, BufReader, Read};
    #[cfg(target_os = "linux")]
    use std::os::unix::fs::symlink;
    #[cfg(target_os = "linux")]
    use std::os::unix::process::CommandExt;
    #[cfg(target_os = "linux")]
    use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
    #[cfg(target_os = "linux")]
    use std::sync::mpsc;
    #[cfg(target_os = "linux")]
    use std::thread;
    #[cfg(target_os = "linux")]
    use std::time::Instant;

    use super::*;
    use crate::brain::storage::security::{EntryMetadata, SecureDatabaseDirectory, SecurityError};

    fn fixture() -> (tempfile::TempDir, SecureDatabaseDirectory) {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let directory = SecureDatabaseDirectory::prepare(root.path(), true).unwrap();
        let database = directory.create_database_file(c"brain.sqlite3").unwrap();
        drop(database);
        (root, directory)
    }

    #[cfg(target_os = "linux")]
    fn write_test_stat(process: &Path, pid: u32, ppid: u32, start_time: u64) {
        fs::write(
            process.join("stat"),
            format!(
                "{pid} (synthetic ) process) S {ppid} 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 {start_time} 0"
            ),
        )
        .unwrap();
    }

    #[cfg(target_os = "linux")]
    fn add_test_process(
        proc_root: &Path,
        pid: u32,
        target_paths: &[&Path],
        fdinfo: &[u8],
    ) -> PathBuf {
        let process = proc_root.join(pid.to_string());
        fs::create_dir_all(process.join("fd")).unwrap();
        fs::create_dir_all(process.join("fdinfo")).unwrap();
        write_test_stat(&process, pid, std::process::id(), 99);
        symlink(std::env::current_exe().unwrap(), process.join("exe")).unwrap();
        for (index, target) in target_paths.iter().enumerate() {
            let descriptor = (10 + index).to_string();
            symlink(target, process.join("fd").join(&descriptor)).unwrap();
            fs::write(process.join("fdinfo").join(descriptor), fdinfo).unwrap();
        }
        process
    }

    #[cfg(target_os = "linux")]
    struct ChildGuard {
        child: Child,
        stdin: Option<ChildStdin>,
    }

    #[cfg(target_os = "linux")]
    impl ChildGuard {
        fn new(mut child: Child) -> Self {
            let stdin = child.stdin.take().expect("piped child stdin");
            Self {
                child,
                stdin: Some(stdin),
            }
        }

        fn id(&self) -> u32 {
            self.child.id()
        }

        fn wait_ready(&mut self, timeout: Duration) -> io::Result<()> {
            let stdout = self.child.stdout.take().ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "piped child stdout unavailable")
            })?;
            let (sender, receiver) = mpsc::channel();
            thread::spawn(move || {
                let mut reader = BufReader::new(stdout);
                let result = loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) => break Err(io::Error::other("child exited before readiness")),
                        Ok(_) if line == "ready\n" => {
                            let _ = sender.send(Ok(()));
                            let _ = io::copy(&mut reader, &mut io::sink());
                            return;
                        }
                        Ok(_) => {}
                        Err(error) => break Err(error),
                    }
                };
                let _ = sender.send(result);
            });
            receiver.recv_timeout(timeout).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("child readiness timed out: {error}"),
                )
            })?
        }

        fn release(&mut self) {
            self.stdin.take();
        }

        fn wait_bounded(&mut self, timeout: Duration) -> io::Result<ExitStatus> {
            self.release();
            let started = Instant::now();
            loop {
                if let Some(status) = self.child.try_wait()? {
                    return Ok(status);
                }
                if started.elapsed() >= timeout {
                    self.kill_and_reap(Duration::from_secs(1))?;
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "child completion timed out",
                    ));
                }
                thread::sleep(Duration::from_millis(10));
            }
        }

        fn kill_and_reap(&mut self, timeout: Duration) -> io::Result<()> {
            self.release();
            if self.child.try_wait()?.is_some() {
                return Ok(());
            }
            self.child.kill()?;
            let started = Instant::now();
            loop {
                if self.child.try_wait()?.is_some() {
                    return Ok(());
                }
                if started.elapsed() >= timeout {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "child kill/reap timed out",
                    ));
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if let Err(error) = self.kill_and_reap(Duration::from_secs(1)) {
                eprintln!("child cleanup failed: {error}");
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn spawn_helper(name: &str, configure: impl FnOnce(&mut Command)) -> ChildGuard {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .arg(name)
            .arg("--exact")
            .arg("--ignored")
            .arg("--nocapture")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        configure(&mut command);
        ChildGuard::new(command.spawn().unwrap())
    }

    #[cfg(target_os = "linux")]
    fn process_start_time(pid: u32) -> Option<u64> {
        let record = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        parse_linux_stat(record.trim_end())
            .ok()
            .map(|stat| stat.start_time)
    }

    #[cfg(target_os = "linux")]
    fn assert_process_identity_absent(pid: u32, start_time: u64) {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(2) {
            if process_start_time(pid) != Some(start_time) {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("process identity {pid}/{start_time} survived cleanup");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_linux_stat_with_spaces_and_parentheses_in_comm() {
        let parsed =
            parse_linux_stat("42 (odd ) name) S 7 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 99 0").unwrap();
        assert_eq!(parsed.ppid, 7);
        assert_eq!(parsed.start_time, 99);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn rejects_malformed_linux_stat_records() {
        let cases = [
            ("42 odd) S 7 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 99", "opening"),
            ("42 (odd S 7 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 99", "closing"),
            ("42 (odd) ? 7 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 99", "state"),
            (
                "42 (odd) SS 7 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 99",
                "state width",
            ),
            (
                "42 (odd) é 7 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 99",
                "multi-byte state",
            ),
            ("42 (odd) S 7 0", "fields"),
            (
                "42 (odd) S 7 0 0 0 0 0 0 0 0 0\n0 0 0 0 0 0 0 0 99",
                "newline",
            ),
            (
                "42 (odd) S 4294967296 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 99",
                "ppid overflow",
            ),
            (
                "42 (odd) S 7 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 18446744073709551616",
                "start time overflow",
            ),
            (
                "42 (odd) S 7 0 0 0 0 0 nope 0 0 0 0 0 0 0 0 0 0 0 99 0",
                "intermediate field",
            ),
        ];

        for (record, label) in cases {
            assert!(parse_linux_stat(record).is_err(), "accepted {label}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn directory_enumeration_reports_deadline_before_next_advance() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        fs::create_dir(root.path().join("1")).unwrap();
        fs::create_dir(root.path().join("2")).unwrap();
        let mut budget = ScanBudget::new_with_delay(
            Duration::from_millis(5),
            usize::MAX,
            BudgetHookPoint::AfterDirectoryAdvance,
            Duration::from_millis(20),
        );

        let result = numeric_directory_entries(root.path(), &mut budget);
        assert!(matches!(result, Err(ScanStop::Deadline)));
        let mut report = String::new();
        write_descriptor_scan_result(&mut report, result.map(|_| ()));
        assert_eq!(report, "descriptor_scan=truncated deadline\n");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn directory_sort_reports_operation_overrun() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        fs::create_dir(root.path().join("2")).unwrap();
        fs::create_dir(root.path().join("1")).unwrap();
        let mut budget = ScanBudget::new_with_delay(
            Duration::from_millis(5),
            usize::MAX,
            BudgetHookPoint::Sort,
            Duration::from_millis(20),
        );

        let result = numeric_directory_entries(root.path(), &mut budget);
        assert!(matches!(result, Err(ScanStop::OperationOverrun)));
        let mut report = String::new();
        write_descriptor_scan_result(&mut report, result.map(|_| ()));
        assert_eq!(report, "descriptor_scan=truncated operation-overrun\n");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn scanner_rejects_sibling_targets_and_inode_mismatches() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let directory = root.path().join("database");
        let sibling = root.path().join("database-sibling");
        fs::create_dir(&directory).unwrap();
        fs::create_dir(&sibling).unwrap();
        let rejected_path = directory.join("brain.sqlite3-wal");
        let other_path = directory.join("other");
        let sibling_path = sibling.join("brain.sqlite3-wal");
        fs::write(&rejected_path, b"rejected").unwrap();
        fs::write(&other_path, b"other").unwrap();
        fs::write(&sibling_path, b"sibling").unwrap();
        let rejected = EntryMetadata::from(&fs::metadata(&rejected_path).unwrap());
        let proc_root = tempfile::tempdir_in("/tmp").unwrap();
        add_test_process(
            proc_root.path(),
            101,
            &[&sibling_path, &other_path],
            b"pos:\t0\nflags:\t0100000\nmnt_id:\t1\nino:\t2\n",
        );
        let mut budget = ScanBudget::new(Duration::from_secs(1), 8);
        let mut owners = Vec::new();

        assert_eq!(
            scan_linux_descriptors_at(
                proc_root.path(),
                &directory,
                rejected,
                &mut budget,
                &mut owners,
                None,
            ),
            Ok(())
        );
        assert!(owners.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn scanner_skips_owner_when_fdinfo_is_unreadable() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let directory = root.path().join("database");
        fs::create_dir(&directory).unwrap();
        let rejected_path = directory.join("brain.sqlite3-wal");
        fs::write(&rejected_path, b"rejected").unwrap();
        let rejected = EntryMetadata::from(&fs::metadata(&rejected_path).unwrap());
        let proc_root = tempfile::tempdir_in("/tmp").unwrap();
        let process = add_test_process(
            proc_root.path(),
            102,
            &[&rejected_path],
            b"pos:\t0\nflags:\t0100000\nmnt_id:\t1\nino:\t2\n",
        );
        fs::remove_file(process.join("fdinfo/10")).unwrap();
        let mut budget = ScanBudget::new(Duration::from_secs(1), 8);
        let mut owners = Vec::new();

        scan_linux_descriptors_at(
            proc_root.path(),
            &directory,
            rejected,
            &mut budget,
            &mut owners,
            None,
        )
        .unwrap();
        assert!(owners.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn scanner_skips_unstable_process_identity() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let directory = root.path().join("database");
        fs::create_dir(&directory).unwrap();
        let rejected_path = directory.join("brain.sqlite3-wal");
        fs::write(&rejected_path, b"rejected").unwrap();
        let rejected = EntryMetadata::from(&fs::metadata(&rejected_path).unwrap());
        let proc_root = tempfile::tempdir_in("/tmp").unwrap();
        let process = add_test_process(
            proc_root.path(),
            103,
            &[&rejected_path],
            b"pos:\t0\nflags:\t0100000\nmnt_id:\t1\nino:\t2\n",
        );
        let mutate = |process: &Path| write_test_stat(process, 103, std::process::id(), 100);
        let mut budget = ScanBudget::new(Duration::from_secs(1), 8);
        let mut owners = Vec::new();

        scan_linux_descriptors_at(
            proc_root.path(),
            &directory,
            rejected,
            &mut budget,
            &mut owners,
            Some(&mutate),
        )
        .unwrap();
        assert!(owners.is_empty());
        assert_eq!(
            parse_linux_stat(&fs::read_to_string(process.join("stat")).unwrap())
                .unwrap()
                .start_time,
            100
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fdinfo_is_filtered_and_capped_at_four_kibibytes() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let fdinfo = root.path().join("fdinfo");
        let mut bytes =
            b"pos:\t7\nflags:\t0100000\nsecret:\tnot-for-report\nmnt_id:\t8\nino:\t9\n".to_vec();
        bytes.resize(4_096, b'x');
        bytes.extend_from_slice(b"\nino:\tsecret-after-cap\n");
        fs::write(&fdinfo, bytes).unwrap();

        let selected = read_selected_fdinfo(&fdinfo).unwrap();
        assert_eq!(
            selected,
            vec![
                ("pos", b"\t7".to_vec()),
                ("flags", b"\t0100000".to_vec()),
                ("mnt_id", b"\t8".to_vec()),
                ("ino", b"\t9".to_vec()),
            ]
        );
        assert!(selected.iter().all(|(_, value)| value.len() < 4_096));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn exact_descriptor_limit_completes_but_one_more_truncates() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let directory = root.path().join("database");
        fs::create_dir(&directory).unwrap();
        let target = directory.join("brain.sqlite3-wal");
        fs::write(&target, b"rejected").unwrap();
        let rejected = EntryMetadata::from(&fs::metadata(&target).unwrap());

        for (targets, expected) in [
            (vec![target.as_path()], Ok(())),
            (
                vec![target.as_path(), target.as_path()],
                Err(ScanStop::Limit),
            ),
        ] {
            let proc_root = tempfile::tempdir_in("/tmp").unwrap();
            add_test_process(
                proc_root.path(),
                104,
                &targets,
                b"pos:\t0\nflags:\t0100000\nmnt_id:\t1\nino:\t2\n",
            );
            let mut budget = ScanBudget::new(Duration::from_secs(1), 1);
            let mut owners = Vec::new();
            assert_eq!(
                scan_linux_descriptors_at(
                    proc_root.path(),
                    &directory,
                    rejected,
                    &mut budget,
                    &mut owners,
                    None,
                ),
                expected
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn scanner_records_deleted_descriptor_target_losslessly() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let target = root.path().join("brain.sqlite3-wal");
        fs::write(&target, b"rejected").unwrap();
        let held = File::open(&target).unwrap();
        let rejected = EntryMetadata::from(&held.metadata().unwrap());
        fs::remove_file(&target).unwrap();
        let mut budget = ScanBudget::new(Duration::from_secs(1), 4_096);
        let mut owners = Vec::new();

        scan_linux_descriptors_at(
            Path::new("/proc"),
            root.path(),
            rejected,
            &mut budget,
            &mut owners,
            None,
        )
        .unwrap();
        let owner = owners
            .iter()
            .find(|owner| owner.pid == std::process::id())
            .expect("current process deleted descriptor owner");
        assert!(owner.deleted);
        assert_eq!(
            owner.target.as_os_str().as_bytes(),
            [target.as_os_str().as_bytes(), b" (deleted)"].concat()
        );
        drop(held);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn zero_descriptor_budget_is_reported_as_truncated() {
        let (_root, directory) = fixture();
        drop(
            directory
                .create_database_file(c"brain.sqlite3-wal")
                .unwrap(),
        );
        let report_parent = tempfile::tempdir_in("/tmp").unwrap();
        let guard = SidecarDiagnosticGuard::arm_with_limits(
            report_parent.path().to_owned(),
            0,
            Duration::from_secs(1),
        );
        assert!(matches!(
            directory.validate_database_without_sidecars(c"brain.sqlite3"),
            Err(SecurityError::Invalid("staging SQLite sidecar remains"))
        ));
        let report = guard.take_report().unwrap();
        assert!(
            fs::read_to_string(&report)
                .unwrap()
                .contains("descriptor_scan=truncated limit")
        );
        fs::remove_dir_all(report.parent().unwrap()).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn readiness_timeout_has_bounded_kill_and_reap_cleanup() {
        const HELPER: &str =
            "brain::storage::security::diagnostics::tests::sidecar_descriptor_holder_helper";
        let mut child = spawn_helper(HELPER, |command| {
            command.env("CBRAIN_TEST_NO_READY", "1");
        });
        let pid = child.id();
        let start_time = process_start_time(pid).unwrap();

        let error = child.wait_ready(Duration::from_millis(50)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        child.kill_and_reap(Duration::from_secs(1)).unwrap();
        assert_process_identity_absent(pid, start_time);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_timeout_has_bounded_kill_and_reap_cleanup() {
        const HELPER: &str =
            "brain::storage::security::diagnostics::tests::sidecar_descriptor_holder_helper";
        let mut child = spawn_helper(HELPER, |command| {
            command.env("CBRAIN_TEST_HANG_AFTER_EOF", "1");
        });
        let pid = child.id();
        let start_time = process_start_time(pid).unwrap();
        child.wait_ready(Duration::from_secs(1)).unwrap();

        let error = child.wait_bounded(Duration::from_millis(50)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_process_identity_absent(pid, start_time);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unwind_drop_has_bounded_kill_and_reap_cleanup() {
        const HELPER: &str =
            "brain::storage::security::diagnostics::tests::sidecar_descriptor_holder_helper";
        let child = spawn_helper(HELPER, |_| {});
        let pid = child.id();
        let start_time = process_start_time(pid).unwrap();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _child = child;
            panic!("exercise child guard unwind cleanup");
        }));
        assert!(result.is_err());
        assert_process_identity_absent(pid, start_time);
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "subprocess helper"]
    fn sidecar_descriptor_holder_helper() {
        let _held = std::env::var_os("CBRAIN_TEST_SIDECAR_HOLDER")
            .map(File::open)
            .transpose()
            .unwrap();
        if std::env::var_os("CBRAIN_TEST_NO_READY").is_none() {
            println!("ready");
            io::stdout().flush().unwrap();
        }
        let mut byte = [0_u8; 1];
        let _ = io::stdin().read(&mut byte);
        if std::env::var_os("CBRAIN_TEST_HANG_AFTER_EOF").is_some() {
            loop {
                thread::sleep(Duration::from_secs(60));
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn report_identifies_child_descriptor_holder() {
        const HOLDER: &str =
            "brain::storage::security::diagnostics::tests::sidecar_descriptor_holder_helper";
        const SENTINEL: &str = "not-for-report";

        let (_root, directory) = fixture();
        let sidecar_path = directory.path().join("brain.sqlite3-wal");
        drop(
            directory
                .create_database_file(c"brain.sqlite3-wal")
                .unwrap(),
        );
        let rejected = fs::metadata(&sidecar_path).unwrap();
        let mut holder = spawn_helper(HOLDER, |command| {
            command.env("CBRAIN_TEST_SIDECAR_HOLDER", &sidecar_path);
        });
        let holder_pid = holder.id();
        holder.wait_ready(Duration::from_secs(5)).unwrap();
        let mut unrelated = spawn_helper(HOLDER, |command| {
            command
                .arg0(SENTINEL)
                .env("CBRAIN_SECRET_SENTINEL", SENTINEL);
        });
        unrelated.wait_ready(Duration::from_secs(5)).unwrap();
        let unrelated_pid = unrelated.id();

        let report_parent = tempfile::tempdir_in("/tmp").unwrap();
        let guard = SidecarDiagnosticGuard::arm_at(report_parent.path().to_owned());
        assert!(matches!(
            directory.validate_database_without_sidecars(c"brain.sqlite3"),
            Err(SecurityError::Invalid("staging SQLite sidecar remains"))
        ));
        let report = guard.take_report().unwrap();
        let text = fs::read_to_string(&report).unwrap();
        let holder_record = text
            .split("owner_pid=")
            .skip(1)
            .find(|record| record.starts_with(&format!("{holder_pid}\n")))
            .expect("holder owner record");
        assert!(holder_record.contains(&format!("owner_ppid={}", std::process::id())));
        assert!(holder_record.contains(&format!("owner_dev={}", rejected.dev())));
        assert!(holder_record.contains(&format!("owner_ino={}", rejected.ino())));
        assert!(holder_record.contains("owner_start_time_before="));
        assert!(holder_record.contains("owner_start_time_after="));
        assert!(holder_record.contains("process_identity=stable"));
        assert!(holder_record.contains(&format!(
            "owner_executable_hex={}",
            hex(std::env::current_exe().unwrap().as_os_str().as_bytes())
        )));
        assert!(holder_record.contains("owner_descriptor="));
        assert!(holder_record.contains(&format!(
            "owner_target_hex={}",
            hex(sidecar_path.as_os_str().as_bytes())
        )));
        assert!(holder_record.contains("owner_deleted=false"));
        assert!(holder_record.contains("owner_fdinfo_pos_hex="));
        assert!(holder_record.contains("owner_fdinfo_flags_hex="));
        assert!(holder_record.contains("owner_fdinfo_mnt_id_hex="));
        assert!(holder_record.contains("owner_fdinfo_ino_hex="));
        assert!(text.contains("descriptor_scan=complete"));
        assert!(!text.contains(&format!("owner_pid={unrelated_pid}\n")));
        assert!(!text.contains(SENTINEL));

        holder.release();
        assert!(
            holder
                .wait_bounded(Duration::from_secs(5))
                .unwrap()
                .success()
        );
        unrelated.release();
        assert!(
            unrelated
                .wait_bounded(Duration::from_secs(5))
                .unwrap()
                .success()
        );
        fs::remove_dir_all(report.parent().unwrap()).unwrap();
    }

    #[test]
    fn armed_rejection_records_exact_private_evidence() {
        let (_root, directory) = fixture();
        let sidecar = directory
            .create_database_file(c"brain.sqlite3-wal")
            .unwrap();
        let sidecar_metadata = sidecar.metadata().unwrap();
        let rejected = EntryMetadata::from(&sidecar_metadata);
        drop(sidecar);
        let report_parent = tempfile::tempdir_in("/tmp").unwrap();
        let guard = SidecarDiagnosticGuard::arm_at(report_parent.path().to_owned());
        assert!(matches!(
            directory.validate_database_without_sidecars(c"brain.sqlite3"),
            Err(SecurityError::Invalid("staging SQLite sidecar remains"))
        ));
        let report = guard.take_report().unwrap();
        assert_eq!(
            report
                .parent()
                .unwrap()
                .metadata()
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            report.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        let text = fs::read_to_string(&report).unwrap();
        assert_eq!(fs::read_dir(report.parent().unwrap()).unwrap().count(), 1);
        assert!(text.contains(&format!("rejected_dev={}", rejected.dev)));
        assert!(text.contains(&format!("rejected_ino={}", rejected.ino)));
        assert!(text.contains(&format!("rejected_type={}", file_type(rejected.mode))));
        assert!(text.contains(&format!("rejected_mode={:#o}", rejected.mode)));
        assert!(text.contains(&format!("rejected_uid={}", rejected.uid)));
        assert!(text.contains(&format!(
            "rejected_diagnostic_gid={}",
            sidecar_metadata.gid()
        )));
        assert!(text.contains(&format!("rejected_nlink={}", rejected.nlink)));
        assert!(text.contains(&format!("rejected_length={}", rejected.size)));
        assert!(text.contains("sidecar_name_hex=627261696e2e73716c697465332d77616c"));
        let database_position = text
            .find("entry_name_hex=627261696e2e73716c69746533\n")
            .unwrap();
        let sidecar_position = text
            .find("entry_name_hex=627261696e2e73716c697465332d77616c\n")
            .unwrap();
        assert!(database_position < sidecar_position);
        assert!(text.contains("pathname_identity=same\n"));
        #[cfg(target_os = "linux")]
        assert!(
            text.contains("descriptor_scan=complete\n")
                || text.contains("descriptor_scan=truncated ")
        );
        #[cfg(not(target_os = "linux"))]
        assert!(text.contains("descriptor_scan=unsupported\n"));
        fs::remove_dir_all(report.parent().unwrap()).unwrap();
    }

    #[test]
    fn unarmed_rejection_creates_no_report() {
        let (_root, directory) = fixture();
        let sidecar = directory
            .create_database_file(c"brain.sqlite3-wal")
            .unwrap();
        drop(sidecar);
        let report_parent = tempfile::tempdir_in("/tmp").unwrap();
        drop(SidecarDiagnosticGuard::arm_at(
            report_parent.path().to_owned(),
        ));

        assert!(matches!(
            directory.validate_database_without_sidecars(c"brain.sqlite3"),
            Err(SecurityError::Invalid("staging SQLite sidecar remains"))
        ));
        assert_eq!(fs::read_dir(report_parent.path()).unwrap().count(), 0);
    }

    #[test]
    fn armed_success_creates_no_report() {
        let (_root, directory) = fixture();
        let report_parent = tempfile::tempdir_in("/tmp").unwrap();
        let guard = SidecarDiagnosticGuard::arm_at(report_parent.path().to_owned());

        directory
            .validate_database_without_sidecars(c"brain.sqlite3")
            .unwrap();

        assert!(guard.take_report().is_none());
        assert_eq!(fs::read_dir(report_parent.path()).unwrap().count(), 0);
    }

    #[test]
    fn capture_failure_cannot_change_validator_error() {
        let (_root, directory) = fixture();
        let sidecar = directory
            .create_database_file(c"brain.sqlite3-wal")
            .unwrap();
        drop(sidecar);
        let report_root = tempfile::tempdir_in("/tmp").unwrap();
        let non_directory = report_root.path().join("not-a-directory");
        fs::write(&non_directory, b"fixture").unwrap();
        let guard = SidecarDiagnosticGuard::arm_at(non_directory);

        assert!(matches!(
            directory.validate_database_without_sidecars(c"brain.sqlite3"),
            Err(SecurityError::Invalid("staging SQLite sidecar remains"))
        ));
        assert!(guard.take_report().is_none());
    }

    #[test]
    fn non_utf8_names_are_recorded_losslessly() {
        let (_root, directory) = fixture();
        let database_name = CString::new(b"brain-\xff.sqlite3".to_vec()).unwrap();
        let sidecar_name = CString::new(b"brain-\xff.sqlite3-wal".to_vec()).unwrap();
        let sidecar = directory
            .create_database_file(c"brain.sqlite3-wal")
            .unwrap();
        let rejected = EntryMetadata::from(&sidecar.metadata().unwrap());
        drop(sidecar);
        let report_parent = tempfile::tempdir_in("/tmp").unwrap();
        let guard = SidecarDiagnosticGuard::arm_at(report_parent.path().to_owned());

        capture_sidecar_rejection(
            &directory,
            database_name.as_c_str(),
            sidecar_name.as_c_str(),
            rejected,
        );
        let report = guard.take_report().unwrap();
        let text = fs::read_to_string(&report).unwrap();
        assert!(text.contains("database_name_hex=627261696e2dff2e73716c69746533\n"));
        assert!(text.contains("sidecar_name_hex=627261696e2dff2e73716c697465332d77616c\n"));
        fs::remove_dir_all(report.parent().unwrap()).unwrap();
    }

    #[test]
    fn rejected_pathname_disappearance_is_reported() {
        let (_root, directory) = fixture();
        let sidecar = directory
            .create_database_file(c"brain.sqlite3-wal")
            .unwrap();
        let rejected = EntryMetadata::from(&sidecar.metadata().unwrap());
        drop(sidecar);
        fs::remove_file(directory.path().join("brain.sqlite3-wal")).unwrap();
        let report_parent = tempfile::tempdir_in("/tmp").unwrap();
        let guard = SidecarDiagnosticGuard::arm_at(report_parent.path().to_owned());

        capture_sidecar_rejection(&directory, c"brain.sqlite3", c"brain.sqlite3-wal", rejected);

        let report = guard.take_report().expect("missing pathname report");
        let text = fs::read_to_string(&report).unwrap();
        assert!(text.contains("rejected_diagnostic_gid=unavailable\n"));
        assert!(text.contains("pathname_identity=missing\n"));
        fs::remove_dir_all(report.parent().unwrap()).unwrap();
    }

    #[test]
    fn rejected_pathname_replacement_does_not_supply_rejection_gid() {
        let (_root, directory) = fixture();
        let sidecar = directory
            .create_database_file(c"brain.sqlite3-wal")
            .unwrap();
        let rejected = EntryMetadata::from(&sidecar.metadata().unwrap());
        drop(sidecar);
        fs::remove_file(directory.path().join("brain.sqlite3-wal")).unwrap();
        drop(
            directory
                .create_database_file(c"brain.sqlite3-wal")
                .unwrap(),
        );
        let report_parent = tempfile::tempdir_in("/tmp").unwrap();
        let guard = SidecarDiagnosticGuard::arm_at(report_parent.path().to_owned());

        capture_sidecar_rejection(&directory, c"brain.sqlite3", c"brain.sqlite3-wal", rejected);

        let report = guard.take_report().expect("replaced pathname report");
        let text = fs::read_to_string(&report).unwrap();
        assert!(text.contains("rejected_diagnostic_gid=unavailable\n"));
        assert!(text.contains("pathname_identity=replaced\n"));
        fs::remove_dir_all(report.parent().unwrap()).unwrap();
    }

    #[test]
    fn nested_guards_keep_reports_owned_by_their_receiver() {
        let (_root, directory) = fixture();
        drop(
            directory
                .create_database_file(c"brain.sqlite3-wal")
                .unwrap(),
        );
        let outer_parent = tempfile::tempdir_in("/tmp").unwrap();
        let inner_parent = tempfile::tempdir_in("/tmp").unwrap();
        let outer = SidecarDiagnosticGuard::arm_at(outer_parent.path().to_owned());
        let inner = SidecarDiagnosticGuard::arm_at(inner_parent.path().to_owned());

        assert!(matches!(
            directory.validate_database_without_sidecars(c"brain.sqlite3"),
            Err(SecurityError::Invalid("staging SQLite sidecar remains"))
        ));

        assert!(outer.take_report().is_none());
        let report = inner.take_report().expect("inner guard report");
        assert_eq!(report.parent().unwrap().parent(), Some(inner_parent.path()));
        fs::remove_dir_all(report.parent().unwrap()).unwrap();
    }

    #[test]
    fn out_of_order_guard_drop_does_not_restore_stale_capture() {
        let (_root, directory) = fixture();
        drop(
            directory
                .create_database_file(c"brain.sqlite3-wal")
                .unwrap(),
        );
        let outer_parent = tempfile::tempdir_in("/tmp").unwrap();
        let inner_parent = tempfile::tempdir_in("/tmp").unwrap();
        let outer = SidecarDiagnosticGuard::arm_at(outer_parent.path().to_owned());
        let inner = SidecarDiagnosticGuard::arm_at(inner_parent.path().to_owned());
        drop(outer);
        drop(inner);

        assert!(matches!(
            directory.validate_database_without_sidecars(c"brain.sqlite3"),
            Err(SecurityError::Invalid("staging SQLite sidecar remains"))
        ));
        assert_eq!(fs::read_dir(outer_parent.path()).unwrap().count(), 0);
        assert_eq!(fs::read_dir(inner_parent.path()).unwrap().count(), 0);
    }

    #[test]
    fn dropping_inner_guard_restores_outer_capture() {
        let (_root, directory) = fixture();
        drop(
            directory
                .create_database_file(c"brain.sqlite3-wal")
                .unwrap(),
        );
        let outer_parent = tempfile::tempdir_in("/tmp").unwrap();
        let inner_parent = tempfile::tempdir_in("/tmp").unwrap();
        let outer = SidecarDiagnosticGuard::arm_at(outer_parent.path().to_owned());
        let inner = SidecarDiagnosticGuard::arm_at(inner_parent.path().to_owned());
        drop(inner);

        assert!(matches!(
            directory.validate_database_without_sidecars(c"brain.sqlite3"),
            Err(SecurityError::Invalid("staging SQLite sidecar remains"))
        ));

        let report = outer.take_report().expect("outer guard report");
        assert_eq!(report.parent().unwrap().parent(), Some(outer_parent.path()));
        assert_eq!(fs::read_dir(inner_parent.path()).unwrap().count(), 0);
        fs::remove_dir_all(report.parent().unwrap()).unwrap();
    }

    #[test]
    fn disappearing_snapshot_entry_does_not_abort_report() {
        fn remove_snapshot_entry(directory: &SecureDatabaseDirectory) {
            fs::remove_file(directory.path().join("vanishing-entry")).unwrap();
        }

        let (_root, directory) = fixture();
        drop(
            directory
                .create_database_file(c"brain.sqlite3-wal")
                .unwrap(),
        );
        drop(directory.create_database_file(c"vanishing-entry").unwrap());
        let rejected = metadata_at(&directory.descriptor, c"brain.sqlite3-wal").unwrap();
        let report_parent = tempfile::tempdir_in("/tmp").unwrap();
        let guard = SidecarDiagnosticGuard::arm_with(CaptureConfig {
            parent: report_parent.path().to_owned(),
            descriptor_limit: 4_096,
            deadline: Duration::from_secs(1),
            after_read_dir: Some(remove_snapshot_entry),
            diagnostic_sink: None,
        });

        capture_sidecar_rejection(&directory, c"brain.sqlite3", c"brain.sqlite3-wal", rejected);

        let report = guard.take_report().expect("snapshot churn report");
        let text = fs::read_to_string(&report).unwrap();
        assert!(text.contains("entry_name_hex=76616e697368696e672d656e747279\n"));
        assert!(text.contains("entry_metadata=unavailable:not_found\n"));
        assert!(text.contains("pathname_identity=same\n"));
        fs::remove_dir_all(report.parent().unwrap()).unwrap();
    }

    #[test]
    fn diagnostic_logging_failure_cannot_replace_validator_error() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        struct FailingWriter(Arc<AtomicBool>);
        impl Write for FailingWriter {
            fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
                self.0.store(true, Ordering::Relaxed);
                Err(io::Error::other("injected diagnostic sink failure"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let (_root, directory) = fixture();
        drop(
            directory
                .create_database_file(c"brain.sqlite3-wal")
                .unwrap(),
        );
        let report_parent = tempfile::tempdir_in("/tmp").unwrap();
        let attempted = Arc::new(AtomicBool::new(false));
        let guard = SidecarDiagnosticGuard::arm_with(CaptureConfig {
            parent: report_parent.path().to_owned(),
            descriptor_limit: 4_096,
            deadline: Duration::from_secs(1),
            after_read_dir: None,
            diagnostic_sink: Some(Rc::new(RefCell::new(Box::new(FailingWriter(Arc::clone(
                &attempted,
            )))))),
        });

        let result = directory.validate_database_without_sidecars(c"brain.sqlite3");

        assert!(attempted.load(Ordering::Relaxed));
        assert!(matches!(
            result,
            Err(SecurityError::Invalid("staging SQLite sidecar remains"))
        ));
        let report = guard.take_report().expect("capture still completes");
        fs::remove_dir_all(report.parent().unwrap()).unwrap();
    }
}
