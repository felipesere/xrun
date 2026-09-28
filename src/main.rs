//! xrun: run one command in every subdirectory of the current directory
//! and show the results as a table.

use std::fs;
use std::io::{BufRead, BufReader, IsTerminal, Read};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use comfy_table::presets::UTF8_NO_BORDERS;
use comfy_table::{Cell, Color, ColumnConstraint, ContentArrangement, Table};
use owo_colors::OwoColorize;

/// How long to keep reading output after the command itself has exited.
/// Covers stray grandchildren that inherited the pipes and never closed them.
const DRAIN_GRACE: Duration = Duration::from_millis(500);

/// Process groups of jobs currently running in table mode, so Ctrl-C can kill them.
static LIVE_GROUPS: Mutex<Vec<i32>> = Mutex::new(Vec::new());

/// Run a command in every subdirectory of the current directory and show the results as a table.
///
/// No config file and no VCS awareness: xrun iterates over the folders in the
/// current directory, runs the command in each, and renders one row per folder.
///
/// A single COMMAND argument is run through `sh -c`, so pipes, redirects and
/// variables work. Several arguments are run directly, which keeps quoting
/// simple. The directory name is exported as $XRUN_DIR. Stdin is closed;
/// redirect from a file if the command needs input.
///
/// Stdout and stderr are merged into the Output column with ANSI codes
/// stripped. The Exit column shows the exit code, or `timeout`, `signal`, or
/// `error` when the program could not be started. xrun exits 1 if any
/// directory failed.
///
/// Each job runs in its own process group. A timeout or Ctrl-C kills the whole
/// tree, including anything the shell or the command forked.
#[derive(Parser, Debug)]
#[command(version, about, arg_required_else_help = true, verbatim_doc_comment, after_long_help = RECIPES)]
struct Args {
    /// Maximum number of commands to run at once. Omit (or pass 0) to run all at once.
    ///
    /// Running everything at once is fine for `jj st` across thirty repos, but
    /// not for anything that hits the network or an API quota. Rows always come
    /// out in directory order however the jobs finish.
    ///
    ///     xrun -j 4 -- jj git fetch                      # be polite to the remote
    ///     xrun -j 2 -- 'claude -p "..." < ../prompt.md'  # two Claude sessions at a time
    ///     xrun -j 1 -- cargo build                       # sequential
    #[arg(short = 'j', long, value_name = "N", verbatim_doc_comment)]
    jobs: Option<usize>,

    /// Directory names to skip. Comma separated, may be repeated.
    ///
    /// Matches directory names exactly.
    ///
    ///     xrun --ignore archive,scratch -- jj st
    ///     xrun --ignore archive --ignore scratch -- jj st
    #[arg(long, value_delimiter = ',', value_name = "NAME,...", verbatim_doc_comment, conflicts_with = "only")]
    ignore: Vec<String>,

    /// Directory names to run in, skipping every other directory. Comma separated, may be repeated.
    ///
    /// Matches directory names exactly. A named directory is included even if
    /// it is hidden. Names that do not match a directory produce a warning.
    /// Cannot be combined with --ignore.
    ///
    ///     xrun --only api,web -- cargo test
    ///     xrun --only api --only web -- cargo test
    #[arg(long, value_delimiter = ',', value_name = "NAME,...", verbatim_doc_comment)]
    only: Vec<String>,

    /// Include directories whose name starts with a dot.
    ///
    ///     xrun --hidden -- du -sh .
    #[arg(long, verbatim_doc_comment)]
    hidden: bool,

    /// Print every line of output to stderr as it arrives, prefixed with the directory.
    ///
    /// Table mode buffers everything until the last job is done. For anything
    /// that runs longer than a few seconds, --follow shows progress as it
    /// happens. The table is still printed at the end.
    ///
    ///     xrun -f -- cargo test
    ///     xrun -f -j 2 -t 15m -- 'claude -p --permission-mode acceptEdits < ../prompt.md'
    ///
    ///     api   │    Compiling api v0.3.0
    ///     web   │    Compiling web v1.2.0
    ///     api   │ test result: ok. 41 passed
    ///     web   │ test result: FAILED. 1 failed
    #[arg(short = 'f', long, verbatim_doc_comment)]
    follow: bool,

