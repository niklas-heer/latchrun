//! Public CLI timing only: isolated fake credentials, production persistence and sandboxing.

use std::{
    error::Error,
    fs::{self, DirBuilder, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const WARMUPS: usize = 5;
const TRUE: &str = "/usr/bin/true";
const FAKE: &str = "latchrun-fake-benchmark";
static NEXT_ROOT: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy)]
enum Provider {
    None,
    Fake,
    Delayed,
}

#[derive(Clone, Copy)]
struct Case {
    name: &'static str,
    cached: bool,
    provider: Provider,
    sandbox: bool,
}

const CASES: [Case; 6] = [
    Case {
        name: "no_credentials",
        cached: false,
        provider: Provider::None,
        sandbox: false,
    },
    Case {
        name: "fake_per_command",
        cached: false,
        provider: Provider::Fake,
        sandbox: false,
    },
    Case {
        name: "fake_cache_hit",
        cached: true,
        provider: Provider::Fake,
        sandbox: false,
    },
    Case {
        name: "provider_20ms_uncached",
        cached: false,
        provider: Provider::Delayed,
        sandbox: false,
    },
    Case {
        name: "provider_20ms_cached",
        cached: true,
        provider: Provider::Delayed,
        sandbox: false,
    },
    Case {
        name: "sandbox_no_credentials",
        cached: false,
        provider: Provider::None,
        sandbox: true,
    },
];

struct Service {
    root: PathBuf,
    child: Option<Child>,
}

