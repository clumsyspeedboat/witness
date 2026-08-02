use std::ffi::CString;
use std::fs::{self, File};
use std::hint::black_box;
use std::io::{BufWriter, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

use witness::access_compiler::{ClosureMode, ReadSession, Span, coalesce_ranges};
use witness::experiment::access_real::RealAccessColumn;

use super::measure::{PlanKind, QueryKind, Task, answer_checksum, bounds, execute_session};
use super::storage::{StorageBundle, StorageTier};

const SCAN_PAGES: usize = 1_024;
const PAGE_ALIGNMENT: usize = 4_096;
const REPEATS: usize = 3;

#[derive(Clone)]
struct ScanTask {
    query: Task,
    closure: Vec<Span>,
}

#[derive(Clone, Copy)]
enum AccessOrder {
    Sequential,
    Random,
}

impl AccessOrder {
    fn name(self) -> &'static str {
        match self {
            Self::Sequential => "sequential",
            Self::Random => "random",
        }
    }
}

#[derive(Clone, Copy)]
enum SchedulePolicy {
    PerPage,
    Coalesce(usize),
    FullFile,
}

impl SchedulePolicy {
    const ALL: [Self; 6] = [
        Self::PerPage,
        Self::Coalesce(0),
        Self::Coalesce(512),
        Self::Coalesce(4_096),
        Self::Coalesce(65_536),
        Self::FullFile,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::PerPage => "per_page",
            Self::Coalesce(0) => "sorted_closure",
            Self::Coalesce(512) => "coalesce_512b",
            Self::Coalesce(4_096) => "coalesce_4k",
            Self::Coalesce(65_536) => "coalesce_64k",
            Self::Coalesce(_) => "coalesce_custom",
            Self::FullFile => "full_file",
        }
    }
}