    /// Kill a command that runs longer than this. Accepts 90, 90s, 10m, 1h.
    ///
    /// The whole process tree is killed and the row is marked `timeout`.
    /// Headless Claude can wait up to ten minutes for background work it
    /// started, so a timeout is the backstop that guarantees you get the
    /// table back.
    ///
    ///     xrun -t 30s -- jj git fetch     # a hung remote should not block the table
    ///     xrun -j 2 -t 15m -- 'claude -p --permission-mode acceptEdits < ../prompt.md'
    #[arg(short = 't', long, value_name = "DURATION", value_parser = parse_duration, verbatim_doc_comment)]
    timeout: Option<Duration>,

    /// Do not print start/finish progress lines to stderr.
    ///
    /// When stderr is a terminal, xrun prints `▶ name` when a job starts and
    /// `✓ name 12.3s` or `✗ name 1 12.3s` when it ends. Use -q when capturing
    /// stderr in a script.
    ///
    ///     xrun -q -- jj st 2> errors.log
    #[arg(short = 'q', long, verbatim_doc_comment)]
    quiet: bool,

    /// Run a single-argument command directly instead of through `sh -c`.
    ///
    /// Only matters when the command is one argument that must not be
    /// interpreted, such as a program whose path contains a space.
    ///
    ///     xrun -n -- '/Applications/Some Tool.app/Contents/MacOS/tool'
    #[arg(short = 'n', long, verbatim_doc_comment)]
    no_shell: bool,

    /// Run sequentially and pass output straight through instead of building a table.
    ///
    /// One directory at a time with your own stdin, stdout and stderr. Use it
    /// for interactive tools, or when you want raw, unbuffered, unstripped
    /// output.
    ///
    ///     xrun --stream -- claude               # interactive session per repo
    ///     xrun --stream -- jj log --limit 5     # colours preserved
    #[arg(long, verbatim_doc_comment)]
    stream: bool,

    /// Hide rows that produced no output and exited zero.
    ///
    /// Ideal for status checks where most directories are quiet. If nothing is
    /// left, xrun prints a one-line note instead of an empty table.
    ///
    ///     xrun --omit-empty -- 'grep -l TODO README.md'
    ///     xrun --omit-empty -- 'jj log -r "trunk()..@ & ~empty()" --no-graph -T "change_id.short() ++ \"\n\""'
    #[arg(long, verbatim_doc_comment)]
    omit_empty: bool,

    /// Command to run in each directory. Put `--` before it if it has flags of its own.
    ///
    /// One argument is run through `sh -c`; several are run directly.
    ///
    ///     xrun -- 'jj log -r "trunk()..@" | head -3'
    ///     xrun -- claude -p "summarise this repo"
    ///     xrun -- 'echo "$XRUN_DIR: $(jj log -r @ --no-graph -T "change_id.short()")"'
    #[arg(required = true, trailing_var_arg = true, num_args = 1.., value_name = "COMMAND", verbatim_doc_comment)]
    cmd: Vec<String>,
}

const RECIPES: &str = "\
Recipes:
  # Which repos have unpushed work?
  xrun --omit-empty -- 'jj log -r \"trunk()..@ & ~empty()\" --no-graph -T \"change_id.short() ++ \\\"\\n\\\"\"'

  # Fetch everything, four at a time, give up on stragglers
  xrun -j 4 -t 1m -- jj git fetch

  # Apply a prompt to every repo with Claude, watching progress, bounded to 15 minutes each
  xrun -j 2 -f -t 15m -- 'claude -p --permission-mode acceptEdits \\
    --allowedTools \"Bash(yq *)\" \"Bash(grep *)\" \"Bash(jj diff *)\" < ../prompt.md'

  # Name the directory in the output
  xrun -- 'echo \"$XRUN_DIR: $(jj log -r @ --no-graph -T \"change_id.short()\")\"'

Short help: -h    Long help with examples: --help
";

fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3600),
        _ => (s, 1),
    };
    num.parse::<u64>()
        .map(|n| Duration::from_secs(n * mult))
        .map_err(|_| format!("invalid duration `{s}`, use e.g. 90, 90s, 10m, 1h"))
}

