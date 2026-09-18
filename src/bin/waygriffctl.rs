//! Agent-facing companion CLI: launch waygriff out of the way on a Hyprland
//! workspace, read its stats, flip its state, grab screenshots, stop it.

use anyhow::{Context as _, Result, bail};
use clap::{Parser, Subcommand};
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::linux::net::SocketAddrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{SocketAddr, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const CTL_SOCKET: &str = "waygriff-0.ctl";
const SNAP_SOCKET: &str = "waygriff-0.snap";
const PROJECT_DIR: &str = env!("CARGO_MANIFEST_DIR");

#[derive(Parser)]
#[command(version, about = "Control a running waygriff, or launch one")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build and launch waygriff headless on a Hyprland workspace, fullscreen, without taking focus.
    ///
    /// Logs go to temp/waygriff.log; the process is a child of Hyprland, so it survives this shell.
    Start {
        /// Capture backend
        #[arg(long, default_value = "nvfbc")]
        backend: String,
        /// Source display to capture
        #[arg(long, default_value = ":80")]
        display: String,
        /// Hyprland workspace name to place the window on
        #[arg(long, default_value = "F1")]
        workspace: String,
        /// Leave the window in its normal tiled state
        #[arg(long)]
        no_fullscreen: bool,
        /// Skip `cargo build`
        #[arg(long)]
        no_build: bool,
        /// Log a stats line at this interval, e.g. 5s
        #[arg(long, default_value = "5s", value_parser = humantime::parse_duration)]
        stats_every: Duration,
        /// Extra arguments passed through to waygriff
        #[arg(last = true)]
        extra: Vec<String>,
    },
    /// Ask waygriff to exit, falling back to SIGTERM
    Stop,
    /// Is waygriff running and answering?
    Status,
    /// Print the stats snapshot as JSON
    Stats {
        /// Repeat at this interval, e.g. 1s
        #[arg(long, value_parser = humantime::parse_duration)]
        watch: Option<Duration>,
    },
    /// Print the confine/capture/relative flags
    State,
    /// Set a flag: confine, capture, or relative
    Set { flag: String, value: String },
    /// Grab the current frame as PNG
    Snap {
        /// Output path
        #[arg(short, long, default_value = "temp/snap.png")]
        out: PathBuf,
        /// Also render it in the terminal
        #[arg(long)]
        show: bool,
    },
    /// Print the tail of temp/waygriff.log
    Log {
        #[arg(short = 'n', default_value_t = 40)]
        lines: usize,
    },
    /// Send a raw control command line and print the reply
    Raw { line: Vec<String> },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Start {
            backend,
            display,
            workspace,
            no_fullscreen,
            no_build,
            stats_every,
            extra,
        } => start(
            &backend,
            &display,
            &workspace,
            !no_fullscreen,
            !no_build,
            stats_every,
            &extra,
        ),
        Cmd::Stop => stop(),
        Cmd::Status => status(),
        Cmd::Stats { watch } => {
            loop {
                println!("{}", ctl("stats")?);
                let Some(every) = watch else { break };
                std::thread::sleep(every);
            }
            Ok(())
        }
        Cmd::State => {
            println!("{}", ctl("state")?);
            Ok(())
        }
        Cmd::Set { flag, value } => {
            println!("{}", ctl(&format!("set {flag} {value}"))?);
            Ok(())
        }
        Cmd::Snap { out, show } => snap(&out, show),
        Cmd::Log { lines } => {
            let text = std::fs::read_to_string(log_path())
                .with_context(|| format!("read {}", log_path().display()))?;
            let all: Vec<&str> = text.lines().collect();
            for l in &all[all.len().saturating_sub(lines)..] {
                println!("{l}");
            }
            Ok(())
        }
        Cmd::Raw { line } => {
            println!("{}", ctl(&line.join(" "))?);
            Ok(())
        }
    }
}

fn temp_dir() -> PathBuf {
    Path::new(PROJECT_DIR).join("temp")
}