impl Service {
    fn new() -> Result<Self> {
        let root = PathBuf::from(format!(
            "/tmp/lrb-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        DirBuilder::new().mode(0o700).create(&root)?;
        let service = Self {
            root: root.canonicalize()?,
            child: None,
        };
        for name in ["runtime", "data", "project"] {
            DirBuilder::new()
                .mode(0o700)
                .create(service.root.join(name))?;
        }
        Ok(service)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_latchrun"));
        command
            .env_clear()
            .arg("--runtime-dir")
            .arg(self.root.join("runtime"))
            .arg("--data-dir")
            .arg(self.root.join("data"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn cli(&self, arguments: &[&str]) -> Result<Output> {
        checked(self.command().args(arguments).output()?)
    }

    fn start(&mut self, cancelled: &AtomicBool) -> Result<u64> {
        let mut command = self.command();
        command
            .args(["service", "serve"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let start = Instant::now();
        self.child = Some(command.spawn()?);
        loop {
            if self
                .command()
                .args(["service", "status"])
                .output()?
                .status
                .success()
            {
                return nanoseconds(start.elapsed());
            }
            check_cancelled(cancelled)?;
            if start.elapsed() > Duration::from_secs(10)
                || self
                    .child
                    .as_mut()
                    .is_some_and(|child| child.try_wait().is_ok_and(|status| status.is_some()))
            {
                return Err("benchmark service did not become ready".into());
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn session(&self, case: Case) -> Result<()> {
        let mut profile = json!({"project":self.root.join("project"),"purpose":"isolated latency measurement",
            "provider":if matches!(case.provider, Provider::Delayed) {"one_password"} else {"fake"},
            "cache_ttl_seconds":if case.cached {900} else {0},
            "commands":[{"executable":TRUE,"args":[]}],
            "sandbox":{"enabled":case.sandbox,"network":"deny"}});
        if !matches!(case.provider, Provider::None) {
            profile["credentials"] = json!({"TOKEN":if matches!(case.provider, Provider::Delayed) {"op://fixture/benchmark/value"} else {"fake://benchmark"}});
        }
        if matches!(case.provider, Provider::Delayed) {
            profile["op_path"] = json!(std::env::current_exe()?);
        }
        let path = self.root.join(format!("{}.json", case.name));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(profile.to_string().as_bytes())?;
        self.cli(&[
            "session",
            "start",
            case.name,
            "--profile",
            path.to_str().ok_or("non-UTF8 fixture path")?,
        ])?;
        Ok(())
    }

    fn direct(&self, case: Case) -> Result<u64> {
        let mut command = Command::new(TRUE);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LANG", "C")
            .current_dir(self.root.join("project"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if !matches!(case.provider, Provider::None) {
            command.env("TOKEN", FAKE);
        }
        timed(&mut command)
    }

    fn run(&self, case: Case, operation: &str) -> Result<u64> {
        let mut command = self.command();
        command.stdout(Stdio::null());
        command.args(["run", case.name, "--operation", operation, "--", TRUE]);
        timed(&mut command)
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = self.command().args(["service", "stop"]).output();
            let deadline = Instant::now() + Duration::from_secs(5);
            while child.try_wait().is_ok_and(|status| status.is_none()) && Instant::now() < deadline
            {
                thread::sleep(Duration::from_millis(5));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn checked(output: Output) -> Result<Output> {
    if !output.status.success() {
        return Err(format!(
            "benchmark command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(output)
}

fn nanoseconds(duration: Duration) -> Result<u64> {
    Ok(duration.as_nanos().try_into()?)
}

fn timed(command: &mut Command) -> Result<u64> {
    let start = Instant::now();
    let output = command.output()?;
    let elapsed = nanoseconds(start.elapsed())?;
    checked(output)?;
    Ok(elapsed)
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::Relaxed) {
        return Err("benchmark interrupted; fixtures cleaned up".into());
    }
    Ok(())
}

fn setting(name: &str, default: usize, minimum: usize, maximum: usize) -> Result<usize> {
    let value = std::env::var(name).map_or(Ok(default), |value| value.parse::<usize>())?;
    if !(minimum..=maximum).contains(&value) {
        return Err(format!("{name} must be {minimum}..={maximum}").into());
    }
    Ok(value)
}

fn distribution(values: &[i64]) -> Value {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let percentile = |percent: usize| {
        sorted
            .get((sorted.len() * percent).div_ceil(100).saturating_sub(1))
            .copied()
    };
    json!({"samples":sorted.len(),"min_ns":sorted.first(),"median_ns":percentile(50),"p95_ns":percentile(95),"p99_ns":percentile(99),"max_ns":sorted.last()})
}

fn diagnostic(program: &str, arguments: &[&str]) -> String {
    Command::new(program)
        .args(arguments)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map_or_else(
            || "unavailable".into(),
            |output| String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        )
}

fn platform(project: &Path) -> Result<Value> {
    #[cfg(target_os = "macos")]
    let filesystem = nix::sys::statfs::statfs(project)?;
    #[cfg(target_os = "macos")]
    let (cpu, os, filesystem_name) = (
        diagnostic("/usr/sbin/sysctl", &["-n", "machdep.cpu.brand_string"]),
        diagnostic("/usr/bin/sw_vers", &["-productVersion"]),
        filesystem.filesystem_type_name().to_owned(),
    );
    #[cfg(target_os = "linux")]
    let (cpu, os, filesystem_name) = (
        fs::read_to_string("/proc/cpuinfo")?
            .lines()
            .find_map(|line| {
                line.strip_prefix("model name")
                    .or_else(|| line.strip_prefix("Hardware"))
            })
            .map_or_else(
                || "unreported".into(),
                |line| line.trim_start_matches([' ', '\t', ':']).to_owned(),
            ),
        fs::read_to_string("/etc/os-release")?
            .lines()
            .find_map(|line| line.strip_prefix("PRETTY_NAME="))
            .map_or_else(
                || "unreported".into(),
                |name| name.trim_matches('"').to_owned(),
            ),
        diagnostic(
            "/usr/bin/stat",
            &[
                "-f",
                "-c",
                "%T",
                project.to_str().ok_or("non-UTF8 fixture path")?,
            ],
        ),
    );
    Ok(
        json!({"os":std::env::consts::OS,"os_version":os,"kernel":diagnostic("/usr/bin/uname",&["-r"]),
        "architecture":std::env::consts::ARCH,"cpu":cpu,"effective_threads":std::thread::available_parallelism()?.get(),
        "filesystem":filesystem_name,
        "storage_location":"private fixture under /tmp; runtime and analytics on the same filesystem",
        "latchrun_version":env!("CARGO_PKG_VERSION"),"rustc":diagnostic("rustc",&["--version"]),
        "git_commit":diagnostic("git",&["rev-parse","HEAD"]),"git_tracked_changes":!diagnostic("git",&["diff","--name-only"]).is_empty(),
        "profile":"release","sqlite":"bundled; journal_mode=DELETE; synchronous=FULL",
        "sandbox_backend":if cfg!(target_os="macos") {"sandbox-exec"} else {"bubblewrap+seccomp"},
        "bubblewrap_version":if cfg!(target_os="linux") {Some(diagnostic("/usr/bin/bwrap",&["--version"]))} else {None}}),
    )
}

#[derive(Default)]
struct Measurements {
    direct: Vec<i64>,
    latchrun: Vec<i64>,
    differences: Vec<i64>,
}

fn collect(service: &Service, samples: usize, cancelled: &AtomicBool) -> Result<Vec<Value>> {
    let mut measurements: Vec<Measurements> =
        CASES.iter().map(|_| Measurements::default()).collect();
    for round in 0..WARMUPS + samples {
        // Rotate the first case and alternate pair order to spread chronological drift.
        for offset in 0..CASES.len() {
            check_cancelled(cancelled)?;
            let index = (round + offset) % CASES.len();
            let case = CASES[index];
            let operation = format!("sample_{round}_{index}");
            let (direct, latchrun) = if (round + index).is_multiple_of(2) {
                (service.direct(case)?, service.run(case, &operation)?)
            } else {
                let latchrun = service.run(case, &operation)?;
                (service.direct(case)?, latchrun)
            };
            if round >= WARMUPS {
                let direct = i64::try_from(direct)?;
                let latchrun = i64::try_from(latchrun)?;
                measurements[index].direct.push(direct);
                measurements[index].latchrun.push(latchrun);
                measurements[index].differences.push(latchrun - direct);
            }
        }
        if round >= WARMUPS && (round - WARMUPS + 1).is_multiple_of(25) {
            eprintln!(
                "Measured {} / {samples} pairs per case",
                round - WARMUPS + 1
            );
        }
    }
    Ok(CASES.iter().zip(measurements).map(|(case,measurement)|json!({
        "case":case.name,"direct":distribution(&measurement.direct),"latchrun":distribution(&measurement.latchrun),
        "paired_overhead":distribution(&measurement.differences),
        "negative_overhead_samples":measurement.differences.iter().filter(|&&value|value<0).count(),
        "raw":{"direct_ns":measurement.direct,"latchrun_ns":measurement.latchrun,"paired_overhead_ns":measurement.differences}
    })).collect())
}

fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments == ["read", "--no-newline", "op://fixture/benchmark/value"] {
        thread::sleep(Duration::from_millis(20));
        print!("{FAKE}");
        return Ok(());
    }
    if cfg!(debug_assertions) {
        return Err("run the latency benchmark with cargo bench (release profile)".into());
    }
    let samples = setting("LATCHRUN_BENCH_SAMPLES", 100, 1, 165)?;
    let cold_samples = setting("LATCHRUN_BENCH_COLD_SAMPLES", 20, 0, 100)?;
    let cancelled = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&cancelled))?;
    }
    let started_at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let mut service = Service::new()?;
    let metadata = platform(&service.root)?;
    service.start(&cancelled)?;
    for case in CASES {
        service.session(case)?;
    }
    eprintln!(
        "Measuring {samples} pairs for each of six cases; {WARMUPS} warmups per case; fake credentials only."
    );
    let cases = collect(&service, samples, &cancelled)?;
    let stats: Value = serde_json::from_slice(&service.cli(&["stats", "--days", "1"])?.stdout)?;
    let expected = (samples + WARMUPS) * CASES.len();
    if stats["totals"]["succeeded"].as_u64() != Some(u64::try_from(expected)?)
        || stats["totals"]["failed"] != 0
        || stats["cache"]["hits"].as_u64() != Some(u64::try_from(2 * (samples + WARMUPS - 1))?)
        || stats["cache"]["misses"] != 2
    {
        return Err("benchmark operation accounting mismatch".into());
    }
    drop(service);
    let mut cold = Vec::with_capacity(cold_samples);
    for _ in 0..cold_samples {
        check_cancelled(&cancelled)?;
        let mut service = Service::new()?;
        cold.push(i64::try_from(service.start(&cancelled)?)?);
    }
    let report = json!({"schema_version":1,"started_at_unix_seconds":started_at,"environment":metadata,
        "method":{"command":TRUE,"argv":[],"child_environment":"PATH=/usr/bin:/bin; LANG=C; TOKEN=fake fixture only for credential cases",
            "stdio":"null stdin/stdout; captured empty stderr on both paths","warmups_per_case":WARMUPS,"samples_per_case":samples,
            "case_order":"round robin, rotating first case each round","pair_order":"alternates direct-first and latchrun-first",
            "clock":"std::time::Instant; wall-clock nanoseconds","percentiles":"exact nearest rank; median is rank ceil(n/2)",
            "negative_differences":"retained signed; never clamped or discarded","pruning":"none during measurement",
            "ledger_entries_at_first_sample":WARMUPS*CASES.len(),"ledger_entries_at_end":expected,
            "persistence":"normal journal and SQLite commits remain enabled",
            "provider_fixture":"benchmark executable sleeps 20 ms then returns a fake value; not live 1Password or unlock latency",
            "cold_start":"fresh private runtime/data; service serve spawn through first successful public service status; 1 ms polling sleep; binary and OS caches not flushed"},
        "cases":cases,"cold_service_start":distribution(&cold),"cold_service_start_raw_ns":cold,
        "verification":{"operations":stats["totals"],"cache":stats["cache"]}});
    write_report(&report, started_at, cold_samples)
}

fn write_report(report: &Value, started_at: u64, cold_samples: usize) -> Result<()> {
    let output = std::env::var_os("LATCHRUN_BENCH_OUTPUT").map_or_else(
        || {
            PathBuf::from(format!(
                "scratch/latency-{}-{started_at}.json",
                std::env::consts::OS
            ))
        },
        PathBuf::from,
    );
    if let Some(parent) = output.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&output)?;
    serde_json::to_writer_pretty(&mut file, report)?;
    writeln!(file)?;
    println!(
        "case                           direct median   latchrun median   added median / p95 / p99 (ms)"
    );
    for case in report["cases"]
        .as_array()
        .ok_or("invalid benchmark report")?
    {
        let ms = |field: &str, percentile: &str| {
            case[field][percentile].as_f64().unwrap_or_default() / 1_000_000.0
        };
        println!(
            "{:<30} {:>10.3} ms {:>13.3} ms {:>10.3} / {:.3} / {:.3}",
            case["case"].as_str().unwrap_or_default(),
            ms("direct", "median_ns"),
            ms("latchrun", "median_ns"),
            ms("paired_overhead", "median_ns"),
            ms("paired_overhead", "p95_ns"),
            ms("paired_overhead", "p99_ns")
        );
    }
    println!(
        "Fresh service readiness: {} samples; median {:.3} ms, p95 {:.3} ms",
        cold_samples,
        report["cold_service_start"]["median_ns"]
            .as_f64()
            .unwrap_or_default()
            / 1_000_000.0,
        report["cold_service_start"]["p95_ns"]
            .as_f64()
            .unwrap_or_default()
            / 1_000_000.0
    );
    println!("Sanitized JSON: {}", output.display());
    Ok(())
}