enum Status {
    Exit(i32),
    Signal,
    TimedOut,
    /// The command could not be started at all (not found, not executable, ...).
    Failed,
}

impl Status {
    fn ok(&self) -> bool {
        matches!(self, Status::Exit(0))
    }

    fn label(&self) -> String {
        match self {
            Status::Exit(code) => code.to_string(),
            Status::Signal => "signal".to_string(),
            Status::TimedOut => "timeout".to_string(),
            Status::Failed => "error".to_string(),
        }
    }

    fn from_exit(status: ExitStatus) -> Self {
        match status.code() {
            Some(code) => Status::Exit(code),
            None => Status::Signal,
        }
    }
}

struct Outcome {
    dir: String,
    output: String,
    status: Status,
    elapsed: Duration,
}

/// Everything a worker needs besides the directory itself.
struct RunOpts<'a> {
    cmd: &'a [String],
    no_shell: bool,
    follow: bool,
    timeout: Option<Duration>,
    progress: bool,
    /// Width to pad directory names to in progress and follow output.
    name_width: usize,
}

fn main() -> ExitCode {
    let args = Args::parse();

    let dirs = match discover(&args) {
        Ok(dirs) => dirs,
        Err(err) => {
            eprintln!("xrun: cannot read current directory: {err}");
            return ExitCode::from(2);
        }
    };

    if dirs.is_empty() {
        eprintln!("xrun: no directories to run in");
        return ExitCode::SUCCESS;
    }

    let failed = if args.stream {
        run_stream(&args, &dirs)
    } else {
        let outcomes = run_parallel(&args, &dirs);
        print_table(&outcomes, args.omit_empty);
        outcomes.iter().filter(|o| !o.status.ok()).count()
    };

    if failed > 0 {
        eprintln!("{}", format!("{failed} of {} failed", dirs.len()).red());
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

/// Immediate subdirectories of the current directory, sorted by name.
///
/// With `--only`, exactly the named directories are returned (hidden or not)
/// and a warning is printed for any name that does not exist. Otherwise every
/// directory is returned except hidden ones (unless `--hidden`) and those
/// listed in `--ignore`.
fn discover(args: &Args) -> std::io::Result<Vec<PathBuf>> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(".")?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter(|path| {
            let name = dir_name(path);
            if !args.only.is_empty() {
                return args.only.contains(&name);
            }
            let hidden = name.starts_with('.');
            (args.hidden || !hidden) && !args.ignore.contains(&name)
        })
        .collect();
    dirs.sort();

    for name in &args.only {
        if !dirs.iter().any(|d| dir_name(d) == *name) {
            eprintln!("{}", format!("xrun: --only {name}: no such directory").yellow());
        }
    }

    Ok(dirs)
}

fn dir_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Run the command in every directory with at most `jobs` running at once.
/// Results come back in directory order regardless of completion order.
fn run_parallel(args: &Args, dirs: &[PathBuf]) -> Vec<Outcome> {
    let workers = match args.jobs {
        None | Some(0) => dirs.len(),
        Some(n) => n.min(dirs.len()),
    };

    install_ctrlc_handler();

    let opts = RunOpts {
        cmd: &args.cmd,
        no_shell: args.no_shell,
        follow: args.follow,
        timeout: args.timeout,
        // Progress lines only make sense for a human watching stderr.
        progress: !args.quiet && std::io::stderr().is_terminal(),
        name_width: dirs.iter().map(|d| dir_name(d).len()).max().unwrap_or(0),
    };

    if opts.progress {
        eprintln!(
            "{}",
            format!("running in {} directories, {} at a time", dirs.len(), workers).dimmed()
        );
    }

    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel();

    thread::scope(|scope| {
        for _ in 0..workers {
            let tx = tx.clone();
            let next = &next;
            let opts = &opts;
            scope.spawn(move || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= dirs.len() {
                    break;
                }
                let outcome = run_captured(opts, &dirs[i]);
                if tx.send((i, outcome)).is_err() {
                    break;
                }
            });
        }
        drop(tx);
    });

    let mut slots: Vec<Option<Outcome>> = (0..dirs.len()).map(|_| None).collect();
    for (i, outcome) in rx {
        slots[i] = Some(outcome);
    }
    slots.into_iter().flatten().collect()
}