struct Device {
    name: &'static str,
    directory: PathBuf,
    persistent: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ScanAnswer {
    checksum: i128,
    query_transferred_bytes: usize,
}

#[derive(Clone, Copy)]
struct Counters {
    read_bytes: u64,
    read_chars: u64,
    minor_faults: i64,
    major_faults: i64,
}

impl Counters {
    fn delta(self, before: Self) -> Self {
        Self {
            read_bytes: self.read_bytes.saturating_sub(before.read_bytes),
            read_chars: self.read_chars.saturating_sub(before.read_chars),
            minor_faults: self.minor_faults.saturating_sub(before.minor_faults),
            major_faults: self.major_faults.saturating_sub(before.major_faults),
        }
    }
}

struct Sample {
    ns: f64,
    answer: ScanAnswer,
    counters: Counters,
}

pub fn run(
    columns: &[RealAccessColumn],
    result_dir: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let temporary = format!("witness-real-access-{}", std::process::id());
    let devices = [
        Device {
            name: "workspace_mount",
            directory: PathBuf::from(format!("{result_dir}/storage_xfs")),
            persistent: true,
        },
        Device {
            name: "temporary_mount",
            directory: PathBuf::from("/tmp").join(&temporary),
            persistent: false,
        },
        Device {
            name: "memory_mount",
            directory: PathBuf::from("/dev/shm").join(&temporary),
            persistent: false,
        },
    ];
    let mut output = BufWriter::new(File::create(format!("{result_dir}/storage_scan.csv"))?);
    writeln!(
        output,
        "storage,filesystem,order,cache_state,policy,pages,logical_rows,selected_rows,file_bytes,required_bytes,scheduled_bytes,read_calls,gap_bytes,p25_ns,median_ns,p75_ns,os_read_bytes,os_read_chars,minor_faults,major_faults,query_transferred_bytes,checksum"
    )?;
    let mut expected = None;
    for device in devices {
        fs::create_dir_all(&device.directory)?;
        let filesystem = filesystem_name(&device.directory)?;
        let path = device.directory.join("real_scan_bundle.acp");
        let page_cases = (0..SCAN_PAGES)
            .map(|page| page % columns.len())
            .collect::<Vec<_>>();
        let pages = page_cases
            .iter()
            .map(|&case| columns[case].size_selected.page.bytes())
            .collect::<Vec<_>>();
        let bundle = StorageBundle::build(&path, &pages, PAGE_ALIGNMENT)?;
        let base_tasks = tasks(columns, &page_cases, &bundle)?;
        let mut warm_buffer = vec![0_u8; bundle.file_len()];
        for order in [AccessOrder::Sequential, AccessOrder::Random] {
            let ordered = ordered_tasks(&base_tasks, order);
            for cold in [false, true] {
                for policy in SchedulePolicy::ALL {
                    let ranges = absolute_ranges(&ordered, &bundle);
                    let schedule = match policy {
                        SchedulePolicy::PerPage | SchedulePolicy::Coalesce(0) => {
                            coalesce_ranges(ranges, 0)?
                        }
                        SchedulePolicy::Coalesce(gap) => coalesce_ranges(ranges, gap)?,
                        SchedulePolicy::FullFile => {
                            let required = coalesce_ranges(ranges, 0)?.required_bytes;
                            witness::access_compiler::ReadSchedule {
                                ranges: vec![Span::new(0, bundle.file_len())?],
                                required_bytes: required,
                                scheduled_bytes: bundle.file_len(),
                            }
                        }
                    };
                    let mut samples = Vec::with_capacity(REPEATS);
                    let mut buffer = vec![0_u8; bundle.file_len()];
                    for _ in 0..REPEATS {
                        if cold {
                            bundle.evict_all()?;
                        } else {
                            warm(&bundle, &mut warm_buffer)?;
                        }
                        buffer.fill(0);
                        let before = counters()?;
                        let started = Instant::now();
                        let answer = match policy {
                            SchedulePolicy::PerPage => direct_scan(columns, &ordered, &bundle)?,
                            _ => scheduled_scan(
                                columns,
                                &ordered,
                                &bundle,
                                &schedule.ranges,
                                &mut buffer,
                            )?,
                        };
                        let ns = started.elapsed().as_nanos() as f64;
                        samples.push(Sample {
                            ns,
                            answer: black_box(answer),
                            counters: counters()?.delta(before),
                        });
                    }
                    if samples
                        .iter()
                        .any(|sample| sample.answer != samples[0].answer)
                    {
                        return Err("real storage scan repetitions disagree".into());
                    }
                    if let Some(expected) = expected {
                        if samples[0].answer.checksum != expected {
                            return Err("real storage scan policies disagree".into());
                        }
                    } else {
                        expected = Some(samples[0].answer.checksum);
                    }
                    samples.sort_by(|left, right| left.ns.total_cmp(&right.ns));
                    let sample = &samples[1];
                    let read_calls = match policy {
                        SchedulePolicy::PerPage => absolute_ranges(&ordered, &bundle).len(),
                        _ => schedule.ranges.len(),
                    };
                    let scheduled_bytes = match policy {
                        SchedulePolicy::PerPage => schedule.required_bytes,
                        _ => schedule.scheduled_bytes,
                    };
                    writeln!(
                        output,
                        "{},{},{},{},{},{},{},{},{},{},{},{},{},{:.1},{:.1},{:.1},{},{},{},{},{},{}",
                        device.name,
                        filesystem,
                        order.name(),
                        if cold { "cold" } else { "warm" },
                        policy.name(),
                        SCAN_PAGES,
                        ordered
                            .iter()
                            .map(|task| columns[task.query.case].size_selected.truth.len())
                            .sum::<usize>(),
                        ordered
                            .iter()
                            .map(|task| task.query.rows.len())
                            .sum::<usize>(),
                        bundle.file_len(),
                        schedule.required_bytes,
                        scheduled_bytes,
                        read_calls,
                        scheduled_bytes.saturating_sub(schedule.required_bytes),
                        samples[0].ns,
                        sample.ns,
                        samples[2].ns,
                        sample.counters.read_bytes,
                        sample.counters.read_chars,
                        sample.counters.minor_faults,
                        sample.counters.major_faults,
                        sample.answer.query_transferred_bytes,
                        sample.answer.checksum,
                    )?;
                }
            }
        }
        drop(bundle);
        if !device.persistent {
            fs::remove_dir_all(&device.directory)?;
        }
        println!("completed real storage scans on {}", device.name);
    }
    output.flush()?;
    Ok(())
}

fn filesystem_name(path: &Path) -> Result<&'static str, String> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| "filesystem path contains a NUL byte".to_string())?;
    let mut info = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: statfs initializes `info` when it returns zero.
    if unsafe { libc::statfs(path.as_ptr(), info.as_mut_ptr()) } != 0 {
        return Err(format!(
            "statfs failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: the successful statfs call initialized `info` above.
    let kind = unsafe { info.assume_init() }.f_type as u64;
    Ok(match kind {
        0x5846_5342 => "xfs",
        0x0000_ef53 => "ext4",
        0x0102_1994 => "tmpfs",
        _ => "other",
    })
}

fn tasks(
    columns: &[RealAccessColumn],
    page_cases: &[usize],
    bundle: &StorageBundle,
) -> Result<Vec<ScanTask>, String> {
    page_cases
        .iter()
        .enumerate()
        .map(|(page_index, &case)| {
            let column = &columns[case].size_selected;
            let widths = [
                1,
                (column.truth.len() / 100).max(1),
                column.truth.len() / 10,
                column.truth.len() / 2,
                column.truth.len(),
            ];
            let width = widths[page_index % widths.len()];
            let start = (column.truth.len() - width) / 2;
            let (low, high) = bounds(&column.truth);
            let plan = if width == column.truth.len() {
                PlanKind::Fused
            } else {
                PlanKind::Selective
            };
            let query = Task {
                page_index,
                case,
                query: if page_index.is_multiple_of(2) {
                    QueryKind::Sum
                } else {
                    QueryKind::Between
                },
                rows: Span::new(start, start + width)?,
                low,
                high,
                plan,
            };
            let mode = if plan == PlanKind::Fused {
                ClosureMode::FullPage
            } else {
                ClosureMode::Selective
            };
            let mut session =
                bundle.session(&column.page, StorageTier::Memory, page_index, mode)?;
            black_box(execute_session(
                case,
                column,
                query.query,
                query.rows,
                low,
                high,
                plan,
                &mut session,
            )?);
            Ok(ScanTask {
                query,
                closure: session.transferred_ranges().to_vec(),
            })
        })
        .collect()
}

fn ordered_tasks(tasks: &[ScanTask], order: AccessOrder) -> Vec<ScanTask> {
    let mut output = tasks.to_vec();
    if matches!(order, AccessOrder::Random) {
        output.sort_by_key(|task| {
            (task.query.page_index as u64)
                .wrapping_mul(6_364_136_223_846_793_005)
                .rotate_left(17)
        });
    }
    output
}

fn absolute_ranges(tasks: &[ScanTask], bundle: &StorageBundle) -> Vec<Span> {
    tasks
        .iter()
        .flat_map(|task| {
            let offset = bundle.offsets[task.query.page_index] as usize;
            task.closure.iter().map(move |range| Span {
                start: offset + range.start,
                end: offset + range.end,
            })
        })
        .collect()
}

fn direct_scan(
    columns: &[RealAccessColumn],
    tasks: &[ScanTask],
    bundle: &StorageBundle,
) -> Result<ScanAnswer, String> {
    let mut answer = ScanAnswer {
        checksum: 0,
        query_transferred_bytes: 0,
    };
    for task in tasks {
        let query = task.query;
        let column = &columns[query.case].size_selected;
        let mode = if query.plan == PlanKind::Fused {
            ClosureMode::FullPage
        } else {
            ClosureMode::Selective
        };
        let mut session = bundle.session(
            &column.page,
            StorageTier::BufferedHot,
            query.page_index,
            mode,
        )?;
        let execution = execute_session(
            query.case,
            column,
            query.query,
            query.rows,
            query.low,
            query.high,
            query.plan,
            &mut session,
        )?;
        answer.checksum = answer
            .checksum
            .checked_add(answer_checksum(&execution.answer))
            .ok_or("real scan checksum overflow")?;
        answer.query_transferred_bytes += execution.metrics.transferred_bytes;
    }
    Ok(answer)
}

fn scheduled_scan(
    columns: &[RealAccessColumn],
    tasks: &[ScanTask],
    bundle: &StorageBundle,
    ranges: &[Span],
    buffer: &mut [u8],
) -> Result<ScanAnswer, String> {
    for range in ranges {
        bundle
            .file()
            .read_exact_at(&mut buffer[range.start..range.end], range.start as u64)
            .map_err(|error| format!("coalesced read failed: {error}"))?;
    }
    let mut answer = ScanAnswer {
        checksum: 0,
        query_transferred_bytes: 0,
    };
    for task in tasks {
        let query = task.query;
        let column = &columns[query.case].size_selected;
        let mode = if query.plan == PlanKind::Fused {
            ClosureMode::FullPage
        } else {
            ClosureMode::Selective
        };
        let mut session = ReadSession::from_bytes(
            &column.page,
            mode,
            buffer,
            bundle.offsets[query.page_index] as usize,
        )?;
        let execution = execute_session(
            query.case,
            column,
            query.query,
            query.rows,
            query.low,
            query.high,
            query.plan,
            &mut session,
        )?;
        answer.checksum = answer
            .checksum
            .checked_add(answer_checksum(&execution.answer))
            .ok_or("real scan checksum overflow")?;
        answer.query_transferred_bytes += execution.metrics.transferred_bytes;
    }
    Ok(answer)
}

fn warm(bundle: &StorageBundle, buffer: &mut [u8]) -> Result<(), String> {
    bundle
        .file()
        .read_exact_at(buffer, 0)
        .map_err(|error| format!("warm-up read failed: {error}"))
}

fn counters() -> Result<Counters, String> {
    let io = fs::read_to_string("/proc/self/io")
        .map_err(|error| format!("cannot read /proc/self/io: {error}"))?;
    let value = |name: &str| -> Result<u64, String> {
        io.lines()
            .find_map(|line| line.strip_prefix(name))
            .ok_or_else(|| format!("/proc/self/io lacks {name}"))?
            .trim()
            .parse()
            .map_err(|error| format!("invalid /proc/self/io counter: {error}"))
    };
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage initializes the supplied object on success.
    let status = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if status != 0 {
        return Err(format!(
            "getrusage failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: successful getrusage initialized `usage` above.
    let usage = unsafe { usage.assume_init() };
    Ok(Counters {
        read_bytes: value("read_bytes:")?,
        read_chars: value("rchar:")?,
        minor_faults: usage.ru_minflt,
        major_faults: usage.ru_majflt,
    })
}