fn log_path() -> PathBuf {
    temp_dir().join("waygriff.log")
}

fn pid_path() -> PathBuf {
    temp_dir().join("waygriff.pid")
}

fn ctl_connect() -> Result<UnixStream> {
    let addr = SocketAddr::from_abstract_name(CTL_SOCKET)?;
    UnixStream::connect_addr(&addr)
        .with_context(|| format!("connect @{CTL_SOCKET}; is waygriff running?"))
}

fn ctl(line: &str) -> Result<String> {
    let mut stream = ctl_connect()?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    writeln!(stream, "{line}")?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    Ok(reply.trim_end().to_owned())
}

fn read_pid() -> Option<u32> {
    std::fs::read_to_string(pid_path())
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

fn hyprland_signature() -> Result<String> {
    if let Ok(sig) = std::env::var("HYPRLAND_INSTANCE_SIGNATURE") {
        return Ok(sig);
    }
    let runtime = std::env::var("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR unset")?;
    let dir = Path::new(&runtime).join("hypr");
    let mut live: Vec<PathBuf> = std::fs::read_dir(&dir)
        .with_context(|| format!("list {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join(".socket.sock").exists())
        .collect();
    live.sort();
    let sig = live
        .pop()
        .context("no Hyprland instance under $XDG_RUNTIME_DIR/hypr")?;
    Ok(sig.file_name().unwrap().to_string_lossy().into_owned())
}

fn hyprctl(args: &[&str]) -> Result<String> {
    let sig = hyprland_signature()?;
    let out = Command::new("hyprctl")
        .env("HYPRLAND_INSTANCE_SIGNATURE", &sig)
        .args(args)
        .output()
        .context("run hyprctl")?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if !out.status.success() {
        bail!(
            "hyprctl {:?}: {}{}",
            args,
            text,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(text)
}

fn lua_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn start(
    backend: &str,
    display: &str,
    workspace: &str,
    fullscreen: bool,
    build: bool,
    stats_every: Duration,
    extra: &[String],
) -> Result<()> {
    if ctl_connect().is_ok() {
        bail!("waygriff already answering on @{CTL_SOCKET}; `waygriffctl stop` first");
    }
    if build {
        let status = Command::new("cargo")
            .current_dir(PROJECT_DIR)
            .args(["build", "--release", "--bin", "waygriff"])
            .status()
            .context("run cargo")?;
        if !status.success() {
            bail!("cargo build failed");
        }
    }
    std::fs::create_dir_all(temp_dir())?;
    let bin = Path::new(PROJECT_DIR).join("target/release/waygriff");
    if !bin.exists() {
        bail!("{} missing; build first", bin.display());
    }

    // Hyprland spawns the process with its own environment, so the wrapper
    // carries over what the dev shell provides (driver libs via LD_LIBRARY_PATH).
    let mut args = vec![
        format!("--backend={backend}"),
        format!("--display={display}"),
        format!("--stats-every={}", humantime::format_duration(stats_every)),
    ];
    args.extend(extra.iter().cloned());
    let quoted_args: Vec<String> = args.iter().map(|a| shell_quote(a)).collect();
    let ld = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
    let wrapper = format!(
        "#!/usr/bin/env sh\n\
         echo $$ > {pid}\n\
         export LD_LIBRARY_PATH={ld}\n\
         cd {project}\n\
         exec {bin} {args} >> {log} 2>&1\n",
        pid = shell_quote(&pid_path().to_string_lossy()),
        ld = shell_quote(&ld),
        project = shell_quote(PROJECT_DIR),
        bin = shell_quote(&bin.to_string_lossy()),
        args = quoted_args.join(" "),
        log = shell_quote(&log_path().to_string_lossy()),
    );
    let wrapper_path = temp_dir().join("waygriff-launch.sh");
    std::fs::write(&wrapper_path, wrapper)?;
    std::fs::set_permissions(&wrapper_path, std::fs::Permissions::from_mode(0o755))?;
    let _ = std::fs::remove_file(pid_path());
    writeln!(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path())?,
        "===== waygriffctl start {} =====",
        humantime::format_rfc3339_seconds(std::time::SystemTime::now())
    )?;

    let mut rules = vec![format!(
        "workspace = {}",
        lua_string(&format!("name:{workspace} silent"))
    )];
    if fullscreen {
        rules.push("fullscreen = true".into());
    }
    let lua = format!(
        "hl.exec_cmd({}, {{ {} }})",
        lua_string(&wrapper_path.to_string_lossy()),
        rules.join(", ")
    );
    let reply = hyprctl(&["eval", &lua])?;
    if reply != "ok" {
        bail!("hyprctl eval: {reply}");
    }

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if ctl_connect().is_ok() {
            break;
        }
        if let Some(pid) = read_pid()
            && !pid_alive(pid)
        {
            eprintln!("waygriff exited during startup; log tail:");
            print_log_tail(20);
            bail!("startup failed");
        }
        if Instant::now() > deadline {
            print_log_tail(20);
            bail!("control socket did not come up within 20s");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    println!(
        "started pid {} on workspace {workspace}; log: {}",
        read_pid().unwrap_or(0),
        log_path().display()
    );
    std::thread::sleep(Duration::from_secs(2));
    println!("{}", ctl("stats")?);
    Ok(())
}

fn print_log_tail(n: usize) {
    if let Ok(text) = std::fs::read_to_string(log_path()) {
        let all: Vec<&str> = text.lines().collect();
        for l in &all[all.len().saturating_sub(n)..] {
            eprintln!("{l}");
        }
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn stop() -> Result<()> {
    let pid = read_pid();
    match ctl("quit") {
        Ok(reply) => println!("{reply}"),
        Err(e) => eprintln!("{e:#}"),
    }
    let Some(pid) = pid else {
        return Ok(());
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while pid_alive(pid) {
        if Instant::now() > deadline {
            eprintln!("pid {pid} still alive, sending SIGTERM");
            let _ = Command::new("kill").arg(pid.to_string()).status();
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    println!("stopped");
    Ok(())
}

fn status() -> Result<()> {
    let pid = read_pid();
    let alive = pid.is_some_and(pid_alive);
    match ctl("state") {
        Ok(reply) => println!("running pid={} {reply}", pid.unwrap_or(0)),
        Err(_) if alive => println!(
            "pid {} alive but control socket not answering",
            pid.unwrap()
        ),
        Err(_) => println!("not running"),
    }
    Ok(())
}

fn read_u32_le(stream: &mut impl std::io::Read) -> Result<u32> {
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

fn snap(out: &Path, show: bool) -> Result<()> {
    let addr = SocketAddr::from_abstract_name(SNAP_SOCKET)?;
    let mut stream =
        UnixStream::connect_addr(&addr).with_context(|| format!("connect @{SNAP_SOCKET}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let width = read_u32_le(&mut stream)?;
    let height = read_u32_le(&mut stream)?;
    let stride = read_u32_le(&mut stream)?;
    let mut bgra = vec![0u8; stride as usize * height as usize];
    stream.read_exact(&mut bgra)?;

    let row_bytes = width as usize * 4;
    let mut rgba = Vec::with_capacity(row_bytes * height as usize);
    for row in bgra.chunks_exact(stride as usize) {
        for px in row[..row_bytes].as_chunks::<4>().0 {
            rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
    }
    let img = image::RgbaImage::from_raw(width, height, rgba).context("bad image dimensions")?;
    let out = if out.is_absolute() {
        out.to_path_buf()
    } else {
        Path::new(PROJECT_DIR).join(out)
    };
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    img.save(&out)
        .with_context(|| format!("write {}", out.display()))?;
    println!("{width}x{height} -> {}", out.display());
    if show {
        viuer::print(
            &image::DynamicImage::ImageRgba8(img),
            &viuer::Config::default(),
        )?;
    }
    Ok(())
}