/// Run the command in one directory, capturing stdout and stderr into a single string.
///
/// The wait is keyed on the child exiting, not on the pipes closing, so a grandchild
/// that keeps the pipes open cannot hang us. Reader threads are left to die with the
/// process if that happens.
fn run_captured(opts: &RunOpts, dir: &Path) -> Outcome {
    let name = dir_name(dir);
    let start = Instant::now();

    if opts.progress {
        eprintln!("{} {:<w$}", "▶".dimmed(), name.dimmed(), w = opts.name_width);
    }

    let mut command = build_command(opts.cmd, opts.no_shell, dir);
    let program = command.get_program().to_string_lossy().into_owned();
    let spawned = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Own process group, so a timeout or Ctrl-C can take out the whole tree,
        // including anything a shell or the command itself forked.
        .process_group(0)
        .spawn();

    let mut child = match spawned {
        Ok(child) => child,
        Err(err) => {
            let outcome = Outcome {
                dir: name,
                output: format!("{program}: {err}"),
                status: Status::Failed,
                elapsed: start.elapsed(),
            };
            report_finished(opts, &outcome);
            return outcome;
        }
    };

    let (line_tx, line_rx) = mpsc::channel::<String>();
    if let Some(stdout) = child.stdout.take() {
        spawn_reader(stdout, line_tx.clone(), opts.follow, name.clone(), opts.name_width);
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_reader(stderr, line_tx.clone(), opts.follow, name.clone(), opts.name_width);
    }
    drop(line_tx);

    let pgid = child.id() as i32;
    track_group(pgid, true);
    let status = wait_with_timeout(&mut child, opts.timeout, start);
    track_group(pgid, false);

    // Collect whatever the readers have. Once the child has exited, the pipes normally
    // close right away and the channel disconnects. If something is still holding them,
    // stop after a short grace period.
    let mut lines = Vec::new();
    let deadline = Instant::now() + DRAIN_GRACE;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match line_rx.recv_timeout(remaining) {
            Ok(line) => lines.push(line),
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                if !matches!(status, Status::TimedOut) {
                    lines.push("[xrun: output still open after exit, truncated]".to_string());
                }
                break;
            }
        }
    }

    let outcome = Outcome {
        dir: name,
        output: lines.join("\n").trim_end().to_string(),
        status,
        elapsed: start.elapsed(),
    };
    report_finished(opts, &outcome);
    outcome
}

/// Poll the child until it exits or the timeout passes. On timeout the child is killed.
fn wait_with_timeout(child: &mut Child, timeout: Option<Duration>, start: Instant) -> Status {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Status::from_exit(status),
            Ok(None) => {}
            Err(_) => return Status::Failed,
        }
        if let Some(limit) = timeout
            && start.elapsed() >= limit
        {
            kill_group(child.id() as i32);
            let _ = child.wait();
            return Status::TimedOut;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Read a pipe line by line on its own thread, forwarding each line to `tx`
/// and, with `follow`, echoing it to stderr with the directory as prefix.
fn spawn_reader<R: Read + Send + 'static>(
    pipe: R,
    tx: mpsc::Sender<String>,
    follow: bool,
    name: String,
    name_width: usize,
) {
    thread::spawn(move || {
        let reader = BufReader::new(pipe);
        for line in reader.split(b'\n') {
            let Ok(raw) = line else { break };
            let clean = strip_ansi_escapes::strip(&raw);
            let text = String::from_utf8_lossy(&clean).trim_end().to_string();
            if follow {
                eprintln!("{:<w$} │ {}", name.cyan(), text, w = name_width);
            }
            if tx.send(text).is_err() {
                break;
            }
        }
    });
}

fn report_finished(opts: &RunOpts, outcome: &Outcome) {
    if !opts.progress {
        return;
    }
    let secs = format!("{:.1}s", outcome.elapsed.as_secs_f64());
    if outcome.status.ok() {
        eprintln!(
            "{} {:<w$} {}",
            "✓".green(),
            outcome.dir,
            secs.dimmed(),
            w = opts.name_width
        );
    } else {
        eprintln!(
            "{} {:<w$} {} {}",
            "✗".red(),
            outcome.dir,
            outcome.status.label().red(),
            secs.dimmed(),
            w = opts.name_width
        );
    }
}

/// Run the command in every directory one after another, sharing our stdin/stdout/stderr.
/// Returns the number of directories where the command did not exit zero.
fn run_stream(args: &Args, dirs: &[PathBuf]) -> usize {
    let mut failed = 0;
    for (i, dir) in dirs.iter().enumerate() {
        if i > 0 {
            println!();
        }
        println!("{}", format!("== {}", dir_name(dir)).bold());
        let mut command = build_command(&args.cmd, args.no_shell, dir);
        let program = command.get_program().to_string_lossy().into_owned();
        match command.status() {
            Ok(status) if status.success() => {}
            Ok(status) => {
                failed += 1;
                eprintln!("{}", format!("   exit {}", status.code().unwrap_or(-1)).red());
            }
            Err(err) => {
                failed += 1;
                eprintln!("{}", format!("   {program}: {err}").red());
            }
        }
    }
    failed
}

/// A single argument is a shell command line; several arguments are a program and its args.
fn build_command(cmd: &[String], no_shell: bool, dir: &Path) -> Command {
    let mut command = if cmd.len() == 1 && !no_shell {
        let mut command = Command::new("sh");
        command.arg("-c").arg(&cmd[0]);
        command
    } else {
        let mut command = Command::new(&cmd[0]);
        command.args(&cmd[1..]);
        command
    };
    command.current_dir(dir).env("XRUN_DIR", dir_name(dir));
    command
}

fn kill_group(pgid: i32) {
    // SAFETY: killpg has no memory-safety preconditions; an invalid pgid just returns an error.
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
}

fn track_group(pgid: i32, live: bool) {
    let mut groups = LIVE_GROUPS.lock().unwrap_or_else(|e| e.into_inner());
    if live {
        groups.push(pgid);
    } else {
        groups.retain(|&g| g != pgid);
    }
}

/// Children run in their own process groups, so the terminal's Ctrl-C does not reach
/// them. Forward it by hand, then exit with the conventional 130.
fn install_ctrlc_handler() {
    let result = ctrlc::set_handler(|| {
        let groups = LIVE_GROUPS.lock().unwrap_or_else(|e| e.into_inner());
        for &pgid in groups.iter() {
            kill_group(pgid);
        }
        eprintln!("\n{}", format!("interrupted, killed {} running job(s)", groups.len()).red());
        std::process::exit(130);
    });
    if let Err(err) = result {
        eprintln!("xrun: could not install Ctrl-C handler: {err}");
    }
}

fn print_table(outcomes: &[Outcome], omit_empty: bool) {
    let mut table = Table::new();
    table
        .load_style(UTF8_NO_BORDERS)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(["Dir", "Output", "Exit"]);

    // Terminal width detection can come back tiny or absent (pipes, odd ptys).
    // Fall back to a sane fixed width rather than wrapping every cell to one character.
    match table.width() {
        Some(width) if width >= 40 => {}
        _ => {
            table.set_width(120);
        }
    }

    // Let the Output column absorb all wrapping; keep Dir and Exit at their natural width.
    if let Some(col) = table.column_mut(0) {
        col.set_constraint(ColumnConstraint::ContentWidth);
    }
    if let Some(col) = table.column_mut(2) {
        col.set_constraint(ColumnConstraint::ContentWidth);
    }

    let shown: Vec<&Outcome> = outcomes
        .iter()
        .filter(|o| !(omit_empty && o.status.ok() && o.output.is_empty()))
        .collect();

    if shown.is_empty() {
        eprintln!(
            "all {} directories exited zero with no output",
            outcomes.len()
        );
        return;
    }

    for outcome in shown {
        let color = if outcome.status.ok() { Color::Green } else { Color::Red };
        table.add_row(vec![
            Cell::new(&outcome.dir),
            Cell::new(&outcome.output),
            Cell::new(outcome.status.label()).fg(color),
        ]);
    }

    println!("{table}");
}
