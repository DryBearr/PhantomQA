//! PhantomQA: an AI agent tests an Android app on an emulator and reports bugs.
//!
//! `phantomqa mcp` runs an MCP server over stdio (for Claude Code and other agents).
//! `phantomqa screen | tap N | type N TEXT | back | launch PACKAGE | crashes` does the same by hand.
//!
//! Everything goes through `adb`. Run `phantomqa doctor` to check the setup.
//! Bug reports go to ./reports/run-NNN/. See `phantomqa --help` for the paths you can set.

use jiff::{Timestamp, tz::TimeZone};
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, mpsc};
use std::thread::{JoinHandle, sleep};
use std::time::{Duration, Instant};

/// How long to wait after an action before reading the screen again.
const SETTLE: Duration = Duration::from_millis(500);
/// An adb command that runs longer than this is killed.
const ADB_TIMEOUT: Duration = Duration::from_secs(30);
/// Longer labels are cut, to keep the screen list small.
const MAX_LABEL: usize = 80;
/// Log lines kept per crash.
const MAX_CRASH_LINES: usize = 40;
/// Recent actions kept for the "how did we get here" part of a bug.
const MAX_HISTORY: usize = 15;
const SEVERITIES: [&str; 4] = ["critical", "high", "medium", "low"];

/// One thing on the screen the agent can refer to by number.
#[derive(Debug, Clone, PartialEq)]
struct Element {
    kind: String,
    label: String,
    rid: String,
    flags: Vec<&'static str>,
    center: (i32, i32),
}

/// Everything the server remembers between tool calls.
struct State {
    /// Elements from the last screen read. Numbers in `tap N` index into this list.
    elements: Vec<Element>,
    /// App package of the last screen read.
    package: String,
    /// Recent actions, newest last.
    history: Vec<String>,
    /// Logcat time ("sssss.mmm") to read crashes from. None until the first check.
    crash_since: Option<String>,
    report_root: PathBuf,
    /// Created on the first bug, or when video starts.
    report: Option<Report>,
    /// True once the first tool call decided about video.
    video_checked: bool,
    /// The running recording, if video is on.
    video: Option<Video>,
    /// Why there is no video, for the report.
    video_off: Option<String>,
    /// Apps started with launch_app. Crashes of other apps are not saved as bugs.
    apps: Vec<String>,
    /// Crashes already saved, by title, with their bug number. A crash loop is saved once.
    saved_crashes: Vec<(String, usize)>,
    /// The crash check failed once already; do not repeat the same warning after every action.
    crash_check_failed: bool,
    /// The run folder `phantomqa run` made for this session, instead of a new one.
    run_dir: Option<PathBuf>,
    /// Button texts from the test file that tap and type_text refuse.
    avoid: Vec<String>,
    /// Env var with the test account password, for type_password.
    password_env: Option<String>,
}

struct Report {
    dir: PathBuf,
    run: usize,
    device: String,
    /// Number of bugs saved.
    bugs: usize,
    /// One JSON entry per bug, for run.json.
    bug_list: Vec<Value>,
    finished: bool,
    session_video: Option<String>,
    /// Held while the session runs. The API sees a free lock on an unfinished run as "interrupted".
    lock: Option<fs::File>,
    /// The test file's settings when `phantomqa run` started the run, else null.
    test: Value,
}

impl State {
    fn new(report_root: PathBuf) -> Self {
        State {
            elements: Vec::new(),
            package: String::new(),
            history: Vec::new(),
            crash_since: None,
            report_root,
            report: None,
            video_checked: false,
            video: None,
            video_off: None,
            apps: Vec::new(),
            saved_crashes: Vec::new(),
            crash_check_failed: false,
            run_dir: None,
            avoid: Vec::new(),
            password_env: None,
        }
    }

    fn remember(&mut self, action: String) {
        self.history.push(action);
        if self.history.len() > MAX_HISTORY {
            self.history.remove(0);
        }
    }
}

// ---------------------------------------------------------------- settings

/// Paths and switches. Each comes from a flag, then an env var, then a default.
#[derive(Debug, PartialEq)]
struct Config {
    adb: String,
    /// None when ffmpeg is not set and not on PATH.
    ffmpeg: Option<String>,
    reports: PathBuf,
    video: bool,
    screenshots: bool,
    /// None means adb picks the device (it reads ANDROID_SERIAL itself).
    serial: Option<String>,
    /// tokens.json: the hashed API tokens.
    tokens: PathBuf,
    /// Claude Code, the agent `phantomqa run` starts.
    claude: String,
    /// routines.json: the tests `serve` runs on a schedule.
    routines: PathBuf,
}

static CONFIG: OnceLock<Config> = OnceLock::new();

fn config() -> &'static Config {
    CONFIG.get_or_init(|| load_config(&[], &env_var).expect("no flags to parse").0)
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Reads the options before the command. Returns the config and where the command starts.
fn load_config(args: &[String], env: &dyn Fn(&str) -> Option<String>) -> Result<(Config, usize), String> {
    let (mut adb, mut ffmpeg, mut reports, mut serial, mut tokens, mut claude, mut routines) =
        (None, None, None, None, None, None, None);
    let (mut no_video, mut no_screenshots) = (false, false);
    let mut i = 0;
    while let Some(arg) = args.get(i).filter(|a| a.starts_with("--")) {
        i += 1;
        // "--" ends the options.
        if arg == "--" {
            break;
        }
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        match name {
            "--no-video" | "--no-screenshots" if inline.is_some() => return Err(format!("{name} takes no value")),
            "--no-video" => {
                no_video = true;
                continue;
            }
            "--no-screenshots" => {
                no_screenshots = true;
                continue;
            }
            _ => {}
        }
        let slot = match name {
            "--adb" => &mut adb,
            "--ffmpeg" => &mut ffmpeg,
            "--reports" => &mut reports,
            "--serial" => &mut serial,
            "--tokens" => &mut tokens,
            "--claude" => &mut claude,
            "--routines" => &mut routines,
            _ => return Err(format!("unknown option: {name}")),
        };
        let value = match inline {
            Some(value) => value,
            None => {
                i += 1;
                // "--reports --no-video" is a missing value, not a folder named "--no-video".
                args.get(i - 1)
                    .filter(|v| !v.starts_with("--"))
                    .cloned()
                    .unwrap_or_default()
            }
        };
        if value.trim().is_empty() {
            return Err(format!("{name} needs a value"));
        }
        *slot = Some(value);
    }

    let config = Config {
        adb: adb.or_else(|| env("ADB")).unwrap_or_else(|| find_adb(env)),
        ffmpeg: ffmpeg
            .or_else(|| env("FFMPEG"))
            .or_else(|| runs("ffmpeg").then(|| "ffmpeg".to_string())),
        // Absolute, so messages and links stay right whatever the working folder is later.
        reports: {
            let reports = PathBuf::from(
                reports
                    .or_else(|| env("PHANTOMQA_REPORTS"))
                    .unwrap_or_else(|| "reports".into()),
            );
            std::path::absolute(&reports).unwrap_or(reports)
        },
        video: !no_video && env("PHANTOMQA_VIDEO").is_none_or(|v| v != "0"),
        screenshots: !no_screenshots && env("PHANTOMQA_SCREENSHOTS").is_none_or(|v| v != "0"),
        serial,
        tokens: {
            let tokens = PathBuf::from(
                tokens
                    .or_else(|| env("PHANTOMQA_TOKENS"))
                    .unwrap_or_else(|| "tokens.json".into()),
            );
            std::path::absolute(&tokens).unwrap_or(tokens)
        },
        claude: claude
            .or_else(|| env("PHANTOMQA_CLAUDE"))
            .unwrap_or_else(|| "claude".into()),
        routines: {
            let routines = PathBuf::from(
                routines
                    .or_else(|| env("PHANTOMQA_ROUTINES"))
                    .unwrap_or_else(|| "routines.json".into()),
            );
            std::path::absolute(&routines).unwrap_or(routines)
        },
    };
    Ok((config, i))
}

/// adb from the Android SDK if ANDROID_HOME or ANDROID_SDK_ROOT is set, else adb on PATH.
fn find_adb(env: &dyn Fn(&str) -> Option<String>) -> String {
    let exe = if cfg!(windows) { "adb.exe" } else { "adb" };
    for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(home) = env(var) {
            let path = Path::new(&home).join("platform-tools").join(exe);
            if path.is_file() {
                return path.to_string_lossy().into_owned();
            }
        }
    }
    "adb".into()
}

/// True if the program starts and exits cleanly with `-version`.
fn runs(program: &str) -> bool {
    first_line(program, "-version").is_ok()
}

/// First output line of `program arg`.
fn first_line(program: &str, arg: &str) -> Result<String, String> {
    let out = Command::new(program)
        .arg(arg)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{program} {arg} failed"));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string())
}

// ---------------------------------------------------------------- adb

/// Runs adb with a timeout and returns its stdout as text.
fn adb(args: &[&str]) -> Result<String, String> {
    adb_bytes(args).map(|out| String::from_utf8_lossy(&out).into_owned())
}

/// An adb command for the chosen device.
fn adb_command() -> Command {
    let config = config();
    let mut command = Command::new(&config.adb);
    if let Some(serial) = &config.serial {
        command.args(["-s", serial]);
    }
    command
}

fn adb_missing(error: std::io::Error) -> String {
    format!(
        "cannot run adb ({}): {error}. Install Android platform-tools, or set --adb, ADB or ANDROID_HOME.",
        config().adb
    )
}

/// Waits for a process and kills it after `timeout`.
fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Result<ExitStatus, String> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timed out after {}s", timeout.as_secs()));
            }
            Ok(None) => sleep(Duration::from_millis(10)),
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// Runs adb with the usual timeout and returns its raw stdout.
fn adb_bytes(args: &[&str]) -> Result<Vec<u8>, String> {
    adb_bytes_for(args, ADB_TIMEOUT)
}

/// Runs adb with a timeout and returns its raw stdout.
fn adb_bytes_for(args: &[&str], timeout: Duration) -> Result<Vec<u8>, String> {
    let mut child = adb_command()
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(adb_missing)?;

    // Read both pipes on threads, or a big screen dump fills the pipe and adb blocks.
    let mut out_pipe = child.stdout.take().expect("stdout is piped");
    let mut err_pipe = child.stderr.take().expect("stderr is piped");
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out_pipe.read_to_end(&mut buf);
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err_pipe.read_to_end(&mut buf);
        buf
    });

    let status = wait_with_timeout(&mut child, timeout).map_err(|e| format!("adb {}: {e}", args.join(" ")))?;

    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    if !status.success() {
        let detail = format!(
            "{} {}",
            String::from_utf8_lossy(&stderr).trim(),
            String::from_utf8_lossy(&stdout).trim()
        );
        return Err(format!("adb {} failed: {}", args.join(" "), detail.trim()));
    }
    Ok(stdout)
}

fn valid_package(package: &str) -> bool {
    !package.is_empty()
        && package
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
}

fn screenshot() -> Result<Vec<u8>, String> {
    let png = adb_bytes(&["exec-out", "screencap", "-p"])?;
    if png.starts_with(b"\x89PNG") {
        Ok(png)
    } else {
        Err("screencap did not return a PNG".into())
    }
}

/// Quotes one argument for the device shell (adb joins shell arguments with spaces).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Remembers if `uiautomator dump /dev/tty` works on this device. It is faster than a file.
static TTY_DUMP_WORKS: AtomicBool = AtomicBool::new(true);

/// Reads the UI hierarchy XML of the current screen.
fn dump_xml() -> Result<String, String> {
    let mut last_error = String::new();
    for attempt in 0..3 {
        if attempt > 0 {
            // Usually "could not get idle state" while the screen animates.
            sleep(Duration::from_millis(400));
        }
        if TTY_DUMP_WORKS.load(Ordering::Relaxed) {
            // On failure, the file attempt below runs and its error is the one reported.
            if let Ok(out) = adb(&["exec-out", "uiautomator", "dump", "/dev/tty"]) {
                if let Some(xml) = extract_hierarchy(&out) {
                    return Ok(xml);
                }
                if !out.contains("idle state") {
                    TTY_DUMP_WORKS.store(false, Ordering::Relaxed);
                }
            }
        }
        // /data/local/tmp is always writable by adb shell; /sdcard is not on every device.
        let cmd =
            "uiautomator dump /data/local/tmp/phantomqa-dump.xml >/dev/null && cat /data/local/tmp/phantomqa-dump.xml";
        match adb(&["exec-out", cmd]) {
            Ok(out) => match extract_hierarchy(&out) {
                Some(xml) => return Ok(xml),
                None => last_error = out.trim().to_string(),
            },
            Err(e) => last_error = e,
        }
    }
    Err(format!("could not read the screen: {last_error}"))
}

/// Cuts the `<hierarchy>...</hierarchy>` part out of the uiautomator output.
fn extract_hierarchy(out: &str) -> Option<String> {
    let start = out.find("<hierarchy")?;
    let end = match out.rfind("</hierarchy>") {
        Some(end) => end + "</hierarchy>".len(),
        // An empty screen is written as <hierarchy rotation="0" />.
        None => out[start..].find("/>")? + start + 2,
    };
    (start < end).then(|| out[start..end].to_string())
}

// ---------------------------------------------------------------- screen parsing

/// One `<node>` from the XML, in document order, with its nesting depth.
struct Node {
    depth: usize,
    attrs: Vec<(String, String)>,
}

impl Node {
    fn get(&self, name: &str) -> &str {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .unwrap_or("")
    }
    fn is(&self, name: &str) -> bool {
        self.get(name) == "true"
    }
    fn own_label(&self) -> String {
        [self.get("text"), self.get("content-desc"), self.get("hint")]
            .into_iter()
            .map(str::trim)
            .find(|s| !s.is_empty())
            .unwrap_or("")
            .to_string()
    }
    fn clickable(&self) -> bool {
        self.is("clickable") || self.is("long-clickable")
    }
}

/// Splits the XML into nodes. Handles `<node .../>`, `<node ...>` and `</node>`.
fn parse_nodes(xml: &str) -> Vec<Node> {
    let mut nodes = Vec::new();
    let mut depth = 0usize;
    let mut rest = xml;
    while let Some(i) = rest.find('<') {
        rest = &rest[i..];
        if rest.starts_with("</node") {
            depth = depth.saturating_sub(1);
            rest = &rest[1..];
            continue;
        }
        if !rest.starts_with("<node") {
            rest = &rest[1..];
            continue;
        }
        // Find the end of the tag, ignoring '>' inside quoted values.
        let body = &rest[5..];
        let mut in_quote = false;
        let mut end = None;
        for (j, c) in body.char_indices() {
            match c {
                '"' => in_quote = !in_quote,
                '>' if !in_quote => {
                    end = Some(j);
                    break;
                }
                _ => {}
            }
        }
        let Some(end) = end else { break };
        let tag = &body[..end];
        let self_closing = tag.trim_end().ends_with('/');
        nodes.push(Node {
            depth,
            attrs: parse_attrs(tag),
        });
        if !self_closing {
            depth += 1;
        }
        rest = &body[end + 1..];
    }
    nodes
}

/// Parses `name="value"` pairs. Values never hold a raw quote (it is written as &quot;).
fn parse_attrs(tag: &str) -> Vec<(String, String)> {
    let mut attrs = Vec::new();
    let mut rest = tag;
    while let Some(eq) = rest.find("=\"") {
        let name = rest[..eq].trim().to_string();
        let after = &rest[eq + 2..];
        let Some(close) = after.find('"') else { break };
        attrs.push((name, unescape(&after[..close])));
        rest = &after[close + 1..];
    }
    attrs
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(semi) = rest.find(';').filter(|&n| n <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ if entity.starts_with("#x") => u32::from_str_radix(&entity[2..], 16).ok().and_then(char::from_u32),
            _ if entity.starts_with('#') => entity[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// "[0,63][1080,210]" -> (left, top, right, bottom)
fn parse_bounds(s: &str) -> Option<(i32, i32, i32, i32)> {
    let nums: Vec<i32> = s
        .split(|c: char| !(c.is_ascii_digit() || c == '-'))
        .filter(|p| !p.is_empty())
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    match nums[..] {
        [l, t, r, b] => Some((l, t, r, b)),
        _ => None,
    }
}

fn shorten(s: &str) -> String {
    one_line(s, MAX_LABEL)
}

/// Joins all whitespace into single spaces and cuts the text at `max` characters.
fn one_line(s: &str, max: usize) -> String {
    let line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= max {
        return line;
    }
    let cut: String = line.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// Turns the XML into the small element list the agent sees, plus the app package.
fn parse_screen(xml: &str) -> (String, Vec<Element>) {
    let nodes = parse_nodes(xml);
    let package = nodes.first().map(|n| n.get("package").to_string()).unwrap_or_default();
    let mut elements = Vec::new();
    // Ancestors of the current node, as indexes into `nodes`.
    let mut ancestors: Vec<usize> = Vec::new();

    for (i, node) in nodes.iter().enumerate() {
        while ancestors.last().is_some_and(|&a| nodes[a].depth >= node.depth) {
            ancestors.pop();
        }
        let inside_clickable = ancestors.iter().any(|&a| nodes[a].clickable());
        ancestors.push(i);

        let Some((l, t, r, b)) = parse_bounds(node.get("bounds")) else {
            continue;
        };
        if r <= l || b <= t {
            continue;
        }
        let class = node.get("class");
        let editable = class.contains("EditText");
        let clickable = node.clickable();
        let scrollable = node.is("scrollable");
        let checkable = node.is("checkable");
        let interactive = clickable || scrollable || checkable || editable;

        let mut label = node.own_label();
        // A clickable row often has no text itself; its children hold the text.
        if label.is_empty() && (clickable || checkable) {
            label = nodes[i + 1..]
                .iter()
                .take_while(|n| n.depth > node.depth)
                .map(Node::own_label)
                .filter(|s| !s.is_empty())
                .take(3)
                .collect::<Vec<_>>()
                .join(" / ");
        }
        // Plain text inside a clickable row is already shown as that row's label.
        if !interactive && (label.is_empty() || inside_clickable) {
            continue;
        }

        let mut flags = Vec::new();
        if editable {
            flags.push("edit");
        } else if clickable {
            flags.push("click");
        }
        if scrollable {
            flags.push("scroll");
        }
        if checkable {
            flags.push(if node.is("checked") { "on" } else { "off" });
        }
        if node.is("focused") {
            flags.push("focused");
        }
        if node.is("password") {
            flags.push("password");
        }
        if node.get("enabled") == "false" {
            flags.push("disabled");
        }

        let rid = node.get("resource-id");
        elements.push(Element {
            kind: class.rsplit('.').next().unwrap_or(class).to_string(),
            label: shorten(&label),
            rid: rid.rsplit('/').next().unwrap_or(rid).to_string(),
            flags,
            center: ((l + r) / 2, (t + b) / 2),
        });
    }
    (package, elements)
}

/// The text the agent reads: one line per element, numbered from 1.
fn render_screen(package: &str, elements: &[Element]) -> String {
    let mut out = format!("app: {package}\n");
    if elements.is_empty() {
        out.push_str("(no elements found: the app may draw its own UI, e.g. a game or Flutter without semantics)\n");
    }
    for (i, e) in elements.iter().enumerate() {
        out.push_str(&format!("[{}] {}", i + 1, e.kind));
        if !e.label.is_empty() {
            out.push_str(&format!(" {:?}", e.label));
        }
        if !e.rid.is_empty() {
            out.push_str(&format!(" #{}", e.rid));
        }
        for flag in &e.flags {
            out.push(' ');
            out.push_str(flag);
        }
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------- actions

fn read_screen(state: &mut State) -> Result<String, String> {
    let xml = dump_xml()?;
    let (package, elements) = parse_screen(&xml);
    let text = render_screen(&package, &elements);
    state.elements = elements;
    state.package = package;
    Ok(text)
}

/// Short description of an element for the action history, e.g. `[3] Button "Log in"`.
fn describe(id: i64, e: &Element) -> String {
    if e.label.is_empty() {
        format!("[{id}] {}", e.kind)
    } else {
        format!("[{id}] {} {:?}", e.kind, e.label)
    }
}

fn element(state: &State, id: i64) -> Result<Element, String> {
    usize::try_from(id)
        .ok()
        .and_then(|n| n.checked_sub(1))
        .and_then(|n| state.elements.get(n))
        .cloned()
        .ok_or_else(|| format!("no element [{id}] on the last screen. Call screen to see the current list."))
}

/// The avoid entry an element's label matches. Ignores case, and the special dashes and spaces
/// apps often use ("Wi\u{2011}Fi" matches "wi-fi").
fn avoided<'a>(avoid: &'a [String], label: &str) -> Option<&'a str> {
    let fold = |s: &str| -> String {
        s.chars()
            .map(|c| match c {
                '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
                '\u{a0}' | '\u{2007}' | '\u{202f}' => ' ',
                c => c,
            })
            .collect::<String>()
            .to_lowercase()
    };
    let label = fold(label);
    avoid.iter().find(|a| label.contains(&fold(a))).map(String::as_str)
}

/// An element the agent may press: on the last screen, and not on the test's avoid list.
fn pressable(state: &State, id: i64) -> Result<Element, String> {
    let e = element(state, id)?;
    match avoided(&state.avoid, &e.label) {
        Some(entry) => Err(format!(
            "refused: [{id}] matches {entry:?} from the test's avoid list. Do not press it; test something else."
        )),
        None => Ok(e),
    }
}

fn tap(state: &mut State, id: i64) -> Result<String, String> {
    let e = pressable(state, id)?;
    state.remember(format!("tap {}", describe(id, &e)));
    let (x, y) = e.center;
    adb(&["shell", "input", "tap", &x.to_string(), &y.to_string()])?;
    sleep(SETTLE);
    read_screen(state)
}

/// Arguments for one or more `input text` calls. `input text` reads "%s" as a space and
/// cannot type non-ASCII characters, so a literal "%s" is typed as "%" and "s" in two calls.
fn input_text_args(text: &str) -> Result<Vec<String>, String> {
    if text.is_empty() {
        return Err("text is empty".into());
    }
    if let Some(c) = text.chars().find(|c| !c.is_ascii() || c.is_ascii_control()) {
        return Err(format!("cannot type {c:?}: only printable ASCII is supported for now"));
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for c in text.chars() {
        if c == 's' && current.ends_with('%') {
            chunks.push(std::mem::take(&mut current));
        }
        current.push(c);
    }
    chunks.push(current);
    Ok(chunks
        .iter()
        .map(|chunk| shell_quote(&chunk.replace(' ', "%s")))
        .collect())
}

fn type_text(state: &mut State, id: i64, text: &str) -> Result<String, String> {
    type_into(state, id, text, "type_text")
}

fn type_into(state: &mut State, id: i64, text: &str, action: &str) -> Result<String, String> {
    let args = input_text_args(text)?;
    let e = pressable(state, id)?;
    // Keep passwords out of the report.
    let shown = if e.flags.contains(&"password") {
        "(hidden)".to_string()
    } else {
        format!("{text:?}")
    };
    state.remember(format!("{action} {}: {shown}", describe(id, &e)));
    let (x, y) = e.center;
    adb(&["shell", "input", "tap", &x.to_string(), &y.to_string()])?;
    sleep(Duration::from_millis(300));
    for arg in &args {
        adb(&["shell", "input", "text", arg])?;
    }
    sleep(SETTLE);
    read_screen(state)
}

/// The test account password, from the env var the test file names.
fn password(state: &State) -> Result<String, String> {
    let name = state
        .password_env
        .as_deref()
        .ok_or("no test password: type_password works in `phantomqa run`, with \"login\" in the test file")?;
    env_var(name).ok_or_else(|| format!("the test password is missing: {name} is not set"))
}

/// Types the test password into a password field. The agent never sees the password.
fn type_password(state: &mut State, id: i64) -> Result<String, String> {
    let secret = password(state)?;
    // An error about the text could quote part of the password.
    input_text_args(&secret).map_err(|_| "the test password has characters PhantomQA cannot type".to_string())?;
    let e = pressable(state, id)?;
    // A normal field would show the password on screen, and so to the agent.
    if !e.flags.contains(&"password") {
        return Err(format!(
            "[{id}] is not a password field. type_password types only into fields marked password; use type_text for the user name."
        ));
    }
    type_into(state, id, &secret, "type_password")
}

/// Hides the test password wherever it shows up in text that leaves PhantomQA.
fn redact(state: &State, text: String) -> String {
    match password(state) {
        // Very short secrets would hide ordinary text too.
        Ok(secret) if secret.len() >= 4 => text.replace(&secret, "(hidden)"),
        _ => text,
    }
}

fn back(state: &mut State) -> Result<String, String> {
    state.remember("back".into());
    adb(&["shell", "input", "keyevent", "KEYCODE_BACK"])?;
    sleep(SETTLE);
    read_screen(state)
}

fn installed(package: &str) -> bool {
    adb(&["shell", "pm", "path", package]).is_ok_and(|p| p.contains("package:"))
}

fn launch_app(state: &mut State, package: &str) -> Result<String, String> {
    if !valid_package(package) {
        return Err(format!("not a valid package name: {package:?}"));
    }
    state.remember(format!("launch_app {package}"));
    if !state.apps.iter().any(|a| a == package) {
        state.apps.push(package.to_string());
    }
    let resolved = adb(&[
        "shell",
        "cmd",
        "package",
        "resolve-activity",
        "--brief",
        "-a",
        "android.intent.action.MAIN",
        "-c",
        "android.intent.category.LAUNCHER",
        package,
    ]);
    // `cmd package` needs Android 7. Older devices, and errors, fall back to monkey.
    let component = resolved.ok().and_then(|out| {
        out.lines()
            .map(str::trim)
            .rfind(|l| l.contains('/'))
            .map(str::to_string)
    });
    match component {
        Some(component) => {
            let started = adb(&["shell", "am", "start", "-W", "-n", &shell_quote(&component)])?;
            if let Some(line) = started.lines().find(|l| l.starts_with("Error")) {
                return Err(format!("could not start {package}: {}", line.trim()));
            }
        }
        None => {
            // Not installed is the common case; say so plainly.
            if !installed(package) {
                return Err(format!("{package} is not installed"));
            }
            // monkey exits with an error when there is nothing to start, so read its output either way.
            let out = adb(&[
                "shell",
                "monkey",
                "-p",
                package,
                "-c",
                "android.intent.category.LAUNCHER",
                "1",
            ])
            .unwrap_or_else(|e| e);
            if out.contains("No activities found") {
                return Err(format!("{package} has no launcher screen to start"));
            }
            if !out.contains("Events injected") {
                return Err(format!("could not start {package}: {}", one_line(&out, 200)));
            }
        }
    }
    sleep(SETTLE);
    read_screen(state)
}

// ---------------------------------------------------------------- crashes

/// A Java crash, native crash or ANR found in logcat.
#[derive(Debug, PartialEq)]
struct Crash {
    kind: &'static str,
    process: String,
    summary: String,
    lines: Vec<String>,
}

impl Crash {
    fn title(&self) -> String {
        let kind = match self.kind {
            "crash" => "Crash",
            "native crash" => "Native crash",
            other => other,
        };
        let process = if self.process.is_empty() {
            "unknown process"
        } else {
            &self.process
        };
        let summary = if self.summary.is_empty() {
            "app stopped"
        } else {
            &self.summary
        };
        format!("{kind} in {process}: {}", one_line(summary, 150))
    }
}

struct LogLine<'a> {
    time: &'a str,
    pid: &'a str,
    tag: &'a str,
    msg: &'a str,
}

/// Parses one `logcat -v threadtime -v epoch` line: "TIME PID TID LEVEL TAG: MESSAGE".
fn parse_log_line(line: &str) -> Option<LogLine<'_>> {
    let mut rest = line.trim_start();
    let mut fields = [""; 4];
    for field in &mut fields {
        let end = rest.find(char::is_whitespace)?;
        *field = &rest[..end];
        rest = rest[end..].trim_start();
    }
    let colon = rest.find(':')?;
    let msg = &rest[colon + 1..];
    Some(LogLine {
        time: fields[0],
        pid: fields[1],
        tag: rest[..colon].trim(),
        msg: msg.strip_prefix(' ').unwrap_or(msg).trim_end(),
    })
}

/// The text inside the last "(...)" of a line.
fn last_parens(s: &str) -> Option<&str> {
    let open = s.rfind('(')?;
    let close = s[open..].find(')')? + open;
    Some(&s[open + 1..close])
}

/// Groups crash and ANR lines from logcat into crashes.
fn parse_crashes(log: &str) -> Vec<Crash> {
    let mut crashes: Vec<Crash> = Vec::new();
    // Tag and pid of the line that started the last crash; its next lines share them.
    let mut current: Option<(String, String)> = None;

    for line in log.lines() {
        let Some(l) = parse_log_line(line) else { continue };
        let start = match l.tag {
            "AndroidRuntime" if l.msg.starts_with("FATAL EXCEPTION") => Some(("crash", String::new())),
            "ActivityManager" if l.msg.starts_with("ANR in ") => {
                let process = l.msg["ANR in ".len()..].split_whitespace().next().unwrap_or("");
                Some(("ANR", process.to_string()))
            }
            "libc" if l.msg.starts_with("Fatal signal") => {
                Some(("native crash", last_parens(l.msg).unwrap_or("").to_string()))
            }
            _ => None,
        };
        if let Some((kind, process)) = start {
            let summary = if kind == "crash" {
                String::new()
            } else {
                l.msg.to_string()
            };
            crashes.push(Crash {
                kind,
                process,
                summary,
                lines: vec![l.msg.to_string()],
            });
            current = Some((l.tag.to_string(), l.pid.to_string()));
            continue;
        }

        let Some(crash) = crashes.last_mut() else { continue };
        let Some((tag, pid)) = &current else { continue };
        // Native crash details come from the DEBUG tag, in another process (crash_dump).
        let belongs = (l.tag == tag && l.pid == pid) || (l.tag == "DEBUG" && crash.kind == "native crash");
        if !belongs || crash.lines.len() >= MAX_CRASH_LINES {
            continue;
        }
        crash.lines.push(l.msg.to_string());

        match crash.kind {
            "crash" => {
                if let Some(rest) = l.msg.strip_prefix("Process: ") {
                    crash.process = rest.split(',').next().unwrap_or("").trim().to_string();
                } else if crash.summary.is_empty() && !l.msg.starts_with('\t') && !l.msg.starts_with("at ") {
                    crash.summary = l.msg.to_string();
                }
            }
            "ANR" => {
                if let Some(reason) = l.msg.strip_prefix("Reason: ") {
                    crash.summary = reason.to_string();
                }
            }
            _ => {
                if let Some(start) = l.msg.find(">>> ")
                    && let Some(len) = l.msg[start + 4..].find(" <<<")
                {
                    crash.process = l.msg[start + 4..start + 4 + len].to_string();
                }
            }
        }
    }
    crashes
}

/// Device time as a logcat time ("sssss.mmm").
fn device_now() -> Result<String, String> {
    let out = adb(&["shell", "date", "+%s"])?;
    let secs = out.trim();
    if secs.is_empty() || !secs.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("unexpected device time: {secs:?}"));
    }
    Ok(format!("{secs}.000"))
}

/// One millisecond after a logcat time, so the next read starts after it.
fn after(time: &str) -> Option<String> {
    let t: f64 = time.parse().ok()?;
    Some(format!("{:.3}", t + 0.001))
}

/// Reads crash and ANR lines from logcat: since a logcat time, or the whole buffer.
fn read_crash_log(since: Option<&str>) -> Result<String, String> {
    let mut cmd = String::from("logcat -d -v threadtime -v epoch -b crash -b system");
    if let Some(since) = since {
        cmd.push_str(&format!(" -T {}", shell_quote(since)));
    }
    cmd.push_str(" AndroidRuntime:E ActivityManager:E DEBUG:F libc:F '*:S'");
    adb(&["exec-out", &cmd])
}

/// Crashes since the last check. The first check only sets the start time.
fn new_crashes(state: &mut State) -> Result<Vec<Crash>, String> {
    let Some(since) = state.crash_since.clone() else {
        state.crash_since = Some(device_now()?);
        return Ok(Vec::new());
    };
    let log = read_crash_log(Some(&since))?;
    if let Some(next) = log.lines().rev().find_map(parse_log_line).and_then(|l| after(l.time)) {
        state.crash_since = Some(next);
    }
    Ok(parse_crashes(&log))
}

fn render_crash(crash: &Crash) -> String {
    let mut out = format!("!! {}\n", crash.title());
    // The first line, the process line and the summary are already in the title.
    let details = crash
        .lines
        .iter()
        .skip(1)
        .filter(|l| !l.starts_with("Process: ") && **l != crash.summary);
    for line in details.take(5) {
        out.push_str(&format!("   {}\n", line.trim()));
    }
    out
}

/// Checks for new crashes, saves each to the report, and returns a notice for the agent ("" if none).
fn crash_notice(state: &mut State) -> String {
    let crashes = match new_crashes(state) {
        Ok(crashes) => {
            state.crash_check_failed = false;
            crashes
        }
        // Say it once, not after every action.
        Err(_) if state.crash_check_failed => return String::new(),
        Err(e) => {
            state.crash_check_failed = true;
            return format!("\n(could not check for crashes: {e})\n");
        }
    };
    handle_crashes(state, &crashes)
}

/// Saves new crashes of the tested apps and describes all of them for the agent.
fn handle_crashes(state: &mut State, crashes: &[Crash]) -> String {
    let mut out = String::new();
    for crash in crashes {
        let app = crash.process.split(':').next().unwrap_or("");
        if !state.apps.is_empty() && !state.apps.iter().any(|a| a == app) {
            out.push_str(&format!("\n(another app crashed, not saved: {})\n", crash.title()));
            continue;
        }
        let title = crash.title();
        if let Some((_, n)) = state.saved_crashes.iter().find(|(t, _)| *t == title) {
            out.push_str(&format!("\n!! The same crash happened again (bug {n}): {title}\n"));
            continue;
        }
        out.push('\n');
        out.push_str(&render_crash(crash));
        match save_crash(state, crash) {
            Ok(n) => {
                state.saved_crashes.push((title, n));
                out.push_str(&format!("   Saved to the report as bug {n}. Do not report it again.\n"));
            }
            Err(e) => out.push_str(&format!("   Could not save it to the report: {e}\n")),
        }
    }
    out
}

// ---------------------------------------------------------------- report

fn device_name() -> Result<String, String> {
    let out = adb(&[
        "shell",
        "getprop ro.product.model; getprop ro.build.version.release; getprop ro.build.version.sdk",
    ])?;
    let mut lines = out.lines().map(str::trim);
    let model = lines.next().unwrap_or("?");
    let version = lines.next().unwrap_or("?");
    let sdk = lines.next().unwrap_or("?");
    Ok(format!("{model}, Android {version} (API {sdk})"))
}

/// The next free run number: one more than the highest "run-NNN" folder.
fn next_run_number(root: &Path) -> usize {
    let entries = fs::read_dir(root).into_iter().flatten().flatten();
    let highest = entries
        .filter_map(|e| e.file_name().to_str()?.strip_prefix("run-")?.parse::<usize>().ok())
        .max();
    highest.map_or(1, |n| n + 1)
}

/// The report folder, reports/run-NNN. Creates it with report.md on first use, or takes over
/// the folder that `phantomqa run` made for this session.
fn report_dir(state: &mut State) -> Result<PathBuf, String> {
    if let Some(report) = &state.report {
        return Ok(report.dir.clone());
    }
    let report = match &state.run_dir {
        Some(dir) => adopt_run(dir)?,
        None => create_run(&state.report_root, Value::Null)?,
    };
    state.avoid = report.test["avoid"]
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    state.password_env = report.test["login"]["password_env"].as_str().map(str::to_string);
    let dir = report.dir.clone();
    state.report = Some(report);
    Ok(dir)
}

/// Makes the next run-NNN folder with report.md and run.json, and holds its lock.
fn create_run(root: &Path, test: Value) -> Result<Report, String> {
    fs::create_dir_all(root).map_err(|e| format!("cannot create {}: {e}", root.display()))?;
    let mut run = next_run_number(root);
    let dir = loop {
        let dir = root.join(format!("run-{run:03}"));
        match fs::create_dir(&dir) {
            Ok(()) => break dir,
            // Another run took this number at the same moment.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => run += 1,
            Err(e) => return Err(format!("cannot create {}: {e}", dir.display())),
        }
    };
    let device = device_name().unwrap_or_else(|_| "unknown".into());
    let mut header = format!("# PhantomQA test report: run {run}\n\n- Device: {device}\n");
    for (key, label) in [
        ("app", "App"),
        ("file", "Test"),
        ("routine", "Routine"),
        ("goal", "Goal"),
        ("model", "Model"),
    ] {
        if let Some(value) = test[key].as_str() {
            header.push_str(&format!("- {label}: {}\n", one_line(value, 300)));
        }
    }
    fs::write(dir.join("report.md"), header).map_err(|e| format!("cannot write report.md: {e}"))?;
    let report = Report {
        lock: hold_run_lock(&dir),
        dir,
        run,
        device,
        bugs: 0,
        bug_list: Vec::new(),
        finished: false,
        session_video: None,
        test,
    };
    write_run_json(&report)?;
    Ok(report)
}

/// Takes over a run folder made by `phantomqa run`, keeping what its run.json already has.
fn adopt_run(dir: &Path) -> Result<Report, String> {
    let doc = read_json(&dir.join("run.json"))?;
    let run = doc["run"]
        .as_u64()
        .ok_or_else(|| format!("{} has no run number", dir.join("run.json").display()))?;
    let bug_list = doc["bugs"].as_array().cloned().unwrap_or_default();
    Ok(Report {
        lock: hold_run_lock(dir),
        dir: dir.to_path_buf(),
        run: run as usize,
        device: doc["device"].as_str().unwrap_or("unknown").to_string(),
        bugs: bug_list.len(),
        bug_list,
        finished: false,
        session_video: None,
        test: doc["test"].clone(),
    })
}

/// Hidden lock file in a run folder. Everyone working on the run holds a shared lock on it:
/// the session, and `phantomqa run` around it. The API sees a free lock on an unfinished run
/// as "interrupted".
const RUN_LOCK: &str = ".lock";
const RUN_FILE_VERSION: u64 = 1;

fn hold_run_lock(dir: &Path) -> Option<fs::File> {
    fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(dir.join(RUN_LOCK))
        .ok()
        .filter(|file| file.lock_shared().is_ok())
}

fn read_json(path: &Path) -> Result<Value, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{} is not valid JSON: {e}", path.display()))
}

/// Writes a JSON file atomically, so a reader never sees half a file.
fn write_json(path: &Path, doc: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(doc).map_err(|e| e.to_string())?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    fs::write(&tmp, text + "\n")
        .and_then(|()| fs::rename(&tmp, path))
        .map_err(|e| {
            let _ = fs::remove_file(&tmp);
            format!("cannot write {}: {e}", path.display())
        })
}

fn write_run_json(report: &Report) -> Result<(), String> {
    let mut doc = json!({
        "version": RUN_FILE_VERSION,
        "run": report.run,
        "status": if report.finished { "finished" } else { "running" },
        "device": report.device,
        "bugs": report.bug_list,
        "session_video": report.session_video,
    });
    if !report.test.is_null() {
        doc["test"] = report.test.clone();
    }
    write_json(&report.dir.join("run.json"), &doc)
}

/// Logs to stderr. Never panics: the agent may have closed the pipe.
/// A line on stdout. Unlike println!, it does not panic when the reader went away (`| head`).
fn say(line: &str) {
    let _ = writeln!(std::io::stdout(), "{line}");
}

fn warn(message: &str) {
    let _ = writeln!(std::io::stderr(), "phantomqa: {message}");
}

/// The app's recent log, or recent warnings and errors when the app is not running.
fn app_log(package: &str) -> String {
    let pid = if valid_package(package) {
        adb(&["shell", "pidof", package])
            .ok()
            .and_then(|out| out.split_whitespace().next().map(str::to_string))
            .filter(|pid| pid.chars().all(|c| c.is_ascii_digit()))
    } else {
        None
    };
    let cmd = match &pid {
        Some(pid) => format!("logcat -d -v threadtime -t 300 --pid={pid}"),
        None => "logcat -d -v threadtime -t 300 '*:W'".to_string(),
    };
    adb(&["exec-out", &cmd]).unwrap_or_else(|e| format!("(could not read the log: {e})\n"))
}

struct Bug {
    title: String,
    severity: String,
    package: String,
    /// Markdown with the details: steps, expected, actual, or crash lines.
    body: String,
}

fn render_bug(n: usize, bug: &Bug, history: &[String], attachments: &str) -> String {
    let mut out = format!("\n## Bug {n}: {}\n\n", bug.title);
    out.push_str(&format!("- Severity: {}\n", bug.severity));
    if !bug.package.is_empty() {
        out.push_str(&format!("- App: {}\n", bug.package));
    }
    out.push_str(&format!("\n{}\n", bug.body.trim_end()));
    if !history.is_empty() {
        out.push_str("\n**Recent actions** (newest last)\n\n");
        for action in history {
            out.push_str(&format!("- {action}\n"));
        }
    }
    out.push_str(&format!("\n{attachments}\n"));
    out
}

/// Saves one bug: a section in report.md, a screenshot and the log. Returns the bug number.
fn save_bug(state: &mut State, bug: &Bug, extra_log: &str) -> Result<usize, String> {
    let dir = report_dir(state)?;
    // The folder may have been deleted while the session runs.
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let n = state.report.as_ref().map_or(0, |r| r.bugs) + 1;
    let name = format!("bug-{n:02}");

    let mut attachments = Vec::new();
    let mut files = serde_json::Map::new();
    if config().screenshots {
        let saved =
            screenshot().and_then(|png| fs::write(dir.join(format!("{name}.png")), png).map_err(|e| e.to_string()));
        attachments.push(match saved {
            Ok(()) => {
                files.insert("screenshot".into(), json!(format!("{name}.png")));
                format!("![screenshot]({name}.png)")
            }
            Err(e) => format!("(no screenshot: {e})"),
        });
    }
    let log = redact(state, format!("{extra_log}{}", app_log(&bug.package)));
    attachments.push(match fs::write(dir.join(format!("{name}.log")), log) {
        Ok(()) => {
            files.insert("log".into(), json!(format!("{name}.log")));
            format!("Log: [{name}.log]({name}.log)")
        }
        Err(e) => format!("(no log: {e})"),
    });
    if let Some(video) = &mut state.video {
        let (line, videos) = video_clip(video, &dir, &name);
        attachments.push(line);
        match videos.as_slice() {
            [] => {}
            [one] if one.ends_with(&format!("{name}.mp4")) => {
                files.insert("video".into(), json!(one));
            }
            parts => {
                files.insert("video_parts".into(), json!(parts));
            }
        }
    }
    let section = render_bug(n, bug, &state.history, &attachments.join("\n\n"));
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("report.md"))
        .and_then(|mut file| file.write_all(section.as_bytes()))
        .map_err(|e| format!("cannot write report.md: {e}"))?;
    if let Some(report) = &mut state.report {
        report.bugs = n;
        report.bug_list.push(json!({
            "bug": n,
            "title": bug.title,
            "severity": bug.severity,
            "app": bug.package,
            "files": files,
        }));
        // report.md already has the bug; a failed run.json write must not lose it.
        if let Err(e) = write_run_json(report) {
            warn(&e);
        }
    }
    Ok(n)
}

fn save_crash(state: &mut State, crash: &Crash) -> Result<usize, String> {
    let lines = crash.lines.join("\n");
    let bug = Bug {
        title: crash.title(),
        severity: "critical".into(),
        package: crash.process.split(':').next().unwrap_or("").to_string(),
        body: format!("Found automatically in logcat.\n\n{}", code_block(&lines)),
    };
    save_bug(
        state,
        &bug,
        &format!("--- {} ---\n{lines}\n\n--- recent log ---\n", crash.kind),
    )
}

/// A Markdown code block whose fence cannot clash with the text inside it.
fn code_block(text: &str) -> String {
    let mut fence = String::from("```");
    while text.contains(&fence) {
        fence.push('`');
    }
    format!("{fence}\n{text}\n{fence}\n")
}

/// "1. Open the app" -> "Open the app"
fn strip_step_number(step: &str) -> &str {
    let digits = step.len() - step.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let rest = &step[digits..];
    match rest.strip_prefix('.').or_else(|| rest.strip_prefix(')')) {
        Some(after) if digits > 0 && after.starts_with(' ') => after.trim_start(),
        _ => step,
    }
}

/// Checks the report_bug arguments and turns them into a bug.
fn bug_from_args(args: &Value, package: &str) -> Result<Bug, String> {
    let text = |key: &str| {
        args[key]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("{key:?} is required"))
    };
    // A title is one heading line.
    let title = one_line(&text("title")?, 150);
    let expected = text("expected")?;
    let actual = text("actual")?;

    let raw_steps: Vec<&str> = match &args["steps"] {
        Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
        Value::String(s) => s.lines().collect(),
        _ => Vec::new(),
    };
    let steps: Vec<&str> = raw_steps
        .into_iter()
        .map(|s| strip_step_number(s.trim()))
        .filter(|s| !s.is_empty())
        .collect();
    if steps.is_empty() {
        return Err("\"steps\" is required: the steps to reproduce, in order".into());
    }

    let severity = match args["severity"].as_str().map(|s| s.trim().to_lowercase()) {
        None => "medium".to_string(),
        Some(s) if SEVERITIES.contains(&s.as_str()) => s,
        Some(s) => return Err(format!("severity must be one of {}, not {s:?}", SEVERITIES.join(", "))),
    };

    let mut body = String::from("**Steps**\n\n");
    for (i, step) in steps.iter().enumerate() {
        body.push_str(&format!("{}. {step}\n", i + 1));
    }
    body.push_str(&format!("\n**Expected:** {expected}\n\n**Actual:** {actual}\n"));
    Ok(Bug {
        title,
        severity,
        package: package.to_string(),
        body,
    })
}

fn report_bug(state: &mut State, args: &Value) -> Result<String, String> {
    let bug = bug_from_args(args, &state.package)?;
    let n = save_bug(state, &bug, "")?;
    let path = state
        .report
        .as_ref()
        .map(|r| r.dir.join("report.md"))
        .unwrap_or_default();
    Ok(format!("Saved bug {n} to {}.", path.display()))
}

// ---------------------------------------------------------------- video

/// Length of one recorded part. Short parts keep bug clips short. Android allows up to 180 s.
const PART_SECONDS: u64 = 30;
/// A bug clip also gets the part before it when the last part is shorter than this.
const MIN_CLIP_SECONDS: f64 = 15.0;
const VIDEO_BIT_RATE: u32 = 1_000_000;
/// Always writable by `adb shell`, unlike /sdcard on some devices.
const DEVICE_TMP: &str = "/data/local/tmp";
/// Video stops after this many failed parts in a row.
const MAX_PART_FAILURES: usize = 3;
/// How long a bug waits for the video part that shows it.
const CUT_TIMEOUT: Duration = Duration::from_secs(20);
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(300);

/// One recorded part, pulled to the host.
struct Part {
    file: PathBuf,
    seconds: f64,
}

/// What the recording thread and the main thread share.
#[derive(Default)]
struct Recorder {
    parts: Vec<Part>,
    /// Parts that ended, kept or dropped. A cut waits for this to grow.
    finished: usize,
    /// Device PID of the running screenrecord.
    pid: Option<String>,
    /// Counts cuts, so a part cut right after it started is not counted as a failure.
    cuts: usize,
    stop: bool,
    /// The recording thread has ended.
    done: bool,
    error: Option<String>,
}

type Shared = Arc<(Mutex<Recorder>, Condvar)>;

struct Video {
    shared: Shared,
    thread: Option<JoinHandle<()>>,
    /// run-NNN/video, where the parts go.
    dir: PathBuf,
    /// The report links to raw parts, so they must be kept.
    parts_linked: bool,
}

fn lock(shared: &Shared) -> MutexGuard<'_, Recorder> {
    shared.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Starts recording for an MCP session, or notes why video is off.
/// The first tool call of an MCP session starts its run: the report folder, then video.
fn start_run(state: &mut State) {
    if let Err(e) = report_dir(state) {
        warn(&e);
    }
    start_video(state);
}

fn start_video(state: &mut State) {
    state.video_checked = true;
    if !config().video {
        state.video_off = Some("turned off with --no-video or PHANTOMQA_VIDEO=0".into());
        return;
    }
    match try_start_video(state) {
        Ok(video) => state.video = Some(video),
        Err(e) => state.video_off = Some(e),
    }
}

fn try_start_video(state: &mut State) -> Result<Video, String> {
    let found = adb(&["shell", "command -v screenrecord"]).unwrap_or_default();
    if found.trim().is_empty() {
        return Err("screenrecord is not available on the device".into());
    }
    // Remove parts left by a session that was killed, unless a session is recording now.
    // "[/]" keeps pgrep from matching its own shell.
    let cleanup = format!("pgrep -f '[/]data/local/tmp/phantomqa-' >/dev/null || rm -f {DEVICE_TMP}/phantomqa-*.mp4");
    let _ = adb(&["shell", &cleanup]);

    let dir = report_dir(state)?.join("video");
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let shared: Shared = Arc::default();
    let thread = std::thread::Builder::new()
        .name("video".into())
        .spawn({
            let shared = shared.clone();
            let dir = dir.clone();
            move || record_parts(&shared, &dir)
        })
        .map_err(|e| format!("cannot start the video thread: {e}"))?;
    Ok(Video {
        shared,
        thread: Some(thread),
        dir,
        parts_linked: false,
    })
}

/// The recording thread: records parts one after another until told to stop.
fn record_parts(shared: &Shared, dir: &Path) {
    let session = std::process::id();
    let mut failures = 0;
    for n in 1.. {
        let cuts_before = {
            let r = lock(shared);
            if r.stop {
                break;
            }
            r.cuts
        };
        let device_file = format!("{DEVICE_TMP}/phantomqa-{session}-{n}.mp4");
        let file = dir.join(format!("part-{n:03}.mp4"));
        let started = Instant::now();
        let result = record_part(shared, &device_file).and_then(|()| pull_video(&device_file, &file));
        let _ = adb(&["shell", "rm", "-f", &device_file]);

        let mut r = lock(shared);
        r.finished += 1;
        match result {
            Ok(()) => {
                failures = 0;
                r.parts.push(Part {
                    file,
                    seconds: started.elapsed().as_secs_f64(),
                });
            }
            Err(e) => {
                let _ = fs::remove_file(&file);
                // A part stopped right after it started can be empty. That is not a failure.
                let stopped_by_us = r.cuts != cuts_before || r.stop;
                if !stopped_by_us {
                    failures += 1;
                }
                if failures >= MAX_PART_FAILURES {
                    r.error = Some(e);
                    break;
                }
            }
        }
        shared.1.notify_all();
        drop(r);
        if failures > 0 {
            sleep(Duration::from_secs(1));
        }
    }
    lock(shared).done = true;
    shared.1.notify_all();
}

/// Records one part on the device. Returns when screenrecord stops (time limit or signal).
fn record_part(shared: &Shared, device_file: &str) -> Result<(), String> {
    // `exec` keeps the shell's PID, which `echo $$` prints first, so we can stop exactly this recorder.
    let script = format!(
        "echo $$; exec screenrecord --time-limit {PART_SECONDS} --bit-rate {VIDEO_BIT_RATE} {}",
        shell_quote(device_file)
    );
    let mut child = adb_command()
        .args(["shell", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(adb_missing)?;

    // Keep reading both pipes until the end. Closing them early can end the recording.
    let (pid_tx, pid_rx) = mpsc::channel();
    let stdout = child.stdout.take().expect("stdout is piped");
    let out_reader = std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut first = String::new();
        let _ = reader.read_line(&mut first);
        let _ = pid_tx.send(first);
        let mut rest = String::new();
        let _ = reader.read_to_string(&mut rest);
    });
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let err_reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });

    let pid = pid_rx
        .recv_timeout(Duration::from_secs(10))
        .unwrap_or_default()
        .trim()
        .to_string();
    if is_pid(&pid) {
        let stop_now = {
            let mut r = lock(shared);
            r.pid = Some(pid.clone());
            r.stop
        };
        // Stop was asked while this part was starting.
        if stop_now {
            stop_part(&pid);
        }
    }
    let status = wait_with_timeout(&mut child, Duration::from_secs(PART_SECONDS + 30));
    lock(shared).pid = None;
    let _ = out_reader.join();
    let errors = err_reader.join().unwrap_or_default();
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(_) => Err(format!("screenrecord failed: {}", errors.trim())),
        Err(e) => Err(format!("screenrecord {e}")),
    }
}

fn is_pid(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

/// Asks one screenrecord to stop. SIGINT makes it finish the file properly.
fn stop_part(pid: &str) {
    if is_pid(pid) {
        // Check the PID still belongs to screenrecord before signalling it.
        let _ = adb(&[
            "shell",
            &format!("grep -q screenrecord /proc/{pid}/cmdline 2>/dev/null && kill -2 {pid}"),
        ]);
    }
}

/// A finished MP4 has an "ftyp" box at the start and a "moov" box. A killed recording has no "moov".
fn looks_like_mp4(bytes: &[u8]) -> bool {
    bytes.len() > 1024 && &bytes[4..8] == b"ftyp" && bytes.windows(4).any(|w| w == b"moov")
}

/// Copies a recording from the device. `exec-out cat` avoids host path quoting problems.
fn pull_video(device_file: &str, file: &Path) -> Result<(), String> {
    let bytes = adb_bytes(&["exec-out", "cat", device_file])?;
    if !looks_like_mp4(&bytes) {
        return Err("the recording is empty or broken".into());
    }
    if let Some(dir) = file.parent() {
        let _ = fs::create_dir_all(dir);
    }
    fs::write(file, bytes).map_err(|e| format!("cannot write {}: {e}", file.display()))
}

/// The last part, plus the part before it when the last one is short.
fn clip_parts(parts: &[Part]) -> Vec<PathBuf> {
    match parts {
        [] => Vec::new(),
        [.., before, last] if last.seconds < MIN_CLIP_SECONDS => vec![before.file.clone(), last.file.clone()],
        [.., last] => vec![last.file.clone()],
    }
}

impl Video {
    /// Ends the current part, so the video covers the moment of the bug. Returns the parts for its clip.
    fn cut(&self) -> Result<Vec<PathBuf>, String> {
        let deadline = Instant::now() + CUT_TIMEOUT;
        let mut r = lock(&self.shared);
        if r.done {
            return Err(r.error.clone().unwrap_or_else(|| "video has stopped".into()));
        }
        let before = r.finished;
        r.cuts += 1;
        let mut stopped: Option<String> = None;
        while r.finished == before && !r.done {
            // Stop whichever part is recording now; a new one may have started meanwhile.
            if let Some(pid) = r.pid.clone()
                && stopped.as_ref() != Some(&pid)
            {
                drop(r);
                stop_part(&pid);
                stopped = Some(pid);
                r = lock(&self.shared);
                continue;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("the video part was not ready in time".into());
            }
            r = self.shared.1.wait_timeout(r, left).unwrap_or_else(|p| p.into_inner()).0;
        }
        let parts = clip_parts(&r.parts);
        if parts.is_empty() {
            return Err(r.error.clone().unwrap_or_else(|| "no video was recorded yet".into()));
        }
        Ok(parts)
    }

    /// Stops recording and waits for the last part.
    fn stop(&mut self) {
        let pid = {
            let mut r = lock(&self.shared);
            r.stop = true;
            r.pid.clone()
        };
        if let Some(pid) = pid {
            stop_part(&pid);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Joins video files into one with ffmpeg, without re-encoding.
fn join_videos(ffmpeg: &str, files: &[PathBuf], out: &Path) -> Result<(), String> {
    let list = out.with_extension("txt");
    let lines: String = files
        .iter()
        .map(|f| format!("file '{}'\n", f.to_string_lossy().replace('\'', "'\\''")))
        .collect();
    fs::write(&list, lines).map_err(|e| format!("cannot write {}: {e}", list.display()))?;
    let result = Command::new(ffmpeg)
        .args(["-nostdin", "-v", "error", "-y", "-f", "concat", "-safe", "0", "-i"])
        .arg(&list)
        .args(["-map", "0:v", "-c", "copy"])
        .arg(out)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run ffmpeg ({ffmpeg}): {e}"))
        .and_then(|mut child| {
            let mut stderr = child.stderr.take().expect("stderr is piped");
            let reader = std::thread::spawn(move || {
                let mut text = String::new();
                let _ = stderr.read_to_string(&mut text);
                text
            });
            let status = wait_with_timeout(&mut child, FFMPEG_TIMEOUT).map_err(|e| format!("ffmpeg {e}"))?;
            let errors = reader.join().unwrap_or_default();
            let size = fs::metadata(out).map(|m| m.len()).unwrap_or(0);
            if status.success() && size > 0 {
                Ok(())
            } else {
                Err(format!(
                    "ffmpeg failed: {}",
                    errors.lines().next().unwrap_or("no output")
                ))
            }
        });
    let _ = fs::remove_file(&list);
    if result.is_err() {
        let _ = fs::remove_file(out);
    }
    result
}

/// Makes one video from the given parts: a copy for one part, ffmpeg for more.
fn make_video(files: &[PathBuf], out: &Path) -> Result<(), String> {
    match (files, &config().ffmpeg) {
        ([], _) => Err("no video parts".into()),
        ([one], _) => fs::copy(one, out)
            .map(|_| ())
            .map_err(|e| format!("cannot write {}: {e}", out.display())),
        (_, Some(ffmpeg)) => join_videos(ffmpeg, files, out),
        (_, None) => Err("ffmpeg is not installed".into()),
    }
}

/// Markdown links to raw parts, relative to the run folder.
fn part_links(files: &[PathBuf]) -> String {
    let links: Vec<String> = files
        .iter()
        .filter_map(|f| f.file_name()?.to_str().map(|name| format!("[{name}](video/{name})")))
        .collect();
    links.join(", ")
}

/// The video line for a bug: its own clip, or links to the raw parts.
/// Also returns the video files, relative to the run folder.
fn video_clip(video: &mut Video, run_dir: &Path, name: &str) -> (String, Vec<String>) {
    let parts = match video.cut() {
        Ok(parts) => parts,
        Err(e) => return (format!("(no video: {e})"), Vec::new()),
    };
    let clip = run_dir.join(format!("{name}.mp4"));
    match make_video(&parts, &clip) {
        Ok(()) => (format!("Video: [{name}.mp4]({name}.mp4)"), vec![format!("{name}.mp4")]),
        Err(e) => {
            video.parts_linked = true;
            let relative = parts
                .iter()
                .filter_map(|p| Some(format!("video/{}", p.file_name()?.to_str()?)))
                .collect();
            (format!("Video parts ({e}): {}", part_links(&parts)), relative)
        }
    }
}

/// Ends the session: stops recording, makes session.mp4, finishes report.md and run.json.
fn finish_session(state: &mut State) {
    let note = match state.video.take() {
        Some(mut video) => {
            video.stop();
            let r = lock(&video.shared);
            let files: Vec<PathBuf> = r.parts.iter().map(|p| p.file.clone()).collect();
            let run_dir = video.dir.parent().map(Path::to_path_buf).unwrap_or_default();
            let mut note = match make_video(&files, &run_dir.join("session.mp4")) {
                Ok(()) => {
                    if !video.parts_linked {
                        let _ = fs::remove_dir_all(&video.dir);
                    }
                    "Whole session: [session.mp4](session.mp4)".to_string()
                }
                Err(_) if files.is_empty() => "No video was recorded.".to_string(),
                Err(e) => format!("The session video is in parts in the video folder ({e})."),
            };
            if let Some(error) = &r.error {
                note.push_str(&format!("\n\nRecording stopped early: {error}"));
            }
            note
        }
        None => match &state.video_off {
            Some(reason) => format!("Video was off: {reason}."),
            None => String::new(),
        },
    };
    // Only finish a run that exists: a session that never started one has no folder.
    let Some(report) = &mut state.report else { return };
    let mut tail = String::new();
    if report.bugs == 0 {
        tail.push_str("\nNo bugs found.\n");
    }
    if !note.is_empty() {
        tail.push_str(&format!("\n## Video\n\n{note}\n"));
    }
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(report.dir.join("report.md"))
        .and_then(|mut file| file.write_all(tail.as_bytes()));
    report.finished = true;
    report.session_video = report
        .dir
        .join("session.mp4")
        .is_file()
        .then(|| "session.mp4".to_string());
    if let Err(e) = write_run_json(report) {
        warn(&e);
    }
    report.lock = None;
    if report.test.is_object() {
        let model = report.test["model"].as_str().unwrap_or("").to_string();
        stand_in_for_runner(&report.dir, &model);
    }
}

// ---------------------------------------------------------------- MCP server

const INSTRUCTIONS: &str = "PhantomQA lets you test an Android app on an emulator like a real user. \
Start with launch_app, read the numbered screen list, and use tap, type_text and back. \
For a test account password, use type_password; you never see the password. \
Try the main features, wrong inputs and edge cases. Call report_bug for each problem while it is on screen, \
with clear steps to reproduce. Crashes and ANRs are found and saved automatically.";

fn tool_list() -> Value {
    let no_args = json!({"type": "object", "properties": {}});
    json!([
        {
            "name": "screen",
            "description": "Read the current Android screen as a numbered list of elements (type, label, #resource-id, flags). Use the numbers with tap and type_text.",
            "inputSchema": no_args,
        },
        {
            "name": "tap",
            "description": "Tap element [id] from the last screen list. Returns the new screen.",
            "inputSchema": {
                "type": "object",
                "properties": {"id": {"type": "integer", "description": "Element number from the last screen list"}},
                "required": ["id"],
            },
        },
        {
            "name": "type_text",
            "description": "Tap element [id] (a text field) and type text into it. Printable ASCII only. Returns the new screen.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "Element number of the text field"},
                    "text": {"type": "string", "description": "Text to type"},
                },
                "required": ["id", "text"],
            },
        },
        {
            "name": "type_password",
            "description": "Tap password field [id] and type the test account password into it. You never see the password. Works when the test file has a login. Returns the new screen.",
            "inputSchema": {
                "type": "object",
                "properties": {"id": {"type": "integer", "description": "Element number of the password field"}},
                "required": ["id"],
            },
        },
        {
            "name": "back",
            "description": "Press the Android back button. Returns the new screen.",
            "inputSchema": no_args,
        },
        {
            "name": "launch_app",
            "description": "Start an installed app by package name, e.g. com.android.settings. Returns the new screen.",
            "inputSchema": {
                "type": "object",
                "properties": {"package": {"type": "string", "description": "Android package name"}},
                "required": ["package"],
            },
        },
        {
            "name": "crashes",
            "description": "Check for app crashes and ANRs since the last check. Every action already does this and saves crashes to the report; use it after waiting on a screen.",
            "inputSchema": no_args,
        },
        {
            "name": "report_bug",
            "description": "Save a bug to the test report. The app log, a screenshot and a video clip are attached automatically (when turned on), so call this while the bug is on screen. Crashes are saved automatically; do not report them again.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "Short summary, e.g. \"Login button does nothing after a wrong password\""},
                    "steps": {"type": "array", "items": {"type": "string"}, "description": "Steps to reproduce, in order"},
                    "expected": {"type": "string", "description": "What should happen"},
                    "actual": {"type": "string", "description": "What happens instead"},
                    "severity": {"type": "string", "enum": SEVERITIES, "description": "Default: medium"},
                },
                "required": ["title", "steps", "expected", "actual"],
            },
        },
    ])
}

fn call_tool(state: &mut State, params: &Value) -> Value {
    let name = params["name"].as_str().unwrap_or("");
    let args = &params["arguments"];
    let int = |key: &str| int_arg(&args[key]).ok_or_else(|| format!("argument {key:?} must be an element number"));
    let text = |key: &str| {
        args[key]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| format!("argument {key:?} must be a string"))
    };
    if !state.video_checked {
        start_run(state);
    }
    // Under `phantomqa run`, acting without the run folder would also act without the avoid list.
    if state.run_dir.is_some() && state.report.is_none() {
        let text = "PhantomQA cannot open its run folder, so it refuses to act. Stop the test.";
        return json!({"content": [{"type": "text", "text": text}], "isError": true});
    }
    // Start the crash clock before the first action, so a crash it causes is not missed.
    if state.crash_since.is_none() {
        state.crash_since = device_now().ok();
    }
    let result = match name {
        "screen" => read_screen(state),
        "tap" => int("id").and_then(|id| tap(state, id)),
        "type_text" => int("id").and_then(|id| text("text").and_then(|t| type_text(state, id, &t))),
        "type_password" => int("id").and_then(|id| type_password(state, id)),
        "back" => back(state),
        "launch_app" => text("package").and_then(|p| launch_app(state, &p)),
        "crashes" => Ok(String::new()),
        "report_bug" => report_bug(state, args),
        _ => Err(format!("unknown tool: {name}")),
    };
    let checks_crashes = matches!(
        name,
        "screen" | "tap" | "type_text" | "type_password" | "back" | "launch_app" | "crashes"
    );
    let notice = if checks_crashes {
        crash_notice(state)
    } else {
        String::new()
    };

    let (mut text, is_error) = match result {
        Ok(text) => (text, false),
        Err(error) => (error, true),
    };
    if name == "crashes" && notice.is_empty() {
        text = "No new crashes or ANRs.".into();
    }
    text.push_str(&notice);
    let text = redact(state, text);
    json!({"content": [{"type": "text", "text": text.trim_start()}], "isError": is_error})
}

/// An element number sent as 3, 3.0, "3" or "[3]".
fn int_arg(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| {
            value
                .as_f64()
                .filter(|f| f.fract() == 0.0 && f.abs() < 1e9)
                .map(|f| f as i64)
        })
        .or_else(|| {
            value
                .as_str()?
                .trim()
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse()
                .ok()
        })
}

/// Handles one JSON-RPC message. Returns the reply, or None for notifications.
fn handle_message(state: &mut State, msg: &Value) -> Option<Value> {
    let method = msg.get("method")?.as_str().unwrap_or("");
    let id = msg.get("id")?.clone();
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "phantomqa", "version": env!("CARGO_PKG_VERSION")},
            "instructions": INSTRUCTIONS,
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tool_list()})),
        "tools/call" => Ok(call_tool(state, &msg["params"])),
        _ => Err(json!({"code": -32601, "message": format!("unknown method: {method}")})),
    };
    Some(match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
    })
}

/// MCP over stdio: one JSON message per line. Only replies go to stdout; logs go to stderr.
fn serve_mcp(mut state: State) {
    let mut stdout = std::io::stdout();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            // Batches were removed from MCP in 2025-06-18; answer instead of staying silent.
            Ok(Value::Array(_)) => Some(json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": {"code": -32600, "message": "batches are not supported; send one message per line"},
            })),
            Ok(msg) => handle_message(&mut state, &msg),
            Err(e) => Some(json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": {"code": -32700, "message": format!("parse error: {e}")},
            })),
        };
        if let Some(reply) = reply
            && writeln!(stdout, "{reply}").and_then(|_| stdout.flush()).is_err()
        {
            break;
        }
    }
    // The client closed stdin: the test session is over.
    finish_session(&mut state);
}

/// Set for the MCP worker process, which does the real work.
const WORKER_ENV: &str = "PHANTOMQA_MCP_WORKER";

/// Runs the MCP server in a child process with its own process group, and passes messages
/// through. Agents kill their MCP server a fraction of a second after they exit (Claude Code
/// sends SIGINT, SIGTERM, then SIGKILL). The worker is not hit by that: it sees stdin close and
/// finishes the session properly (last video part, session.mp4, run.json).
/// Returns false if the worker cannot start; the server then runs in this process.
fn relay_mcp() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let mut command = Command::new(exe);
    command
        .args(std::env::args_os().skip(1))
        .env(WORKER_ENV, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut worker = match command.spawn() {
        Ok(worker) => worker,
        Err(e) => {
            warn(&format!(
                "cannot start the MCP worker, so a killed agent may cut the session short: {e}"
            ));
            return false;
        }
    };
    let mut to_worker = worker.stdin.take().expect("stdin is piped");
    // When stdin closes, to_worker is dropped, and the worker sees the end of the session.
    std::thread::spawn(move || pass_through(&mut std::io::stdin(), &mut to_worker));
    let mut from_worker = worker.stdout.take().expect("stdout is piped");
    pass_through(&mut from_worker, &mut std::io::stdout());
    let _ = worker.wait();
    true
}

/// Copies bytes as they come, until either side closes. Not io::copy: on Linux it uses splice,
/// which can wait for more data on the socket pairs that Node and Bun agents use for stdio.
fn pass_through(from: &mut dyn Read, to: &mut dyn Write) {
    let mut buf = [0u8; 8192];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        if to.write_all(&buf[..n]).and_then(|()| to.flush()).is_err() {
            return;
        }
    }
}

// ---------------------------------------------------------------- tokens
//
// API tokens look like `pqa_<id>_<secret>`: a short public id to find the token, and 32 random
// bytes of secret. tokens.json keeps only a SHA-256 hash of each token. SHA-256 is enough for
// random tokens; slow hashes are for passwords people choose. The file is read on every check,
// so a removed or rotated token stops working at once, and a broken file refuses every token.

const TOKEN_PREFIX: &str = "pqa_";
const TOKEN_ID_BYTES: usize = 8;
const TOKEN_SECRET_BYTES: usize = 32;
const TOKENS_FILE_VERSION: u64 = 1;
/// Longer Authorization headers are refused without further work.
const MAX_AUTH_HEADER: usize = 512;
/// A lock older than this was left by a killed command.
const STALE_LOCK: Duration = Duration::from_secs(30);

/// One stored token. A name can have two entries for a while during a rotation with grace time.
#[derive(Debug, Clone, PartialEq)]
struct TokenEntry {
    name: String,
    id: String,
    /// "sha256:<hex>" of the whole token.
    hash: String,
    /// Unix seconds; None means it never expires.
    expires: Option<u64>,
    /// Lifetime given when created, reused by `rotate`.
    ttl: Option<u64>,
    /// An old key kept valid until `expires` after a rotation.
    rotated: bool,
}

/// Why a token was refused. Clients get the same 401 for all of them; the reason is for logs.
#[derive(Debug, PartialEq)]
enum TokenDenied {
    Missing,
    Malformed,
    Unknown,
    Expired,
    /// tokens.json is missing or broken, so nothing is accepted.
    NoTokens(String),
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Random bytes from the OS, as hex. Fails instead of falling back to weak randomness.
fn random_hex(bytes: usize) -> Result<String, String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| format!("the OS random generator failed: {e}"))?;
    Ok(hex(&buf))
}

fn hash_token(token: &str) -> String {
    use sha2::Digest;
    format!("sha256:{}", hex(&sha2::Sha256::digest(token.as_bytes())))
}

fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Splits `pqa_<id>_<secret>` into id and secret, or None if the format is wrong in any way.
fn parse_token(token: &str) -> Option<(&str, &str)> {
    let (id, secret) = token.strip_prefix(TOKEN_PREFIX)?.split_once('_')?;
    (is_lower_hex(id, TOKEN_ID_BYTES * 2) && is_lower_hex(secret, TOKEN_SECRET_BYTES * 2)).then_some((id, secret))
}

/// The token from an `Authorization: Bearer <token>` header value.
fn bearer_token(header: &str) -> Option<&str> {
    if header.len() > MAX_AUTH_HEADER {
        return None;
    }
    let (scheme, token) = header.trim().split_once(char::is_whitespace)?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() && !token.contains(char::is_whitespace))
        .then_some(token)
}

/// Checks a presented token against the entries. Returns the token's name.
fn check_token(entries: &[TokenEntry], token: &str, now: u64) -> Result<String, TokenDenied> {
    let Some((id, _)) = parse_token(token) else {
        return Err(TokenDenied::Malformed);
    };
    use subtle::ConstantTimeEq;
    let hash = hash_token(token);
    let entry = entries
        .iter()
        .find(|e| e.id == id && bool::from(e.hash.as_bytes().ct_eq(hash.as_bytes())))
        .ok_or(TokenDenied::Unknown)?;
    if entry.expires.is_some_and(|t| now >= t) {
        return Err(TokenDenied::Expired);
    }
    Ok(entry.name.clone())
}

/// Checks an Authorization header against the tokens file, read fresh. This is what the API calls.
fn authorize(tokens_file: &Path, header: Option<&str>) -> Result<String, TokenDenied> {
    let token = header.and_then(bearer_token).ok_or(TokenDenied::Missing)?;
    let entries = match load_tokens(tokens_file) {
        Ok(entries) if entries.is_empty() => return Err(TokenDenied::NoTokens("no tokens exist".into())),
        Ok(entries) => entries,
        Err(e) => return Err(TokenDenied::NoTokens(e)),
    };
    check_token(&entries, token, now_secs())
}

fn valid_token_name(name: &str) -> bool {
    (1..=64).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// "30d", "12h", "90m" -> seconds; "never" -> None.
fn parse_duration(s: &str) -> Result<Option<u64>, String> {
    if s == "never" {
        return Ok(None);
    }
    let error = || format!("not a duration: {s:?} (use for example 90m, 12h, 30d or never)");
    let unit = match s.chars().last() {
        Some('m') => 60,
        Some('h') => 3600,
        Some('d') => 86400,
        _ => return Err(error()),
    };
    let n: u64 = s[..s.len() - 1].parse().map_err(|_| error())?;
    if n == 0 {
        return Err(error());
    }
    n.checked_mul(unit).map(Some).ok_or_else(error)
}

/// Unix seconds as "2026-12-31 14:05 UTC".
fn utc_date(secs: u64) -> String {
    // Days to civil date, from Howard Hinnant's algorithm.
    let days = (secs / 86400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let rest = secs % 86400;
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        rest / 3600,
        rest % 3600 / 60
    )
}

fn token_entry_from_json(v: &Value) -> Option<TokenEntry> {
    let text = |key: &str| v[key].as_str().map(str::to_string);
    Some(TokenEntry {
        name: text("name").filter(|n| valid_token_name(n))?,
        id: text("id")?,
        hash: text("hash")?,
        expires: v["expires"].as_u64(),
        ttl: v["ttl"].as_u64(),
        rotated: v["rotated"].as_bool().unwrap_or(false),
    })
}

/// Reads tokens.json. A missing file means no tokens; anything unexpected is an error.
fn load_tokens(path: &Path) -> Result<Vec<TokenEntry>, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let broken = |why: &str| format!("{} is broken ({why}); fix or delete it", path.display());
    let doc: Value = serde_json::from_str(&text).map_err(|e| broken(&e.to_string()))?;
    if doc["version"].as_u64() != Some(TOKENS_FILE_VERSION) {
        return Err(broken("unknown version"));
    }
    let list = doc["tokens"].as_array().ok_or_else(|| broken("no token list"))?;
    list.iter()
        .map(|v| token_entry_from_json(v).ok_or_else(|| broken("an entry is missing a field")))
        .collect()
}

/// Writes tokens.json atomically (temp file, then rename), readable only by the owner.
fn save_tokens(path: &Path, entries: &[TokenEntry]) -> Result<(), String> {
    let tokens: Vec<Value> = entries
        .iter()
        .map(|e| {
            json!({
                "name": e.name, "id": e.id, "hash": e.hash,
                "expires": e.expires, "ttl": e.ttl, "rotated": e.rotated,
            })
        })
        .collect();
    let text = serde_json::to_string_pretty(&json!({"version": TOKENS_FILE_VERSION, "tokens": tokens}))
        .map_err(|e| e.to_string())?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let written = options.open(&tmp).and_then(|mut file| {
        file.write_all(text.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()
    });
    let result = written.and_then(|()| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Holds tokens.json.lock while it lives, so two commands cannot overwrite each other's changes.
struct TokensLock(PathBuf);

impl TokensLock {
    fn take(tokens_file: &Path) -> Result<Self, String> {
        let lock = tokens_file.with_extension("json.lock");
        if let Some(dir) = lock.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match fs::OpenOptions::new().write(true).create_new(true).open(&lock) {
                Ok(_) => return Ok(TokensLock(lock)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = fs::metadata(&lock)
                        .and_then(|m| m.modified())
                        .is_ok_and(|t| t.elapsed().is_ok_and(|age| age > STALE_LOCK));
                    if stale {
                        let _ = fs::remove_file(&lock);
                        continue;
                    }
                    if Instant::now() > deadline {
                        return Err(format!(
                            "{} is locked by another phantomqa command",
                            tokens_file.display()
                        ));
                    }
                    sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(format!("cannot lock {}: {e}", tokens_file.display())),
            }
        }
    }
}

impl Drop for TokensLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Runs a change on the token list under the lock. Expired entries are dropped on every change.
fn change_tokens<T>(
    path: &Path,
    now: u64,
    f: impl FnOnce(&mut Vec<TokenEntry>) -> Result<T, String>,
) -> Result<T, String> {
    let _lock = TokensLock::take(path)?;
    let mut entries = load_tokens(path)?;
    entries.retain(|e| e.expires.is_none_or(|t| now < t));
    let result = f(&mut entries)?;
    save_tokens(path, &entries)?;
    Ok(result)
}

/// A new token entry. Returns the entry and the token, which is never stored.
fn new_token(name: &str, ttl: Option<u64>, now: u64, taken: &[TokenEntry]) -> Result<(TokenEntry, String), String> {
    // A repeated id is astronomically unlikely, but cheap to rule out.
    let id = loop {
        let id = random_hex(TOKEN_ID_BYTES)?;
        if !taken.iter().any(|e| e.id == id) {
            break id;
        }
    };
    let token = format!("{TOKEN_PREFIX}{id}_{}", random_hex(TOKEN_SECRET_BYTES)?);
    let entry = TokenEntry {
        name: name.to_string(),
        id,
        hash: hash_token(&token),
        expires: ttl.map(|t| now.saturating_add(t)),
        ttl,
        rotated: false,
    };
    Ok((entry, token))
}

fn token_add(path: &Path, name: &str, ttl: Option<u64>, now: u64) -> Result<String, String> {
    if !valid_token_name(name) {
        return Err(format!("not a valid token name: {name:?} (1-64 of a-z A-Z 0-9 . _ -)"));
    }
    change_tokens(path, now, |entries| {
        if entries.iter().any(|e| e.name == name) {
            return Err(format!(
                "a token named {name:?} exists; use `token rotate {name}` for a new one"
            ));
        }
        let (entry, token) = new_token(name, ttl, now, entries)?;
        entries.push(entry);
        Ok(token)
    })
}

/// Makes a new token for a name. The old one stops at once, or after `grace` seconds.
fn token_rotate(path: &Path, name: &str, grace: u64, ttl: Option<Option<u64>>, now: u64) -> Result<String, String> {
    change_tokens(path, now, |entries| {
        let current = entries
            .iter()
            .find(|e| e.name == name && !e.rotated)
            .or_else(|| entries.iter().find(|e| e.name == name))
            .cloned()
            .ok_or_else(|| format!("no token named {name:?}"))?;
        // Keep the old lifetime unless a new one is given.
        let (entry, token) = new_token(name, ttl.unwrap_or(current.ttl), now, entries)?;
        if grace == 0 {
            entries.retain(|e| e.name != name);
        } else {
            let until = now.saturating_add(grace);
            for e in entries.iter_mut().filter(|e| e.name == name) {
                e.rotated = true;
                e.expires = Some(e.expires.map_or(until, |t| t.min(until)));
            }
        }
        entries.push(entry);
        Ok(token)
    })
}

/// Revokes a name's tokens: all of them, or with `old_only` just the keys kept by a rotation.
fn token_remove(path: &Path, name: &str, old_only: bool, now: u64) -> Result<usize, String> {
    change_tokens(path, now, |entries| {
        let before = entries.len();
        entries.retain(|e| !(e.name == name && (e.rotated || !old_only)));
        match before - entries.len() {
            0 if old_only => Err(format!("{name:?} has no old token from a rotation")),
            0 => Err(format!("no token named {name:?}")),
            removed => Ok(removed),
        }
    })
}

fn render_token_list(entries: &[TokenEntry], now: u64) -> String {
    if entries.is_empty() {
        return "No tokens. Add one with: phantomqa token add NAME\n".into();
    }
    let mut out = format!("{:<24} {:<18} {}\n", "NAME", "ID", "STATUS");
    for e in entries {
        let status = match e.expires {
            Some(t) if now >= t => "expired".to_string(),
            Some(t) if e.rotated => format!("old key, works until {}", utc_date(t)),
            Some(t) => format!("expires {}", utc_date(t)),
            None => "never expires".to_string(),
        };
        out.push_str(&format!("{:<24} {:<18} {status}\n", e.name, e.id));
    }
    out
}

const TOKEN_USAGE: &str = "usage:
  phantomqa token add NAME [--expires 30d]            print a new token (shown only once)
  phantomqa token rotate NAME [--grace 1h] [--expires 30d]
                                                      new token; the old one stops now, or after the grace time
  phantomqa token remove NAME [--old]                 revoke the token (--old: only the key kept by a rotation)
  phantomqa token list                                names, ids and expiry; never the tokens
  phantomqa token check                               read a token from stdin and say if it works

durations: 90m, 12h, 30d, or never (the default for --expires)";

/// The value of `--name VALUE` in a subcommand's arguments.
fn sub_option<'a>(args: &[&'a str], name: &str) -> Result<Option<&'a str>, String> {
    match args.iter().position(|a| *a == name) {
        None => Ok(None),
        Some(i) => args
            .get(i + 1)
            .copied()
            .filter(|v| !v.starts_with("--"))
            .map(Some)
            .ok_or_else(|| format!("{name} needs a value")),
    }
}

/// `phantomqa token ...`. Tokens go to stdout alone, so `TOKEN=$(phantomqa token add ci)` works.
fn token_command(args: &[&str]) -> Result<String, String> {
    let path = &config().tokens;
    let now = now_secs();
    let known = |allowed: &[&str]| -> Result<(), String> {
        let mut rest = args.iter().skip(2);
        while let Some(arg) = rest.next() {
            if !allowed.contains(arg) {
                return Err(format!("unexpected argument: {arg}\n\n{TOKEN_USAGE}"));
            }
            if *arg != "--old" {
                rest.next();
            }
        }
        Ok(())
    };
    match args {
        ["add", name, ..] => {
            known(&["--expires"])?;
            let ttl = sub_option(args, "--expires")?
                .map(parse_duration)
                .transpose()?
                .flatten();
            let token = token_add(path, name, ttl, now)?;
            eprintln!("Token {name:?} created. Copy it now; it is not shown again.");
            Ok(format!("{token}\n"))
        }
        ["rotate", name, ..] => {
            known(&["--grace", "--expires"])?;
            let grace = match sub_option(args, "--grace")? {
                None => 0,
                Some(g) => parse_duration(g)?.ok_or("--grace needs a duration, not never")?,
            };
            let ttl = sub_option(args, "--expires")?.map(parse_duration).transpose()?;
            let token = token_rotate(path, name, grace, ttl, now)?;
            if grace == 0 {
                eprintln!("Token {name:?} rotated. The old token no longer works. Copy the new one now.");
            } else {
                eprintln!(
                    "Token {name:?} rotated. The old token works until {}.",
                    utc_date(now + grace)
                );
            }
            Ok(format!("{token}\n"))
        }
        ["remove", name, ..] => {
            known(&["--old"])?;
            let removed = token_remove(path, name, args.contains(&"--old"), now)?;
            Ok(format!(
                "Removed {removed} token(s) named {name:?}. They no longer work.\n"
            ))
        }
        ["list"] => load_tokens(path).map(|entries| render_token_list(&entries, now)),
        ["check"] => {
            // From stdin, so the token stays out of shell history and the process list.
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).map_err(|e| e.to_string())?;
            match authorize(path, Some(&format!("Bearer {}", input.trim()))) {
                Ok(name) => Ok(format!("ok: token {name:?}\n")),
                Err(denied) => Err(format!("refused: {denied:?}")),
            }
        }
        _ => Err(TOKEN_USAGE.to_string()),
    }
}

// ---------------------------------------------------------------- HTTP API
//
// `phantomqa serve`: a read-only JSON API over the reports folder. Every /v1 route needs a token.
// `route` answers a request without touching the network, so tests call it directly.

const OPENAPI: &str = include_str!("../docs/openapi.json");
/// The routes, written as in openapi.json. A test checks that the two match.
#[cfg(test)]
const ROUTES: [&str; 6] = [
    "/health",
    "/openapi.json",
    "/v1/runs",
    "/v1/runs/{run}",
    "/v1/runs/{run}/files/{name}",
    "/v1/runs/{run}/files/video/{name}",
];
const DEFAULT_LISTEN: &str = "127.0.0.1:7070";
const MAX_URL: usize = 2048;
const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 1000;
const SERVER_THREADS: usize = 8;

enum Body {
    Bytes(Vec<u8>),
    /// `len` bytes of the file, from `start`.
    File {
        file: fs::File,
        start: u64,
        len: u64,
    },
}

struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: Body,
}

impl Reply {
    fn json(status: u16, value: &Value) -> Reply {
        Reply {
            status,
            headers: vec![
                ("Content-Type", "application/json; charset=utf-8".into()),
                ("Cache-Control", "no-store".into()),
            ],
            body: Body::Bytes(serde_json::to_vec_pretty(value).unwrap_or_default()),
        }
    }

    fn error(status: u16, code: &str, message: &str) -> Reply {
        Reply::json(status, &json!({"error": {"code": code, "message": message}}))
    }

    fn not_found(what: &str) -> Reply {
        Reply::error(404, "not_found", &format!("no such {what}"))
    }
}

/// The parts of an HTTP request that `route` needs.
struct ApiRequest<'a> {
    method: &'a str,
    url: &'a str,
    authorization: Option<&'a str>,
    range: Option<&'a str>,
}

/// Answers one request. Also returns the token's name, for the log line.
fn route(req: &ApiRequest, reports: &Path, tokens: &Path) -> (Reply, Option<String>) {
    if req.url.len() > MAX_URL {
        return (Reply::error(414, "uri_too_long", "the URL is too long"), None);
    }
    if req.method != "GET" && req.method != "HEAD" {
        let mut reply = Reply::error(405, "method_not_allowed", "only GET and HEAD are supported");
        reply.headers.push(("Allow", "GET, HEAD".into()));
        return (reply, None);
    }
    let (path, query) = req.url.split_once('?').unwrap_or((req.url, ""));
    let path = if path.len() > 1 {
        path.strip_suffix('/').unwrap_or(path)
    } else {
        path
    };

    match path {
        "/health" => {
            return (
                Reply::json(200, &json!({"ok": true, "version": env!("CARGO_PKG_VERSION")})),
                None,
            );
        }
        "/openapi.json" => {
            let mut reply = Reply::json(200, &Value::Null);
            reply.body = Body::Bytes(OPENAPI.as_bytes().to_vec());
            return (reply, None);
        }
        _ => {}
    }
    let segments: Vec<&str> = path.split('/').skip(1).collect();
    if segments.first() != Some(&"v1") {
        return (Reply::not_found("endpoint"), None);
    }
    // Check the token before anything else, so routes and runs stay hidden without one.
    let name = match authorize(tokens, req.authorization) {
        Ok(name) => name,
        Err(denied) => {
            let _ = writeln!(std::io::stderr(), "phantomqa: refused {}: {denied:?}", path);
            let mut reply = Reply::error(401, "unauthorized", "send a valid token: Authorization: Bearer pqa_...");
            reply
                .headers
                .push(("WWW-Authenticate", "Bearer realm=\"phantomqa\"".into()));
            return (reply, None);
        }
    };
    let reply = match segments[1..] {
        ["runs"] => list_runs_reply(reports, query),
        ["runs", run] => match run_dir(reports, run) {
            Some((n, dir)) => Reply::json(200, &run_detail(&dir, n)),
            None => Reply::not_found("run"),
        },
        ["runs", run, "files", file] => file_reply(reports, run, None, file, req.range),
        ["runs", run, "files", "video", file] => file_reply(reports, run, Some("video"), file, req.range),
        _ => Reply::not_found("endpoint"),
    };
    (reply, Some(name))
}

/// "3" -> 3. Digits only, no sign, at most 9 digits, not zero.
fn run_number(s: &str) -> Option<usize> {
    if s.is_empty() || s.len() > 9 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok().filter(|&n| n > 0)
}

/// The folder of run `n`, if it is a real folder (not a link).
fn run_dir(reports: &Path, run: &str) -> Option<(usize, PathBuf)> {
    let n = run_number(run)?;
    let dir = reports.join(format!("run-{n:03}"));
    fs::symlink_metadata(&dir).ok().filter(|m| m.is_dir()).map(|_| (n, dir))
}

/// All run folders, newest first.
fn list_run_dirs(reports: &Path) -> Vec<(usize, PathBuf)> {
    let mut runs: Vec<(usize, PathBuf)> = fs::read_dir(reports)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let n = run_number(e.file_name().to_str()?.strip_prefix("run-")?)?;
            // Only the canonical name, so "run-03" and "run-003" are not the same run twice.
            (e.file_name().to_str()? == format!("run-{n:03}")).then(|| (n, e.path()))
        })
        .collect();
    runs.sort_by_key(|run| std::cmp::Reverse(run.0));
    runs
}

fn list_runs_reply(reports: &Path, query: &str) -> Reply {
    let mut limit = DEFAULT_LIMIT;
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == "limit" {
            match run_number(value).filter(|&n| n <= MAX_LIMIT) {
                Some(n) => limit = n,
                None => return Reply::error(400, "bad_request", &format!("limit must be 1 to {MAX_LIMIT}")),
            }
        }
    }
    let runs: Vec<Value> = list_run_dirs(reports)
        .into_iter()
        .take(limit)
        .map(|(n, dir)| {
            let run = run_detail(&dir, n);
            let bugs = run["bugs"].as_array().map_or(0, Vec::len);
            json!({"run": n, "status": run["status"], "bugs": bugs})
        })
        .collect();
    Reply::json(200, &json!({"runs": runs}))
}

/// True while someone holds the run's lock (the session, or `phantomqa run`).
fn run_is_active(dir: &Path) -> bool {
    let Ok(file) = fs::File::open(dir.join(RUN_LOCK)) else {
        return false;
    };
    matches!(file.try_lock(), Err(fs::TryLockError::WouldBlock))
}

/// A run's details from its run.json, with a status that also notices killed sessions.
fn run_detail(dir: &Path, n: usize) -> Value {
    let files = run_files(dir);
    let base = |status: &str| json!({"run": n, "status": status, "device": "", "bugs": [], "session_video": null, "files": files});
    let text = match fs::read_to_string(dir.join("run.json")) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return base("unknown"),
        Err(e) => {
            let mut run = base("broken");
            run["error"] = json!(format!("cannot read run.json: {e}"));
            return run;
        }
    };
    let doc = match serde_json::from_str::<Value>(&text) {
        Ok(doc) if doc["version"].as_u64() == Some(RUN_FILE_VERSION) && doc["bugs"].is_array() => doc,
        Ok(_) => {
            let mut run = base("broken");
            run["error"] = json!("run.json has an unknown layout");
            return run;
        }
        Err(e) => {
            let mut run = base("broken");
            run["error"] = json!(format!("run.json is not valid JSON: {e}"));
            return run;
        }
    };
    let status = match doc["status"].as_str() {
        Some("finished") => "finished",
        Some("running") if run_is_active(dir) => "running",
        Some("running") => "interrupted",
        _ => "broken",
    };
    let mut run = json!({
        "run": n,
        "status": status,
        "device": doc["device"].as_str().unwrap_or(""),
        "bugs": doc["bugs"],
        "session_video": doc["session_video"].as_str(),
        "files": files,
    });
    // Runs started by `phantomqa run` also say what was tested and how the agent did.
    for key in ["test", "agent"] {
        if doc[key].is_object() {
            run[key] = doc[key].clone();
        }
    }
    run
}

/// Visible files of a run folder, including video parts, sorted.
fn run_files(dir: &Path) -> Vec<String> {
    let names = |dir: &Path, prefix: &str| -> Vec<String> {
        fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter(|name| valid_file_name(name))
            .map(|name| format!("{prefix}{name}"))
            .collect()
    };
    let mut files = names(dir, "");
    files.extend(names(&dir.join("video"), "video/"));
    files.sort();
    files
}

/// Letters, digits, `.`, `_` and `-`; not hidden (no leading dot), so never `..` or the lock.
fn valid_file_name(name: &str) -> bool {
    (1..=128).contains(&name.len())
        && !name.starts_with('.')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

fn content_type(name: &str) -> &'static str {
    match name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("json") => "application/json; charset=utf-8",
        Some("md") => "text/markdown; charset=utf-8",
        Some("log" | "txt") => "text/plain; charset=utf-8",
        Some("png") => "image/png",
        Some("mp4") => "video/mp4",
        _ => "application/octet-stream",
    }
}

#[derive(Debug, PartialEq)]
enum ByteRange {
    Whole,
    /// First and last byte, both included.
    Part(u64, u64),
    Unsatisfiable,
}

/// One `Range: bytes=...` value. Anything this does not understand returns the whole file,
/// which HTTP allows.
fn parse_range(header: Option<&str>, size: u64) -> ByteRange {
    let Some(spec) = header.and_then(|h| h.trim().strip_prefix("bytes=")) else {
        return ByteRange::Whole;
    };
    if spec.contains(',') {
        return ByteRange::Whole;
    }
    let Some((first, last)) = spec.trim().split_once('-') else {
        return ByteRange::Whole;
    };
    let number = |s: &str| -> Option<u64> {
        (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse().ok())
            .flatten()
    };
    match (number(first), number(last), last.is_empty()) {
        // bytes=-500: the last 500 bytes.
        (None, Some(suffix), _) if first.is_empty() => match (size, suffix) {
            (0, _) | (_, 0) => ByteRange::Unsatisfiable,
            _ => ByteRange::Part(size.saturating_sub(suffix), size - 1),
        },
        // bytes=1024-
        (Some(start), None, true) if start < size => ByteRange::Part(start, size - 1),
        // bytes=0-1023
        (Some(start), Some(end), _) if start <= end && start < size => ByteRange::Part(start, end.min(size - 1)),
        (Some(start), Some(end), _) if start <= end => ByteRange::Unsatisfiable,
        (Some(_), None, true) => ByteRange::Unsatisfiable,
        _ => ByteRange::Whole,
    }
}

fn file_reply(reports: &Path, run: &str, sub: Option<&str>, name: &str, range: Option<&str>) -> Reply {
    let Some((_, dir)) = run_dir(reports, run) else {
        return Reply::not_found("run");
    };
    if !valid_file_name(name) {
        return Reply::not_found("file");
    }
    let path = match sub {
        Some(sub) => dir.join(sub).join(name),
        None => dir.join(name),
    };
    // A link could point outside the reports folder, so only real files are served.
    let Some(size) = fs::symlink_metadata(&path)
        .ok()
        .filter(|m| m.is_file())
        .map(|m| m.len())
    else {
        return Reply::not_found("file");
    };
    let Ok(file) = fs::File::open(&path) else {
        return Reply::not_found("file");
    };
    let mut headers = vec![
        ("Content-Type", content_type(name).to_string()),
        ("Accept-Ranges", "bytes".to_string()),
        ("Cache-Control", "private, no-cache".to_string()),
        ("X-Content-Type-Options", "nosniff".to_string()),
    ];
    let (status, start, len) = match parse_range(range, size) {
        ByteRange::Whole => (200, 0, size),
        ByteRange::Part(first, last) => {
            headers.push(("Content-Range", format!("bytes {first}-{last}/{size}")));
            (206, first, last - first + 1)
        }
        ByteRange::Unsatisfiable => {
            let mut reply = Reply::error(416, "range_not_satisfiable", "the range is outside the file");
            reply.headers.push(("Content-Range", format!("bytes */{size}")));
            return reply;
        }
    };
    Reply {
        status,
        headers,
        body: Body::File { file, start, len },
    }
}

/// `phantomqa serve`: the API and the routines, until stopped. `--no-api`: the routines only.
fn serve(args: &[&str]) -> Result<String, String> {
    let config = config();
    let listen = match args {
        [] => env_var("PHANTOMQA_LISTEN").unwrap_or_else(|| DEFAULT_LISTEN.into()),
        ["--listen", addr] => addr.to_string(),
        ["--no-api"] => return serve_routines_only(),
        _ => {
            return Err(format!(
                "usage: phantomqa serve [--listen {DEFAULT_LISTEN}]  (API and routines)\n       phantomqa serve --no-api           (routines only, no tokens needed)"
            ));
        }
    };
    let addr = std::net::ToSocketAddrs::to_socket_addrs(&listen)
        .map_err(|e| format!("not an address to listen on: {listen:?} ({e})"))?
        .next()
        .ok_or_else(|| format!("not an address to listen on: {listen:?}"))?;

    // Refuse to start an API nobody can use, and say why.
    let active = load_tokens(&config.tokens)?
        .iter()
        .filter(|t| t.expires.is_none_or(|e| now_secs() < e))
        .count();
    if active == 0 {
        return Err(format!(
            "no tokens in {}: create one first with `phantomqa token add NAME`, or run only the routines with `phantomqa serve --no-api`",
            config.tokens.display()
        ));
    }
    fs::create_dir_all(&config.reports).map_err(|e| format!("cannot create {}: {e}", config.reports.display()))?;

    let server = tiny_http::Server::http(addr).map_err(|e| format!("cannot listen on {addr}: {e}"))?;
    let server = Arc::new(server);
    let _ = writeln!(std::io::stderr(), "PhantomQA API on http://{addr}");
    let _ = writeln!(std::io::stderr(), "  reports: {}", config.reports.display());
    let _ = writeln!(
        std::io::stderr(),
        "  tokens:  {} ({active} active)",
        config.tokens.display()
    );
    let _ = writeln!(std::io::stderr(), "  routines: {}", config.routines.display());
    if !addr.ip().is_loopback() {
        let _ = writeln!(
            std::io::stderr(),
            "  warning: listening beyond this machine over plain HTTP; put an HTTPS proxy in front"
        );
    }
    std::thread::Builder::new()
        .spawn(|| run_routines(&config.routines))
        .map_err(|e| format!("cannot start the routines: {e}"))?;
    let workers: Vec<_> = (0..SERVER_THREADS)
        .map(|_| {
            let server = server.clone();
            std::thread::spawn(move || {
                while let Ok(request) = server.recv() {
                    handle_http(request, &config.reports, &config.tokens);
                }
            })
        })
        .collect();
    for worker in workers {
        let _ = worker.join();
    }
    Ok(String::new())
}

/// `phantomqa serve --no-api`: no listener and no tokens, only the routines.
fn serve_routines_only() -> Result<String, String> {
    let config = config();
    // Without the API, a missing file leaves nothing to do: most likely a wrong path.
    if read_routines_file(&config.routines)?.is_none() {
        return Err(format!(
            "{} not found: with --no-api, serve has nothing to do. Create it, or set --routines or PHANTOMQA_ROUTINES.",
            config.routines.display()
        ));
    }
    fs::create_dir_all(&config.reports).map_err(|e| format!("cannot create {}: {e}", config.reports.display()))?;
    let _ = writeln!(std::io::stderr(), "PhantomQA routines (no API)");
    let _ = writeln!(std::io::stderr(), "  reports:  {}", config.reports.display());
    let _ = writeln!(std::io::stderr(), "  routines: {}", config.routines.display());
    run_routines(&config.routines);
    Ok(String::new())
}

/// Turns a tiny_http request into a `route` call and sends the reply.
fn handle_http(request: tiny_http::Request, reports: &Path, tokens: &Path) {
    let header = |name: &'static str| {
        request
            .headers()
            .iter()
            .find(|h| h.field.equiv(name))
            .map(|h| h.value.as_str().to_string())
    };
    let (authorization, range) = (header("Authorization"), header("Range"));
    let method = request.method().as_str().to_string();
    let url = request.url().to_string();
    let req = ApiRequest {
        method: &method,
        url: &url,
        authorization: authorization.as_deref(),
        range: range.as_deref(),
    };
    let (reply, who) = route(&req, reports, tokens);

    let path = url.split('?').next().unwrap_or("");
    let _ = writeln!(
        std::io::stderr(),
        "{method} {path} {} {}",
        reply.status,
        who.as_deref().unwrap_or("-")
    );
    let headers: Vec<tiny_http::Header> = reply
        .headers
        .iter()
        .filter_map(|(k, v)| tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()).ok())
        .collect();
    let status = tiny_http::StatusCode(reply.status);
    // tiny_http leaves out the body itself for HEAD requests. It would switch to chunked transfer above
    // 32 KB and drop Content-Length, which video players and download progress need.
    let result = match reply.body {
        Body::Bytes(bytes) => {
            let len = bytes.len();
            request.respond(
                tiny_http::Response::new(status, headers, std::io::Cursor::new(bytes), Some(len), None)
                    .with_chunked_threshold(usize::MAX),
            )
        }
        Body::File { mut file, start, len } => {
            use std::io::Seek;
            match file.seek(std::io::SeekFrom::Start(start)) {
                Ok(_) => {
                    let len = usize::try_from(len).unwrap_or(usize::MAX);
                    request.respond(
                        tiny_http::Response::new(status, headers, file.take(len as u64), Some(len), None)
                            .with_chunked_threshold(usize::MAX),
                    )
                }
                Err(e) => Err(e),
            }
        }
    };
    if let Err(e) = result {
        // Usually the client went away mid-download.
        let _ = writeln!(std::io::stderr(), "phantomqa: could not send {path}: {e}");
    }
}

// ---------------------------------------------------------------- test runs
//
// `phantomqa run shop.json` tests one app without a person: it installs the app if asked,
// makes the run folder, starts Claude Code with PhantomQA as its only tools, and stops it at
// the time limit. The session itself runs in the MCP worker, which takes over the folder.

const TEST_FIELDS: [&str; 7] = ["app", "apk", "goal", "avoid", "login", "model", "max_minutes"];
const LOGIN_FIELDS: [&str; 2] = ["user", "password_env"];
const DEFAULT_MODEL: &str = "sonnet";
const DEFAULT_MAX_MINUTES: u64 = 30;
const MAX_MINUTES: u64 = 24 * 60;
/// Big APKs on slow devices take a while.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(300);
/// After the agent stops: the last video part plus joining session.mp4.
const FINISH_TIMEOUT: Duration = Duration::from_secs(400);
/// Tells the MCP worker which run folder to take over.
const RUN_DIR_ENV: &str = "PHANTOMQA_RUN_DIR";
/// Held by `phantomqa run` until it has saved the agent's part. Free means phantomqa is gone.
const RUNNER_LOCK: &str = ".runner";
/// How long the worker waits for `phantomqa run` to save the agent's part.
const RUNNER_WAIT: Duration = Duration::from_secs(30);

/// A test file, checked.
#[derive(Debug, PartialEq)]
struct TestFile {
    /// The file name, for reports.
    name: String,
    app: String,
    goal: String,
    /// Relative paths in the file are relative to the file.
    apk: Option<PathBuf>,
    avoid: Vec<String>,
    login: Option<Login>,
    model: String,
    max_minutes: u64,
}

#[derive(Debug, PartialEq)]
struct Login {
    user: String,
    /// The name of the env var with the password. The password itself is never in the file.
    password_env: String,
}

fn load_test(path: &Path) -> Result<TestFile, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let base = path.parent().unwrap_or(Path::new("."));
    let mut test = parse_test(&text, base).map_err(|e| format!("{}: {e}", path.display()))?;
    test.name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(test)
}

fn minutes(n: u64) -> String {
    format!("{n} minute{}", if n == 1 { "" } else { "s" })
}

fn valid_env_name(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Checks every field. Unknown fields are errors, so a typo never goes unnoticed.
fn parse_test(text: &str, base: &Path) -> Result<TestFile, String> {
    let doc: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
    let obj = doc
        .as_object()
        .ok_or("must be a JSON object, like {\"app\": \"com.example.app\", \"goal\": \"...\"}")?;
    let unknown = |obj: &serde_json::Map<String, Value>, fields: &[&str]| -> Result<(), String> {
        match obj.keys().find(|k| !fields.contains(&k.as_str())) {
            Some(key) => Err(format!("unknown field {key:?}; the fields are {}", fields.join(", "))),
            None => Ok(()),
        }
    };
    let text_field = |obj: &serde_json::Map<String, Value>, key: &str| -> Result<Option<String>, String> {
        match obj.get(key) {
            None => Ok(None),
            Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.trim().to_string())),
            Some(_) => Err(format!("{key:?} must be a non-empty string")),
        }
    };
    unknown(obj, &TEST_FIELDS)?;

    let app = text_field(obj, "app")?.ok_or("\"app\" is required: the package name, like com.example.app")?;
    if !valid_package(&app) {
        return Err(format!("\"app\" is not a package name: {app:?}"));
    }
    let goal = text_field(obj, "goal")?.ok_or("\"goal\" is required: what to test, in plain words")?;
    let apk = text_field(obj, "apk")?.map(|apk| base.join(apk));
    let model = text_field(obj, "model")?.unwrap_or_else(|| DEFAULT_MODEL.into());
    if !model.chars().all(|c| c.is_ascii_alphanumeric() || "._-[]".contains(c)) {
        return Err(format!("\"model\" is not a model name: {model:?}"));
    }
    let max_minutes = match obj.get("max_minutes") {
        None => DEFAULT_MAX_MINUTES,
        Some(v) => v.as_u64().filter(|m| (1..=MAX_MINUTES).contains(m)).ok_or(format!(
            "\"max_minutes\" must be a whole number from 1 to {MAX_MINUTES}"
        ))?,
    };
    let avoid_error = "\"avoid\" must be a list of button texts, like [\"Delete account\", \"Pay now\"]";
    let avoid = match obj.get("avoid") {
        None => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .ok_or(avoid_error)
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(avoid_error.into()),
    };
    let login = match obj.get("login") {
        None => None,
        Some(Value::Object(login)) => {
            unknown(login, &LOGIN_FIELDS).map_err(|e| format!("\"login\": {e}"))?;
            let user =
                text_field(login, "user")?.ok_or("\"login\" needs \"user\": the test account's email or user name")?;
            if input_text_args(&user).is_err() {
                return Err("\"login\".\"user\" must be printable ASCII".into());
            }
            let password_env = text_field(login, "password_env")?.ok_or(
                "\"login\" needs \"password_env\": the env var that holds the password, like SHOP_QA_PASSWORD",
            )?;
            if !valid_env_name(&password_env) {
                return Err(format!(
                    "\"password_env\" must be an env var name, like SHOP_QA_PASSWORD, not {password_env:?}. Never put the password itself in the file."
                ));
            }
            Some(Login { user, password_env })
        }
        Some(_) => return Err("\"login\" must be an object: {\"user\": \"...\", \"password_env\": \"...\"}".into()),
    };
    Ok(TestFile {
        name: String::new(),
        app,
        goal,
        apk,
        avoid,
        login,
        model,
        max_minutes,
    })
}

impl TestFile {
    /// What run.json keeps about the test. File names only, no local paths.
    fn to_json(&self) -> Value {
        let file_name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned());
        let mut doc = json!({
            "file": self.name,
            "app": self.app,
            "goal": self.goal,
            "avoid": self.avoid,
            "model": self.model,
            "max_minutes": self.max_minutes,
        });
        if let Some(apk) = self.apk.as_deref().and_then(file_name) {
            doc["apk"] = json!(apk);
        }
        if let Some(login) = &self.login {
            doc["login"] = json!({"user": login.user, "password_env": login.password_env});
        }
        doc
    }

    /// The task for the agent.
    fn prompt(&self) -> String {
        let mut out = format!(
            "You are an AI QA tester. Test the Android app {} on the connected device with the PhantomQA tools.\n\nGoal: {}\n\nHow to work:\n",
            self.app, self.goal
        );
        out.push_str(&format!("- Start with launch_app {}.\n", self.app));
        out.push_str("- Work toward the goal like a careful tester: the main flows, wrong inputs and edge cases.\n");
        out.push_str("- Call report_bug for each problem while it is on screen, with clear steps. Crashes and ANRs are saved automatically; do not report them again.\n");
        if !self.avoid.is_empty() {
            let list: Vec<String> = self.avoid.iter().map(|a| format!("{a:?}")).collect();
            out.push_str(&format!(
                "- Never press these: {}. PhantomQA refuses them.\n",
                list.join(", ")
            ));
        }
        if let Some(login) = &self.login {
            out.push_str(&format!(
                "- When the app asks you to sign in, type {:?} with type_text as the email or user name, and use type_password for the password field. You never see the password.\n",
                login.user
            ));
        }
        out.push_str(&format!(
            "- You have about {}. Stop when the goal is covered.\n- End with a short summary: what you tested, what worked, and the bugs you found.\n",
            minutes(self.max_minutes)
        ));
        out
    }
}

/// Holds the device for one run, so two runs never drive the same device.
fn device_lock() -> Result<fs::File, String> {
    let serial = adb(&["get-serialno"]).map_err(|e| {
        format!("{e}. Start an emulator or connect a phone; with several devices, set --serial or ANDROID_SERIAL.")
    })?;
    let serial = serial.trim();
    if serial.is_empty() || serial == "unknown" {
        return Err("no device found. Start an emulator or connect a phone.".into());
    }
    let safe: String = serial
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = std::env::temp_dir().join(format!("phantomqa-device-{safe}.lock"));
    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(fs::TryLockError::WouldBlock) => Err(format!("device {serial} is busy with another phantomqa run")),
        Err(fs::TryLockError::Error(e)) => Err(format!("cannot lock {}: {e}", path.display())),
    }
}

fn install_apk(apk: &Path, app: &str) -> Result<(), String> {
    if !apk.is_file() {
        return Err(format!("apk not found: {}", apk.display()));
    }
    let path = apk.to_str().ok_or("the apk path must be valid UTF-8")?;
    // -r: replace the installed app and keep its data. -t: allow debug builds marked test-only.
    let out = adb_bytes_for(&["install", "-r", "-t", path], INSTALL_TIMEOUT)
        .map(|out| String::from_utf8_lossy(&out).into_owned())
        .unwrap_or_else(|e| e);
    if !out.contains("Success") {
        let hint = if out.contains("UPDATE_INCOMPATIBLE") {
            " The installed app has a different signature: uninstall it first (this deletes its data)."
        } else {
            ""
        };
        return Err(format!(
            "could not install the apk: {}.{hint}",
            one_line(out.trim(), 300)
        ));
    }
    if !installed(app) {
        return Err(format!(
            "the apk installed, but {app} is still not on the device: check that \"app\" is the apk's package name"
        ));
    }
    Ok(())
}

/// MCP config for the agent: this binary, with the same options, taking over the run folder.
fn mcp_config(run_dir: &Path) -> Result<Value, String> {
    let config = config();
    let exe = std::env::current_exe().map_err(|e| format!("cannot find the phantomqa binary: {e}"))?;
    let mut args = vec![
        "--adb".to_string(),
        config.adb.clone(),
        "--reports".into(),
        config.reports.to_string_lossy().into_owned(),
    ];
    if let Some(serial) = &config.serial {
        args.extend(["--serial".into(), serial.clone()]);
    }
    if let Some(ffmpeg) = &config.ffmpeg {
        args.extend(["--ffmpeg".into(), ffmpeg.clone()]);
    }
    if !config.video {
        args.push("--no-video".into());
    }
    if !config.screenshots {
        args.push("--no-screenshots".into());
    }
    args.push("mcp".into());
    Ok(json!({"mcpServers": {"phantomqa": {
        "type": "stdio",
        "command": exe,
        "args": args,
        "env": {RUN_DIR_ENV: run_dir},
    }}}))
}

/// How the agent's part went.
struct AgentRun {
    /// "done", "time limit", "error", or "stopped" when phantomqa itself stopped during the run.
    ended: &'static str,
    summary: String,
    cost_usd: Option<f64>,
    turns: Option<u64>,
    error: Option<String>,
}

/// Starts Claude Code with PhantomQA as its only tools and waits, up to the time limit.
fn run_claude(test: &TestFile, run_dir: &Path, mcp_file: &Path) -> AgentRun {
    let claude = &config().claude;
    let failed = |error: String| AgentRun {
        ended: "error",
        summary: String::new(),
        cost_usd: None,
        turns: None,
        error: Some(error),
    };
    let mut command = Command::new(claude);
    command
        .args(["-p", &test.prompt(), "--model", &test.model, "--mcp-config"])
        .arg(mcp_file)
        .args([
            "--strict-mcp-config",
            // No built-in tools (no shell, no files): only PhantomQA's.
            "--tools",
            "",
            "--allowedTools",
            "mcp__phantomqa",
            "--permission-prompts",
            "none",
            // Leave out the user's own settings and hooks, so every run behaves the same.
            "--setting-sources",
            "",
            "--no-session-persistence",
            "--output-format",
            "json",
        ])
        // Connect the MCP server before the first turn. By default Claude Code connects it in
        // the background, and a fast model can answer before the tools exist.
        .env("MCP_CONNECTION_NONBLOCKING", "0")
        .current_dir(run_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    stop_with_parent(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return failed(format!("cannot run {claude}: {e}")),
    };
    let (sender, receiver) = mpsc::channel();
    for (n, pipe) in [
        child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
        child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .enumerate()
    {
        let sender = sender.clone();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            let _ = sender.send((n, buf));
        });
    }
    let limit = Duration::from_secs(test.max_minutes * 60);
    let start = Instant::now();
    let (status, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (Some(status), false),
            Ok(None) if start.elapsed() > limit => {
                // The worker sees the agent go and finishes the session by itself.
                let _ = child.kill();
                let _ = child.wait();
                break (None, true);
            }
            Ok(None) => sleep(Duration::from_millis(200)),
            Err(e) => return failed(format!("lost track of {claude}: {e}")),
        }
    };
    // A helper of the agent may keep a pipe open; do not wait for it forever.
    let mut out = [Vec::new(), Vec::new()];
    for _ in 0..2 {
        if let Ok((n, buf)) = receiver.recv_timeout(Duration::from_secs(5)) {
            out[n] = buf;
        }
    }
    let stderr = String::from_utf8_lossy(&out[1]);
    match serde_json::from_slice::<Value>(&out[0]) {
        Ok(doc) if doc.is_object() => {
            let is_error = doc["is_error"].as_bool().unwrap_or(false);
            let summary = doc["result"].as_str().unwrap_or("").trim().to_string();
            AgentRun {
                ended: if is_error { "error" } else { "done" },
                error: is_error.then(|| {
                    let reason = if summary.is_empty() {
                        doc["subtype"].as_str().unwrap_or("unknown")
                    } else {
                        &summary
                    };
                    one_line(reason, 500)
                }),
                summary,
                cost_usd: doc["total_cost_usd"].as_f64(),
                turns: doc["num_turns"].as_u64(),
            }
        }
        _ if timed_out => AgentRun {
            ended: "time limit",
            summary: String::new(),
            cost_usd: None,
            turns: None,
            error: None,
        },
        _ => {
            let detail = match status {
                Some(status) if !stderr.trim().is_empty() => {
                    format!("{claude} failed ({status}): {}", one_line(stderr.trim(), 500))
                }
                Some(status) => format!("{claude} failed ({status}) without saying why"),
                None => format!("{claude} failed"),
            };
            failed(detail)
        }
    }
}

/// Linux: the agent gets SIGTERM when the thread that started it dies, for example when phantomqa
/// is killed. Otherwise the agent would go on without a time limit.
#[cfg(target_os = "linux")]
fn stop_with_parent(command: &mut Command) {
    use std::ffi::{c_int, c_ulong};
    use std::os::unix::process::CommandExt;
    unsafe extern "C" {
        fn prctl(option: c_int, ...) -> c_int;
    }
    const PR_SET_PDEATHSIG: c_int = 1;
    const SIGTERM: c_ulong = 15;
    const ESRCH: i32 = 3;
    let parent = std::process::id();
    // SAFETY: this runs between fork and exec. It only makes system calls, with no allocation.
    unsafe {
        command.pre_exec(move || {
            if prctl(PR_SET_PDEATHSIG, SIGTERM) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // phantomqa died before prctl: do not start the agent at all.
            if std::os::unix::process::parent_id() != parent {
                return Err(std::io::Error::from_raw_os_error(ESRCH));
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn stop_with_parent(_: &mut Command) {}

/// Adds the agent's part to run.json and report.md. Returns the number of bugs.
fn save_agent(dir: &Path, model: &str, agent: &AgentRun) -> usize {
    let mut doc = read_json(&dir.join("run.json")).unwrap_or_else(|_| json!({}));
    doc["agent"] = json!({
        "name": "claude",
        "model": model,
        "ended": agent.ended,
        "summary": agent.summary,
        "cost_usd": agent.cost_usd,
        "turns": agent.turns,
        "error": agent.error,
    });
    if let Err(e) = write_json(&dir.join("run.json"), &doc) {
        warn(&e);
    }
    let mut section = format!("\n## Agent\n\n- Model: {model}\n- Ended: {}\n", agent.ended);
    if let Some(cost) = agent.cost_usd {
        section.push_str(&format!("- Cost: ${cost:.2}, {} turns\n", agent.turns.unwrap_or(0)));
    }
    if let Some(error) = &agent.error {
        section.push_str(&format!("- Error: {error}\n"));
    }
    if !agent.summary.is_empty() {
        section.push_str(&format!("\n{}\n", agent.summary));
    }
    let _ = fs::OpenOptions::new()
        .append(true)
        .open(dir.join("report.md"))
        .and_then(|mut file| file.write_all(section.as_bytes()));
    doc["bugs"].as_array().map_or(0, Vec::len)
}

/// `phantomqa run` holds this lock for the whole run, until the agent's part is saved.
fn hold_runner_lock(dir: &Path) -> Result<fs::File, String> {
    let path = dir.join(RUNNER_LOCK);
    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    file.try_lock()
        .map_err(|e| format!("cannot lock {}: {e}", path.display()))?;
    Ok(file)
}

/// The worker outlives `phantomqa run`. When phantomqa stops during a run (killed, or serve
/// stopped), the agent stops with it and nobody else saves the agent's part, so the worker
/// saves it as "stopped".
fn stand_in_for_runner(dir: &Path, model: &str) {
    let has_agent = || read_json(&dir.join("run.json")).is_ok_and(|doc| doc["agent"].is_object());
    // No lock file: not a run of this version; nothing to watch.
    let Ok(file) = fs::OpenOptions::new().write(true).open(dir.join(RUNNER_LOCK)) else {
        return;
    };
    let start = Instant::now();
    while start.elapsed() < RUNNER_WAIT && !has_agent() {
        // Free only when phantomqa is gone. Holding it, the check and the write cannot race with it.
        if file.try_lock().is_ok() {
            if !has_agent() {
                let _ = fs::remove_file(dir.join(".mcp.json"));
                let stopped = AgentRun {
                    ended: "stopped",
                    summary: String::new(),
                    cost_usd: None,
                    turns: None,
                    error: Some("phantomqa stopped during the run, so the agent stopped too".into()),
                };
                save_agent(dir, model, &stopped);
            }
            return;
        }
        sleep(Duration::from_millis(200));
    }
}

/// Waits until the worker has finished the session, or is gone.
fn wait_for_finish(run_dir: &Path) -> bool {
    let start = Instant::now();
    loop {
        if read_json(&run_dir.join("run.json")).is_ok_and(|doc| doc["status"] == "finished") {
            return true;
        }
        // A worker that never started (the agent failed early) never takes the lock.
        let gone = !run_is_active(run_dir) && start.elapsed() > Duration::from_secs(3);
        if gone || start.elapsed() > FINISH_TIMEOUT {
            return false;
        }
        sleep(Duration::from_millis(250));
    }
}

/// Reads a test file and checks that its password env var is ready, before anything starts.
fn check_test(path: &Path) -> Result<TestFile, String> {
    let test = load_test(path)?;
    if let Some(login) = &test.login {
        let name = &login.password_env;
        let password = env_var(name).ok_or_else(|| {
            format!("{name} is not set. Set it to the test account password before the run: export {name}='...'")
        })?;
        if input_text_args(&password).is_err() {
            return Err(format!(
                "{name} has characters PhantomQA cannot type: printable ASCII only"
            ));
        }
    }
    Ok(test)
}

/// `phantomqa run FILE`, and each routine. Returns the summary and the exit code: 0 no bugs,
/// 1 bugs, 2 failed. `routine` is the routine that started it, for run.json.
fn run_test(path: &Path, routine: Option<&str>) -> Result<(String, i32), String> {
    let config = config();
    let test = check_test(path)?;
    first_line(&config.claude, "--version")
        .map_err(|e| format!("{e}. Install Claude Code, or set --claude or PHANTOMQA_CLAUDE."))?;
    let _device = device_lock()?;
    match &test.apk {
        Some(apk) => install_apk(apk, &test.app)?,
        None if !installed(&test.app) => {
            return Err(format!(
                "{} is not installed on the device. Install it, or set \"apk\" in the test file.",
                test.app
            ));
        }
        None => {}
    }

    let mut info = test.to_json();
    if let Some(name) = routine {
        info["routine"] = json!(name);
    }
    let mut report = create_run(&config.reports, info)?;
    let dir = report.dir.clone();
    // Held to the end: while it is held, the worker knows phantomqa will add the agent's part.
    let _runner = hold_runner_lock(&dir)?;
    let mcp_file = dir.join(".mcp.json");
    write_json(&mcp_file, &mcp_config(&dir)?)?;
    warn(&format!(
        "run {}: testing {} with {} for up to {}",
        report.run,
        test.app,
        test.model,
        minutes(test.max_minutes)
    ));
    let mut agent = run_claude(&test, &dir, &mcp_file);
    // From here on only the worker holds the lock, while it finishes the session.
    report.lock = None;
    let finished = wait_for_finish(&dir);
    let _ = fs::remove_file(&mcp_file);
    // The session starts with the first tool call. No session means the agent did no testing.
    if !finished && agent.ended == "done" {
        agent.ended = "error";
        agent.error = Some(
            "the test session did not finish: the agent made no PhantomQA tool calls, or the session was killed".into(),
        );
    }

    let mut state = State::new(config.reports.clone());
    state.password_env = test.login.as_ref().map(|l| l.password_env.clone());
    agent.summary = redact(&state, std::mem::take(&mut agent.summary));
    let bugs = save_agent(&dir, &test.model, &agent);

    let mut line = format!("run {}: {bugs} bug{}", report.run, if bugs == 1 { "" } else { "s" });
    line.push_str(&format!(", agent {}", agent.ended));
    if let Some(cost) = agent.cost_usd {
        line.push_str(&format!(" (${cost:.2})"));
    }
    if !finished {
        line.push_str(", session did not finish");
    }
    if let Some(error) = &agent.error {
        line.push_str(&format!("\nerror: {error}"));
    }
    line.push_str(&format!("\nreport: {}\n", dir.join("report.md").display()));
    let code = match (agent.ended, bugs) {
        (_, n) if n > 0 => 1,
        ("error", _) => 2,
        _ => 0,
    };
    Ok((line, code))
}

// ---------------------------------------------------------------- routines
//
// `phantomqa serve` also runs the tests in routines.json on a schedule:
//
//   {"routines": [
//     {"name": "nightly", "test": "shop.json", "at": "03:00"},
//     {"name": "smoke", "test": "smoke.json", "every": "6h"}
//   ]}
//
// Times are the machine's local time (TZ, then /etc/localtime). `every` counts from midnight, so
// "6h" is 00:00, 06:00, 12:00 and 18:00. serve reads the file every second; a broken file runs
// nothing until it is fixed. Times that pass while the machine sleeps or serve is down are not
// made up. A routine whose last run is still going skips its turn, and so does a busy device.

const ROUTINE_FIELDS: [&str; 4] = ["name", "test", "at", "every"];
/// A routine starts if its time passed at most this many seconds ago. Older times count as missed.
const ROUTINE_GRACE: i64 = 60;
const MAX_ROUTINES_FILE: u64 = 1024 * 1024;
const DAY_MINUTES: u32 = 24 * 60;
const ROUTINES_EXAMPLE: &str = r#"{"routines": [{"name": "nightly", "test": "shop.json", "at": "03:00"}]}"#;

/// When a routine runs, on the local wall clock.
#[derive(Debug, Clone, Copy, PartialEq)]
enum When {
    /// Once a day, at this minute of the day.
    At(u32),
    /// Every this many minutes from midnight. It splits a day evenly.
    Every(u32),
}

impl When {
    fn describe(self) -> String {
        match self {
            When::At(m) => format!("at {:02}:{:02}", m / 60, m % 60),
            When::Every(n) if n % 60 == 0 => format!("every {}h", n / 60),
            When::Every(n) => format!("every {n}m"),
        }
    }

    /// The time between two runs.
    fn period(self) -> Duration {
        let minutes = match self {
            When::At(_) => DAY_MINUTES,
            When::Every(n) => n,
        };
        Duration::from_secs(u64::from(minutes) * 60)
    }

    /// The minutes of a day it runs at.
    fn minutes(self) -> Vec<u32> {
        match self {
            When::At(m) => vec![m],
            When::Every(n) => (0..DAY_MINUTES).step_by(n as usize).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Routine {
    name: String,
    /// Relative paths in routines.json are relative to routines.json.
    test: PathBuf,
    when: When,
}

/// The text of routines.json. Ok(None) when there is no such file.
fn read_routines_file(path: &Path) -> Result<Option<String>, String> {
    let shown = path.display();
    let missing = |e: &std::io::Error| e.kind() == std::io::ErrorKind::NotFound;
    match fs::metadata(path) {
        Err(e) if missing(&e) => return Ok(None),
        Err(e) => return Err(format!("cannot read {shown}: {e}")),
        Ok(meta) if !meta.is_file() => return Err(format!("{shown} is not a file")),
        Ok(meta) if meta.len() > MAX_ROUTINES_FILE => return Err(format!("{shown} is bigger than 1 MB")),
        Ok(_) => {}
    }
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if missing(&e) => Ok(None),
        Err(e) => Err(format!("cannot read {shown}: {e}")),
    }
}

/// Checks every routine. Unknown fields are errors, and so is a name used twice.
fn parse_routines(text: &str, base: &Path) -> Result<Vec<Routine>, String> {
    let doc: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
    let obj = doc
        .as_object()
        .ok_or_else(|| format!("must be a JSON object, like {ROUTINES_EXAMPLE}"))?;
    if let Some(key) = obj.keys().find(|k| *k != "routines") {
        return Err(format!("unknown field {key:?}; the file has only \"routines\""));
    }
    let Some(Value::Array(items)) = obj.get("routines") else {
        return Err(format!("\"routines\" must be a list, like {ROUTINES_EXAMPLE}"));
    };
    let mut routines: Vec<Routine> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let which = match item["name"].as_str() {
            Some(name) => format!("routine {name:?}"),
            None => format!("routine {}", i + 1),
        };
        let routine = parse_routine(item, base).map_err(|e| format!("{which}: {e}"))?;
        if routines.iter().any(|r| r.name == routine.name) {
            return Err(format!("{which}: the name is used twice"));
        }
        routines.push(routine);
    }
    Ok(routines)
}

fn parse_routine(item: &Value, base: &Path) -> Result<Routine, String> {
    let obj = item
        .as_object()
        .ok_or("must be an object, like {\"name\": \"nightly\", \"test\": \"shop.json\", \"at\": \"03:00\"}")?;
    if let Some(key) = obj.keys().find(|k| !ROUTINE_FIELDS.contains(&k.as_str())) {
        return Err(format!(
            "unknown field {key:?}; the fields are {}",
            ROUTINE_FIELDS.join(", ")
        ));
    }
    let text = |key: &str| -> Result<Option<String>, String> {
        match obj.get(key) {
            None => Ok(None),
            Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.trim().to_string())),
            Some(_) => Err(format!("{key:?} must be a non-empty string")),
        }
    };
    let name = text("name")?.ok_or("\"name\" is required, like \"nightly\"")?;
    if !valid_token_name(&name) {
        return Err("\"name\" may have only letters, digits, '.', '_' and '-', up to 64".into());
    }
    let test = text("test")?.ok_or("\"test\" is required: the test file, like \"shop.json\"")?;
    let when = match (text("at")?, text("every")?) {
        (Some(at), None) => When::At(parse_at(&at)?),
        (None, Some(every)) => When::Every(parse_every(&every)?),
        (Some(_), Some(_)) => return Err("set \"at\" or \"every\", not both".into()),
        (None, None) => return Err("needs \"at\", like \"03:00\", or \"every\", like \"6h\"".into()),
    };
    Ok(Routine {
        name,
        test: base.join(test),
        when,
    })
}

/// "03:00" or "3:00" -> minute of the day.
fn parse_at(s: &str) -> Result<u32, String> {
    let error = || format!("\"at\" must be a time of day from 00:00 to 23:59, like \"03:00\", not {s:?}");
    let digits = |part: &str, len: std::ops::RangeInclusive<usize>| {
        len.contains(&part.len()) && part.bytes().all(|b| b.is_ascii_digit())
    };
    let (hour, minute) = s.split_once(':').ok_or_else(error)?;
    if !digits(hour, 1..=2) || !digits(minute, 2..=2) {
        return Err(error());
    }
    let (hour, minute): (u32, u32) = (hour.parse().map_err(|_| error())?, minute.parse().map_err(|_| error())?);
    if hour > 23 || minute > 59 {
        return Err(error());
    }
    Ok(hour * 60 + minute)
}

/// "15m" or "6h" -> minutes. It must split a day evenly, so the times are the same every day.
fn parse_every(s: &str) -> Result<u32, String> {
    let error = || {
        format!(
            "\"every\" must be minutes or hours that split a day evenly, like \"15m\", \"30m\", \"2h\" or \"6h\", not {s:?}. For once a day, use \"at\"."
        )
    };
    let unit = match s.chars().last() {
        Some('m') => 1,
        Some('h') => 60,
        _ => return Err(error()),
    };
    let number = &s[..s.len() - 1];
    if !(1..=4).contains(&number.len()) || !number.bytes().all(|b| b.is_ascii_digit()) {
        return Err(error());
    }
    let minutes = number.parse::<u32>().map_err(|_| error())? * unit;
    if minutes == 0 || minutes >= DAY_MINUTES || !DAY_MINUTES.is_multiple_of(minutes) {
        return Err(error());
    }
    Ok(minutes)
}

/// The machine's time zone: TZ, then /etc/localtime. UTC, and why, when there is none.
fn local_zone() -> (TimeZone, Option<String>) {
    match TimeZone::try_system() {
        Ok(tz) => (tz, None),
        Err(e) => (
            TimeZone::UTC,
            Some(format!("cannot tell the local time zone ({e}), so routines use UTC")),
        ),
    }
}

/// "Asia/Tbilisi (+04:00)", for messages.
fn zone_label(tz: &TimeZone, now: Timestamp) -> String {
    let offset = now.to_zoned(tz.clone()).strftime("%:z").to_string();
    match tz.iana_name() {
        Some(name) => format!("{name} ({offset})"),
        None => format!("UTC{offset}"),
    }
}

/// "2026-11-12 03:00" on the local clock.
fn local_time(t: Timestamp, tz: &TimeZone) -> String {
    t.to_zoned(tz.clone()).strftime("%Y-%m-%d %H:%M").to_string()
}

/// The times a routine runs, from the day before `t` to two days after, on the zone's clock.
/// A time that a clock change skips runs right after the change. A time that happens twice
/// runs the first time.
fn run_times(when: When, tz: &TimeZone, t: Timestamp) -> Vec<Timestamp> {
    let today = t.to_zoned(tz.clone()).date();
    let mut times = Vec::new();
    for days in -1..=2 {
        let Ok(date) = today.checked_add(jiff::Span::new().days(days)) else {
            continue;
        };
        for minute in when.minutes() {
            let wall = date.at((minute / 60) as i8, (minute % 60) as i8, 0, 0);
            if let Ok(time) = tz.to_ambiguous_timestamp(wall).compatible() {
                times.push(time);
            }
        }
    }
    times.sort();
    times.dedup();
    times
}

/// The last run time at or before `t`.
fn last_time(when: When, tz: &TimeZone, t: Timestamp) -> Option<Timestamp> {
    run_times(when, tz, t).into_iter().rfind(|time| *time <= t)
}

/// The first run time after `t`.
fn next_time(when: When, tz: &TimeZone, t: Timestamp) -> Option<Timestamp> {
    run_times(when, tz, t).into_iter().find(|time| *time > t)
}

/// Where one routine stands.
#[derive(Debug, Clone, PartialEq)]
struct Timer {
    /// The run time handled last: started, skipped or missed.
    last: Option<Timestamp>,
    /// When `last` was handled, as time since serve started. The wall clock can jump; this cannot.
    handled: Option<Duration>,
    next: Option<Timestamp>,
}

#[derive(Debug, PartialEq)]
enum Due {
    Run(Timestamp),
    Missed(Timestamp),
}

/// A new routine waits for its next time, even when one passed a moment ago.
fn new_timer(when: When, tz: &TimeZone, now: Timestamp) -> Timer {
    Timer {
        last: last_time(when, tz, now),
        handled: None,
        next: next_time(when, tz, now),
    }
}

/// One check, every second. `before` is the time of the check before, and `uptime` the time since
/// serve started, from a clock that never jumps.
fn tick(
    timer: &mut Timer,
    when: When,
    tz: &TimeZone,
    before: Timestamp,
    now: Timestamp,
    uptime: Duration,
) -> Option<Due> {
    if now < before {
        // The clock went back. Wait for the next time from here.
        timer.next = next_time(when, tz, now);
    }
    let Some(next) = timer.next else {
        timer.next = next_time(when, tz, now);
        return None;
    };
    if now < next {
        return None;
    }
    timer.next = next_time(when, tz, now);
    let time = last_time(when, tz, now)?;
    // The same time again means the clock went back. A small jump would run it twice within
    // minutes, so skip it. A big one (a clock that was a day ahead, set right) runs it again,
    // since a real period has passed.
    let recent = timer
        .handled
        .is_some_and(|at| uptime.saturating_sub(at) < when.period() / 2);
    if timer.last == Some(time) && recent {
        return None;
    }
    timer.last = Some(time);
    timer.handled = Some(uptime);
    // After sleep, or a clock jump forward, only the latest passed time counts, once.
    if now.as_second() - time.as_second() > ROUTINE_GRACE {
        Some(Due::Missed(time))
    } else {
        Some(Due::Run(time))
    }
}

/// Takes a routine off the running list when its thread ends, even by a panic.
struct Running(Arc<Mutex<Vec<String>>>, String);

impl Drop for Running {
    fn drop(&mut self) {
        let mut list = self.0.lock().unwrap_or_else(|e| e.into_inner());
        list.retain(|name| *name != self.1);
    }
}

fn start_routine(routine: &Routine, running: &Arc<Mutex<Vec<String>>>) {
    let name = routine.name.clone();
    {
        let mut list = running.lock().unwrap_or_else(|e| e.into_inner());
        if list.contains(&name) {
            warn(&format!("routine {name}: skipped, its last run is still going"));
            return;
        }
        list.push(name.clone());
    }
    let guard = Running(running.clone(), name.clone());
    let test = routine.test.clone();
    warn(&format!("routine {name}: starting {}", test.display()));
    let started = std::thread::Builder::new().spawn(move || {
        let _guard = guard;
        let outcome = match run_test(&test, Some(&name)) {
            Ok((text, _)) => text.trim().replace('\n', "; "),
            Err(e) => format!("did not run: {e}"),
        };
        warn(&format!("routine {name}: {outcome}"));
    });
    if let Err(e) = started {
        warn(&format!(
            "routine {}: did not run: cannot start a thread: {e}",
            routine.name
        ));
    }
}

/// Runs the routines in `path` until serve stops. Says what it does on stderr.
fn run_routines(path: &Path) {
    let base = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let running: Arc<Mutex<Vec<String>>> = Arc::default();
    // The file as last read, to notice changes.
    let mut seen: Option<Result<Option<String>, String>> = None;
    let mut routines: Vec<Routine> = Vec::new();
    let mut timers: Vec<(String, When, Timer)> = Vec::new();
    let (mut tz, mut zone_problem) = local_zone();
    if let Some(problem) = &zone_problem {
        warn(&format!("routines: {problem}"));
    }
    let mut before = Timestamp::now();
    let started = Instant::now();
    loop {
        let now = Timestamp::now();
        let uptime = started.elapsed();
        let (zone, problem) = local_zone();
        if problem != zone_problem {
            if let Some(problem) = &problem {
                warn(&format!("routines: {problem}"));
            }
            zone_problem = problem;
        }
        if zone != tz {
            tz = zone;
            warn(&format!("routines: the time zone is now {}", zone_label(&tz, now)));
            for (_, when, timer) in &mut timers {
                timer.next = next_time(*when, &tz, before.min(now));
            }
        }

        let file = read_routines_file(path);
        if seen.as_ref() != Some(&file) {
            let shown = path.display();
            routines = match &file {
                Ok(None) => {
                    warn(&format!("routines: none, {shown} not found"));
                    Vec::new()
                }
                Ok(Some(text)) => match parse_routines(text, &base) {
                    Ok(list) => {
                        let names: Vec<String> = list
                            .iter()
                            .map(|r| format!("{} ({})", r.name, r.when.describe()))
                            .collect();
                        warn(&format!(
                            "routines: {} from {shown}, times in {}{}{}",
                            list.len(),
                            zone_label(&tz, now),
                            if list.is_empty() { "" } else { ": " },
                            names.join(", ")
                        ));
                        list
                    }
                    Err(e) => {
                        warn(&format!(
                            "routines: {shown}: {}. Running none until it is fixed.",
                            e.trim_end_matches('.')
                        ));
                        Vec::new()
                    }
                },
                Err(e) => {
                    warn(&format!(
                        "routines: {}. Running none until it is fixed.",
                        e.trim_end_matches('.')
                    ));
                    Vec::new()
                }
            };
            // A routine keeps its place if its schedule did not change. New ones wait for their next time.
            timers.retain(|(name, when, _)| routines.iter().any(|r| r.name == *name && r.when == *when));
            seen = Some(file);
        }

        for routine in &routines {
            let i = match timers.iter().position(|(name, _, _)| *name == routine.name) {
                Some(i) => i,
                None => {
                    timers.push((routine.name.clone(), routine.when, new_timer(routine.when, &tz, now)));
                    timers.len() - 1
                }
            };
            match tick(&mut timers[i].2, routine.when, &tz, before, now, uptime) {
                Some(Due::Run(_)) => start_routine(routine, &running),
                Some(Due::Missed(time)) => warn(&format!(
                    "routine {}: missed {} (the machine was asleep, or its clock jumped); waiting for the next time",
                    routine.name,
                    local_time(time, &tz)
                )),
                None => {}
            }
        }
        before = now;
        sleep(Duration::from_secs(1));
    }
}

/// `phantomqa routines`: checks routines.json and each test file, and says when each runs next.
fn routines_check() -> bool {
    let path = &config().routines;
    let now = Timestamp::now();
    let (tz, problem) = local_zone();
    match problem {
        Some(problem) => say(&format!(
            "warn  time zone: {problem}\n      fix: set TZ to a zone name, like TZ=Europe/Berlin, or unset it"
        )),
        None => say(&format!(
            "info  times are local: {}, now {}",
            zone_label(&tz, now),
            local_time(now, &tz)
        )),
    }
    let base = path.parent().unwrap_or(Path::new("."));
    let routines = read_routines_file(path)
        .and_then(|text| text.ok_or_else(|| format!("{} not found", path.display())))
        .and_then(|text| parse_routines(&text, base).map_err(|e| format!("{}: {e}", path.display())));
    let routines = match routines {
        Ok(routines) => routines,
        Err(e) => {
            say(&format!(
                "FAIL  {e}\n      fix: see {ROUTINES_EXAMPLE}; set --routines or PHANTOMQA_ROUTINES for another file"
            ));
            return false;
        }
    };
    if routines.is_empty() {
        say(&format!("info  {}: no routines", path.display()));
    }
    let mut ok = true;
    for routine in &routines {
        let next = next_time(routine.when, &tz, now).map_or("never".into(), |t| local_time(t, &tz));
        let what = format!("{}: {}, next {next}", routine.name, routine.when.describe());
        match check_test(&routine.test) {
            Ok(test) => say(&format!("ok    {what}: tests {} with {}", test.app, test.model)),
            Err(e) => {
                ok = false;
                say(&format!("FAIL  {what}\n      {e}"));
            }
        }
    }
    ok
}

// ---------------------------------------------------------------- doctor

/// Prints one line per check. Returns false if something required is missing.
fn doctor() -> bool {
    let config = config();
    let mut ok = true;
    let mut check = |required: bool, what: &str, result: Result<String, String>, fix: &str| match result {
        Ok(detail) => say(&format!("ok    {what}: {detail}")),
        Err(error) => {
            let status = if required { "FAIL" } else { "warn" };
            say(&format!("{status:<5} {what}: {error}\n      fix: {fix}"));
            ok &= !required;
        }
    };

    let adb_version = adb(&["version"]).map(|out| out.lines().next().unwrap_or("").trim().to_string());
    let adb_works = adb_version.is_ok();
    check(
        true,
        "adb",
        adb_version.map(|v| format!("{v} ({})", config.adb)),
        "install Android platform-tools, or set --adb, ADB or ANDROID_HOME",
    );
    if adb_works {
        let state = adb(&["get-state"]).map(|s| s.trim().to_string()).and_then(|s| {
            if s == "device" {
                Ok(s)
            } else {
                Err(format!("device is {s:?}"))
            }
        });
        let device_works = state.is_ok();
        check(
            true,
            "device",
            state.and_then(|_| device_name()),
            "start an emulator or connect a phone with USB debugging; with several devices, set --serial or ANDROID_SERIAL",
        );
        if device_works {
            let screen = dump_xml().map(|xml| format!("works ({} elements on screen)", parse_screen(&xml).1.len()));
            check(true, "screen reading", screen, "unlock the device screen and try again");
            let recorder = adb(&["shell", "command -v screenrecord"])
                .map(|p| p.trim().to_string())
                .and_then(|p| if p.is_empty() { Err("not found".into()) } else { Ok(p) });
            check(
                false,
                "screenrecord",
                recorder,
                "video needs screenrecord on the device; testing still works without it",
            );
        }
    }

    let ffmpeg = match &config.ffmpeg {
        Some(path) => first_line(path, "-version").map(|v| format!("{v} ({path})")),
        None => Err("not found".into()),
    };
    check(
        false,
        "ffmpeg",
        ffmpeg,
        "install ffmpeg, or set --ffmpeg or FFMPEG; without it, video stays as raw 1-minute parts",
    );

    let probe = config.reports.join(".phantomqa-write-test");
    let writable = fs::create_dir_all(&config.reports)
        .and_then(|_| fs::write(&probe, b"ok"))
        .and_then(|_| fs::remove_file(&probe))
        .map(|_| format!("{} is writable", config.reports.display()))
        .map_err(|e| format!("{}: {e}", config.reports.display()));
    check(
        true,
        "reports",
        writable,
        "set --reports or PHANTOMQA_REPORTS to a folder you can write to",
    );

    let tokens = load_tokens(&config.tokens).and_then(|list| {
        let active = list.iter().filter(|t| t.expires.is_none_or(|e| now_secs() < e)).count();
        if active == 0 {
            Err("none yet".into())
        } else {
            Ok(format!("{active} active in {}", config.tokens.display()))
        }
    });
    check(
        false,
        "API tokens",
        tokens,
        "only `serve` needs them: phantomqa token add NAME",
    );

    let claude = first_line(&config.claude, "--version").map(|v| format!("{v} ({})", config.claude));
    check(
        false,
        "claude",
        claude,
        "only `run` needs it: install Claude Code, or set --claude or PHANTOMQA_CLAUDE",
    );

    let on_off = |on: bool| if on { "on" } else { "off" };
    say(&format!(
        "info  video: {}, screenshots: {}",
        on_off(config.video),
        on_off(config.screenshots)
    ));
    ok
}

// ---------------------------------------------------------------- CLI

const USAGE: &str = "usage: phantomqa [options] <command>

commands:
  mcp                 MCP server over stdio (for Claude Code and other agents)
  run FILE            test an app by itself: Claude Code follows the test file (exit 0 no bugs, 1 bugs, 2 failed)
  doctor              check adb, the device, ffmpeg and the reports folder
  screen              print the current screen
  tap N               tap element N
  type N TEXT         tap element N and type TEXT
  back                press back
  launch PACKAGE      start an app
  crashes             list crashes and ANRs in the device's crash log
  token ...           API tokens: add, rotate, remove, list, check (see `phantomqa token`)
  serve [--listen ADDR]  read-only HTTP API over the reports (default 127.0.0.1:7070, env PHANTOMQA_LISTEN),
                      and runs the tests in routines.json on schedule
  serve --no-api      only the routines: no API, no tokens needed
  routines            check routines.json and its test files, and show when each runs next

options (each also has an env var):
  --adb PATH          adb binary            ADB  (default: $ANDROID_HOME/platform-tools/adb, then PATH)
  --serial ID         device to use         ANDROID_SERIAL  (default: the only device connected)
  --reports DIR       reports folder        PHANTOMQA_REPORTS  (default: reports)
  --ffmpeg PATH       ffmpeg, optional      FFMPEG  (default: PATH)
  --no-video          do not record video   PHANTOMQA_VIDEO=0
  --no-screenshots    no screenshots        PHANTOMQA_SCREENSHOTS=0
  --tokens PATH       API tokens file       PHANTOMQA_TOKENS  (default: tokens.json)
  --claude PATH       Claude Code, for run  PHANTOMQA_CLAUDE  (default: claude on PATH)
  --routines PATH     scheduled tests       PHANTOMQA_ROUTINES  (default: routines.json)";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args
        .iter()
        .take_while(|a| a.starts_with('-'))
        .any(|a| a == "--help" || a == "-h")
    {
        say(USAGE);
        return;
    }
    let start = match load_config(&args, &env_var) {
        Ok((config, start)) => {
            let _ = CONFIG.set(config);
            start
        }
        Err(error) => {
            eprintln!("error: {error}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    let args: Vec<&str> = args[start..].iter().map(String::as_str).collect();
    let mut state = State::new(config().reports.clone());
    let id = |s: &str| s.parse::<i64>().map_err(|_| format!("not a number: {s}"));

    let result = match args[..] {
        ["mcp"] => {
            if std::env::var_os(WORKER_ENV).is_none() && relay_mcp() {
                return;
            }
            state.run_dir = env_var(RUN_DIR_ENV).map(PathBuf::from);
            serve_mcp(state);
            return;
        }
        ["run", file] => match run_test(Path::new(file), None) {
            Ok((text, code)) => {
                let _ = std::io::stdout().write_all(text.as_bytes());
                std::process::exit(code);
            }
            Err(error) => {
                eprintln!("error: {error}");
                std::process::exit(2);
            }
        },
        ["doctor"] => std::process::exit(if doctor() { 0 } else { 1 }),
        ["routines"] => std::process::exit(if routines_check() { 0 } else { 1 }),
        ["token", ref rest @ ..] => token_command(rest),
        ["serve", ref rest @ ..] => serve(rest),
        ["crashes"] => read_crash_log(None).map(|log| {
            let crashes = parse_crashes(&log);
            if crashes.is_empty() {
                "No crashes or ANRs in the crash log.\n".to_string()
            } else {
                crashes.iter().map(render_crash).collect::<Vec<_>>().join("\n")
            }
        }),
        ["screen"] => read_screen(&mut state),
        ["tap", n] => id(n).and_then(|n| read_screen(&mut state).and_then(|_| tap(&mut state, n))),
        ["type", n, ref text @ ..] if !text.is_empty() => {
            let text = text.join(" ");
            id(n).and_then(|n| read_screen(&mut state).and_then(|_| type_text(&mut state, n, &text)))
        }
        ["back"] => back(&mut state),
        ["launch", package] => launch_app(&mut state, package),
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    match result {
        Ok(text) => {
            let _ = std::io::stdout().write_all(text.as_bytes());
        }
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version='1.0' encoding='UTF-8' standalone='yes' ?><hierarchy rotation="0">
<node index="0" text="" resource-id="" class="android.widget.FrameLayout" package="com.example.app" content-desc="" checkable="false" checked="false" clickable="false" enabled="true" focusable="false" focused="false" scrollable="false" long-clickable="false" password="false" selected="false" bounds="[0,0][1080,2400]">
  <node index="0" text="Sign in &amp; sync" resource-id="com.example.app:id/title" class="android.widget.TextView" package="com.example.app" content-desc="" checkable="false" checked="false" clickable="false" enabled="true" focusable="false" focused="false" scrollable="false" long-clickable="false" password="false" selected="false" bounds="[40,100][1040,200]" />
  <node index="1" text="" resource-id="com.example.app:id/email" class="android.widget.EditText" package="com.example.app" content-desc="" hint="Email" checkable="false" checked="false" clickable="true" enabled="true" focusable="true" focused="true" scrollable="false" long-clickable="true" password="false" selected="false" bounds="[40,300][1040,420]" />
  <node index="2" text="" resource-id="com.example.app:id/row" class="android.widget.LinearLayout" package="com.example.app" content-desc="" checkable="false" checked="false" clickable="true" enabled="true" focusable="true" focused="false" scrollable="false" long-clickable="false" password="false" selected="false" bounds="[0,500][1080,640]">
    <node index="0" text="Wi‑Fi" resource-id="android:id/title" class="android.widget.TextView" package="com.example.app" content-desc="" checkable="false" checked="false" clickable="false" enabled="true" focusable="false" focused="false" scrollable="false" long-clickable="false" password="false" selected="false" bounds="[40,510][800,570]" />
    <node index="1" text="Off &gt; on" resource-id="android:id/summary" class="android.widget.TextView" package="com.example.app" content-desc="" checkable="false" checked="false" clickable="false" enabled="true" focusable="false" focused="false" scrollable="false" long-clickable="false" password="false" selected="false" bounds="[40,570][800,630]" />
  </node>
  <node index="3" text="" resource-id="" class="android.widget.Switch" package="com.example.app" content-desc="Dark mode" checkable="true" checked="true" clickable="true" enabled="false" focusable="true" focused="false" scrollable="false" long-clickable="false" password="false" selected="false" bounds="[900,700][1040,780]" />
  <node index="4" text="" resource-id="" class="android.view.View" package="com.example.app" content-desc="" checkable="false" checked="false" clickable="false" enabled="true" focusable="false" focused="false" scrollable="false" long-clickable="false" password="false" selected="false" bounds="[0,0][0,0]" />
  <node index="5" text="Footer" resource-id="" class="android.widget.TextView" package="com.example.app" content-desc="" checkable="false" checked="false" clickable="false" enabled="true" focusable="false" focused="false" scrollable="false" long-clickable="false" password="false" selected="false" bounds="[0,2300][1080,2400]" />
</node>
</hierarchy>"#;

    fn test_state() -> State {
        let mut state = State::new(std::env::temp_dir().join("phantomqa-test-unused"));
        state.video_checked = true; // never record from unit tests
        state
    }

    #[test]
    fn parses_sample_screen() {
        let (package, elements) = parse_screen(SAMPLE);
        assert_eq!(package, "com.example.app");
        let text = render_screen(&package, &elements);
        assert_eq!(
            text,
            "app: com.example.app\n\
             [1] TextView \"Sign in & sync\" #title\n\
             [2] EditText \"Email\" #email edit focused\n\
             [3] LinearLayout \"Wi‑Fi / Off > on\" #row click\n\
             [4] Switch \"Dark mode\" click on disabled\n\
             [5] TextView \"Footer\"\n"
        );
        assert_eq!(elements[2].center, (540, 570));
    }

    #[test]
    fn handles_dump_noise_around_xml() {
        let out = format!("{SAMPLE}UI hierchary dumped to: /dev/tty\n");
        let xml = extract_hierarchy(&out).unwrap();
        assert!(xml.starts_with("<hierarchy") && xml.ends_with("</hierarchy>"));
        assert_eq!(extract_hierarchy("ERROR: could not get idle state."), None);
        let empty = extract_hierarchy("<?xml version='1.0' ?><hierarchy rotation=\"0\" />").unwrap();
        assert_eq!(parse_screen(&empty).1.len(), 0);
    }

    #[test]
    fn unescapes_entities() {
        assert_eq!(
            unescape("a &amp; b &lt;c&gt; &quot;d&quot; &#39;e&#x27;"),
            "a & b <c> \"d\" 'e'"
        );
        assert_eq!(unescape("50% & more"), "50% & more");
        assert_eq!(unescape("&bogus; &"), "&bogus; &");
    }

    #[test]
    fn parses_bounds() {
        assert_eq!(parse_bounds("[0,63][1080,210]"), Some((0, 63, 1080, 210)));
        assert_eq!(parse_bounds("[0,63]"), None);
        assert_eq!(parse_bounds(""), None);
    }

    #[test]
    fn quotes_for_device_shell() {
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(input_text_args("hi there; rm -rf").unwrap(), ["'hi%sthere;%srm%s-rf'"]);
        // A literal "%s" would become a space, so it is typed in two calls.
        assert_eq!(input_text_args("50%sale").unwrap(), ["'50%'", "'sale'"]);
        assert_eq!(input_text_args("% s").unwrap(), ["'%%ss'"]);
        assert_eq!(input_text_args("-5").unwrap(), ["'-5'"]);
        assert!(input_text_args("").is_err());
        assert!(input_text_args("héllo").is_err());
        assert!(input_text_args("a\nb").is_err());
    }

    #[test]
    fn shortens_long_labels() {
        let long = "word ".repeat(40);
        let short = shorten(&long);
        assert_eq!(short.chars().count(), MAX_LABEL);
        assert!(short.ends_with('…'));
        assert_eq!(shorten("  two\n lines "), "two lines");
    }

    #[test]
    fn rejects_unknown_element_numbers() {
        let mut state = test_state();
        state.elements = parse_screen(SAMPLE).1;
        assert!(element(&state, 1).is_ok());
        assert!(element(&state, 0).is_err());
        assert!(element(&state, 6).is_err());
        assert!(element(&state, -1).is_err());
    }

    #[test]
    fn mcp_handshake_and_errors() {
        let mut state = test_state();
        let init =
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}});
        let reply = handle_message(&mut state, &init).unwrap();
        assert_eq!(reply["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(reply["result"]["serverInfo"]["name"], "phantomqa");

        let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert!(handle_message(&mut state, &note).is_none());

        let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        let tools = handle_message(&mut state, &list).unwrap();
        assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 8);

        let bad_tap = json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "tap", "arguments": {"id": "x"}}});
        let reply = handle_message(&mut state, &bad_tap).unwrap();
        assert_eq!(reply["result"]["isError"], true);

        let unknown = json!({"jsonrpc": "2.0", "id": 4, "method": "nope"});
        assert_eq!(handle_message(&mut state, &unknown).unwrap()["error"]["code"], -32601);
    }

    // Real lines from `am crash com.android.settings` on an API 34 emulator, plus noise.
    const CRASH_LOG: &str = "--------- beginning of crash
         1794377174.406  6199  6199 E AndroidRuntime: FATAL EXCEPTION: main
         1794377174.406  6199  6199 E AndroidRuntime: Process: com.android.settings, PID: 6199
         1794377174.406  6199  6199 E AndroidRuntime: android.app.RemoteServiceException$CrashedByAdbException: shell-induced crash
         1794377174.406  6199  6199 E AndroidRuntime: \tat android.app.ActivityThread.throwRemoteServiceException(ActivityThread.java:2090)
         1794377174.410  7000  7000 E AndroidRuntime: some other process
--------- beginning of system
         1794377180.100   500   600 E ActivityManager: ANR in com.example.app (com.example.app/.MainActivity)
         1794377180.100   500   600 E ActivityManager: PID: 4321
         1794377180.100   500   600 E ActivityManager: Reason: Input dispatching timed out
         1794377190.000  4400  4400 F libc    : Fatal signal 11 (SIGSEGV), code 1 (SEGV_MAPERR), fault addr 0x0 in tid 4400 (RenderThread), pid 4400 (com.example.game)
         1794377190.050  4500  4500 F DEBUG   : *** *** *** *** *** *** *** *** *** *** *** *** *** *** *** ***
         1794377190.050  4500  4500 F DEBUG   : pid: 4400, tid: 4400, name: RenderThread  >>> com.example.game <<<
";

    #[test]
    fn parses_crashes_anrs_and_native_crashes() {
        let crashes = parse_crashes(CRASH_LOG);
        assert_eq!(crashes.len(), 3);

        assert_eq!(crashes[0].kind, "crash");
        assert_eq!(crashes[0].process, "com.android.settings");
        assert_eq!(
            crashes[0].summary,
            "android.app.RemoteServiceException$CrashedByAdbException: shell-induced crash"
        );
        assert_eq!(
            crashes[0].lines.len(),
            4,
            "the other process's line is not part of this crash"
        );
        assert!(crashes[0].lines[3].starts_with("\tat android.app"));

        assert_eq!(crashes[1].kind, "ANR");
        assert_eq!(crashes[1].process, "com.example.app");
        assert_eq!(crashes[1].summary, "Input dispatching timed out");

        assert_eq!(crashes[2].kind, "native crash");
        assert_eq!(crashes[2].process, "com.example.game");
        assert_eq!(crashes[2].lines.len(), 3);
        assert_eq!(
            crashes[2].title(),
            format!("Native crash in com.example.game: {}", crashes[2].lines[0])
        );

        assert!(parse_crashes("--------- beginning of crash\n").is_empty());

        let notice = render_crash(&crashes[0]);
        assert_eq!(
            notice,
            "!! Crash in com.android.settings: android.app.RemoteServiceException$CrashedByAdbException: shell-induced crash\n   \
             at android.app.ActivityThread.throwRemoteServiceException(ActivityThread.java:2090)\n"
        );
    }

    #[test]
    fn moves_crash_clock_past_last_line() {
        assert_eq!(after("1794377174.406").as_deref(), Some("1794377174.407"));
        assert_eq!(after("1794377174.999").as_deref(), Some("1794377175.000"));
        assert_eq!(after("nope"), None);
        let last = CRASH_LOG.lines().rev().find_map(parse_log_line).unwrap();
        assert_eq!((last.time, last.pid, last.tag), ("1794377190.050", "4500", "DEBUG"));
    }

    #[test]
    fn checks_bug_arguments() {
        let args = json!({
            "title": "Login does nothing",
            "steps": ["1. Open the app", "Tap \"Log in\"", "  "],
            "expected": "An error message",
            "actual": "Nothing happens",
        });
        let bug = bug_from_args(&args, "com.example.app").unwrap();
        assert_eq!(bug.severity, "medium");
        assert_eq!(
            bug.body,
            "**Steps**\n\n1. Open the app\n2. Tap \"Log in\"\n\n**Expected:** An error message\n\n**Actual:** Nothing happens\n"
        );

        let as_text =
            json!({"title": "t", "steps": "1) First\n2) Second", "expected": "e", "actual": "a", "severity": "HIGH"});
        let bug = bug_from_args(&as_text, "").unwrap();
        assert_eq!(bug.severity, "high");
        assert!(bug.body.contains("1. First\n2. Second\n"));

        let missing_steps = json!({"title": "t", "steps": [], "expected": "e", "actual": "a"});
        assert!(bug_from_args(&missing_steps, "").is_err());
        let bad_severity = json!({"title": "t", "steps": ["s"], "expected": "e", "actual": "a", "severity": "huge"});
        assert!(bug_from_args(&bad_severity, "").is_err());
        let no_title = json!({"steps": ["s"], "expected": "e", "actual": "a"});
        assert!(bug_from_args(&no_title, "").is_err());

        assert_eq!(strip_step_number("3 dots menu opens"), "3 dots menu opens");
        assert_eq!(strip_step_number("12. Done"), "Done");
    }

    #[test]
    fn renders_bug_section() {
        let bug = Bug {
            title: "Login does nothing".into(),
            severity: "high".into(),
            package: "com.example.app".into(),
            body: "**Steps**\n\n1. Open\n".into(),
        };
        let history = vec![
            "launch_app com.example.app".to_string(),
            "tap [3] Button \"Log in\"".to_string(),
        ];
        let text = render_bug(2, &bug, &history, "![screenshot](bug-02.png)");
        assert_eq!(
            text,
            "\n## Bug 2: Login does nothing\n\n- Severity: high\n- App: com.example.app\n\n\
             **Steps**\n\n1. Open\n\n**Recent actions** (newest last)\n\n- launch_app com.example.app\n\
             - tap [3] Button \"Log in\"\n\n![screenshot](bug-02.png)\n"
        );
    }

    #[test]
    fn keeps_recent_history_only() {
        let mut state = test_state();
        for i in 0..20 {
            state.remember(format!("action {i}"));
        }
        assert_eq!(state.history.len(), MAX_HISTORY);
        assert_eq!(state.history.first().unwrap(), "action 5");
    }

    #[test]
    fn numbers_report_folders() {
        let root = std::env::temp_dir().join(format!("phantomqa-test-{}", std::process::id()));
        let mut a = State::new(root.clone());
        let mut b = State::new(root.clone());
        let dir_a = report_dir(&mut a).unwrap();
        let dir_b = report_dir(&mut b).unwrap();
        assert_eq!(dir_a, root.join("run-001"));
        assert_eq!(dir_b, root.join("run-002"));
        assert_eq!(report_dir(&mut a).unwrap(), dir_a, "same run reuses its folder");
        let header = fs::read_to_string(dir_a.join("report.md")).unwrap();
        assert!(header.starts_with("# PhantomQA test report: run 1\n"));

        fs::create_dir(root.join("run-041")).unwrap();
        fs::create_dir(root.join("notes")).unwrap();
        assert_eq!(next_run_number(&root), 42);
        assert_eq!(next_run_number(&root.join("missing")), 1);
        fs::remove_dir_all(root).unwrap();
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn reads_options_before_the_command() {
        let no_env = |_: &str| None;
        let (config, start) = load_config(
            &args(&[
                "--adb",
                "/x/adb",
                "--reports=out",
                "--serial",
                "emu-1",
                "--no-video",
                "tap",
                "3",
            ]),
            &no_env,
        )
        .unwrap();
        assert_eq!(start, 6);
        assert_eq!(config.adb, "/x/adb");
        assert!(config.reports.is_absolute() && config.reports.ends_with("out"));
        assert_eq!(config.serial.as_deref(), Some("emu-1"));
        assert!(!config.video);

        // Text after the command is never read as an option.
        let (_, start) = load_config(&args(&["type", "2", "--adb"]), &no_env).unwrap();
        assert_eq!(start, 0);

        let (config, _) = load_config(&args(&["--no-screenshots", "mcp"]), &no_env).unwrap();
        assert!(!config.screenshots && config.video);
        assert!(load_config(&args(&["--no-video=1"]), &no_env).is_err());

        assert!(load_config(&args(&["--adb"]), &no_env).is_err());
        assert!(load_config(&args(&["--adb="]), &no_env).is_err());
        assert!(load_config(&args(&["--reports", "--no-video"]), &no_env).is_err());
        let (_, start) = load_config(&args(&["--no-video", "--", "screen"]), &no_env).unwrap();
        assert_eq!(start, 2);
        assert!(load_config(&args(&["--nope", "screen"]), &no_env).is_err());
    }

    #[test]
    fn options_win_over_env_vars() {
        let env = |name: &str| match name {
            "ADB" => Some("/env/adb".to_string()),
            "FFMPEG" => Some("/env/ffmpeg".to_string()),
            "PHANTOMQA_REPORTS" => Some("env-reports".to_string()),
            "PHANTOMQA_VIDEO" => Some("0".to_string()),
            "PHANTOMQA_SCREENSHOTS" => Some("0".to_string()),
            "PHANTOMQA_ROUTINES" => Some("env-routines.json".to_string()),
            _ => None,
        };
        let (config, _) = load_config(&[], &env).unwrap();
        assert_eq!(config.adb, "/env/adb");
        assert!(config.routines.is_absolute() && config.routines.ends_with("env-routines.json"));
        let (flagged, _) = load_config(&args(&["--routines", "/qa/r.json"]), &env).unwrap();
        assert_eq!(flagged.routines, Path::new("/qa/r.json"));
        let (plain, _) = load_config(&[], &|_: &str| None).unwrap();
        assert!(plain.routines.ends_with("routines.json"));
        assert_eq!(config.ffmpeg.as_deref(), Some("/env/ffmpeg"));
        assert!(config.reports.ends_with("env-reports"));
        assert!(!config.video && !config.screenshots);

        let (config, _) = load_config(&args(&["--adb", "/flag/adb", "--ffmpeg", "/flag/ffmpeg"]), &env).unwrap();
        assert_eq!(config.adb, "/flag/adb");
        assert_eq!(config.ffmpeg.as_deref(), Some("/flag/ffmpeg"));

        let sdk = std::env::temp_dir().join(format!("phantomqa-sdk-{}", std::process::id()));
        let exe = if cfg!(windows) { "adb.exe" } else { "adb" };
        fs::create_dir_all(sdk.join("platform-tools")).unwrap();
        fs::write(sdk.join("platform-tools").join(exe), b"").unwrap();
        let sdk_env = |name: &str| (name == "ANDROID_HOME").then(|| sdk.to_string_lossy().into_owned());
        assert_eq!(
            find_adb(&sdk_env),
            sdk.join("platform-tools").join(exe).to_string_lossy()
        );
        assert_eq!(find_adb(&|_: &str| None), "adb");
        fs::remove_dir_all(sdk).unwrap();
    }

    #[test]
    fn checks_mp4_files() {
        let mut good = vec![0, 0, 0, 24];
        good.extend_from_slice(b"ftypmp42");
        good.resize(2000, 0);
        good.extend_from_slice(b"moov");
        assert!(looks_like_mp4(&good));

        let mut killed = good.clone();
        killed.truncate(2000); // no "moov": the recording was cut off
        assert!(!looks_like_mp4(&killed));
        assert!(!looks_like_mp4(b"short"));
        assert!(!looks_like_mp4(&[0u8; 4096]));
    }

    #[test]
    fn picks_parts_for_a_bug_clip() {
        let part = |name: &str, seconds| Part {
            file: PathBuf::from(name),
            seconds,
        };
        assert!(clip_parts(&[]).is_empty());
        assert_eq!(clip_parts(&[part("a", 3.0)]), vec![PathBuf::from("a")]);
        assert_eq!(
            clip_parts(&[part("a", 30.0), part("b", 3.0)]),
            vec![PathBuf::from("a"), PathBuf::from("b")],
            "a short last part gets the one before it"
        );
        assert_eq!(
            clip_parts(&[part("a", 30.0), part("b", 25.0)]),
            vec![PathBuf::from("b")]
        );
        assert_eq!(
            part_links(&[PathBuf::from("/x/video/part-001.mp4")]),
            "[part-001.mp4](video/part-001.mp4)"
        );
        assert!(is_pid("4321") && !is_pid("") && !is_pid("12; rm"));
    }

    #[test]
    fn makes_videos_without_ffmpeg_when_possible() {
        let dir = std::env::temp_dir().join(format!("phantomqa-video-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let one = dir.join("part-001.mp4");
        fs::write(&one, b"video").unwrap();
        let out = dir.join("bug-01.mp4");
        make_video(std::slice::from_ref(&one), &out).unwrap();
        assert_eq!(fs::read(&out).unwrap(), b"video", "one part is copied as is");
        assert!(make_video(&[], &out).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn video_off_note_needs_a_report() {
        let root = std::env::temp_dir().join(format!("phantomqa-note-{}", std::process::id()));
        let mut state = State::new(root.clone());
        state.video_off = Some("turned off".into());
        finish_session(&mut state);
        assert!(!root.exists(), "no bugs and no video: no folder");

        let dir = report_dir(&mut state).unwrap();
        finish_session(&mut state);
        let report = fs::read_to_string(dir.join("report.md")).unwrap();
        assert!(report.ends_with("\nNo bugs found.\n\n## Video\n\nVideo was off: turned off.\n"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reads_element_numbers_leniently() {
        assert_eq!(int_arg(&json!(3)), Some(3));
        assert_eq!(int_arg(&json!(3.0)), Some(3));
        assert_eq!(int_arg(&json!("3")), Some(3));
        assert_eq!(int_arg(&json!("[3]")), Some(3));
        assert_eq!(int_arg(&json!(3.5)), None);
        assert_eq!(int_arg(&json!("three")), None);
        assert_eq!(int_arg(&Value::Null), None);
    }

    #[test]
    fn crash_titles_and_code_blocks_are_safe() {
        let crash = Crash {
            kind: "crash",
            process: String::new(),
            summary: String::new(),
            lines: vec![],
        };
        assert_eq!(crash.title(), "Crash in unknown process: app stopped");
        assert_eq!(code_block("a"), "```\na\n```\n");
        assert_eq!(code_block("x ``` y"), "````\nx ``` y\n````\n");
        let args = json!({"title": "Two\nlines", "steps": ["s"], "expected": "e", "actual": "a"});
        assert_eq!(bug_from_args(&args, "").unwrap().title, "Two lines");
    }

    #[test]
    fn saves_each_crash_of_the_tested_app_once() {
        let root = std::env::temp_dir().join(format!("phantomqa-crashes-{}", std::process::id()));
        let mut state = State::new(root.clone());
        state.video_checked = true;
        state.apps = vec!["com.android.settings".into()];
        let crashes = parse_crashes(CRASH_LOG);

        let first = handle_crashes(&mut state, &crashes);
        assert!(first.contains("Saved to the report as bug 1"));
        assert!(first.contains("(another app crashed, not saved: ANR in com.example.app"));
        assert!(first.contains("(another app crashed, not saved: Native crash in com.example.game"));

        let again = handle_crashes(&mut state, &crashes[..1]);
        assert!(again.contains("The same crash happened again (bug 1)"));
        assert_eq!(state.report.as_ref().unwrap().bugs, 1);

        // A deleted report folder is created again.
        fs::remove_dir_all(&root).unwrap();
        let bug = Bug {
            title: "t".into(),
            severity: "low".into(),
            package: String::new(),
            body: "b".into(),
        };
        assert_eq!(save_bug(&mut state, &bug, "").unwrap(), 2);
        let report = fs::read_to_string(root.join("run-001").join("report.md")).unwrap();
        assert!(report.contains("## Bug 2: t"));
        fs::remove_dir_all(root).unwrap();
    }

    // ------------------------------------------------------------ tokens

    fn token_dir(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("phantomqa-tokens-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    const NOW: u64 = 1_794_377_174;
    const HOUR: u64 = 3600;

    #[test]
    fn token_format_is_strict() {
        let entries = Vec::new();
        let (entry, token) = new_token("ci", None, NOW, &entries).unwrap();
        assert!(token.starts_with("pqa_"));
        assert_eq!(token.len(), 4 + 16 + 1 + 64);
        let (id, secret) = parse_token(&token).unwrap();
        assert_eq!(id, entry.id);
        assert_eq!(secret.len(), 64);
        assert_eq!(entry.hash, hash_token(&token));
        assert!(entry.hash.starts_with("sha256:") && !entry.hash.contains(secret));

        let good = format!("pqa_{}_{}", "a".repeat(16), "b".repeat(64));
        assert!(parse_token(&good).is_some());
        for bad in [
            String::new(),
            "pqa_".into(),
            format!("pqx_{}_{}", "a".repeat(16), "b".repeat(64)),
            format!("pqa_{}_{}", "A".repeat(16), "b".repeat(64)),
            format!("pqa_{}_{}", "a".repeat(15), "b".repeat(64)),
            format!("pqa_{}_{}", "a".repeat(16), "b".repeat(63)),
            format!("pqa_{}_{}_", "a".repeat(16), "b".repeat(64)),
            format!("pqa_{}{}", "a".repeat(16), "b".repeat(64)),
            format!("pqa_{}_{}", "g".repeat(16), "b".repeat(64)),
            format!("pqa_{}_{} ", "a".repeat(16), "b".repeat(64)),
            format!("pqa_{}_{}é", "a".repeat(16), "b".repeat(63)),
        ] {
            assert!(parse_token(&bad).is_none(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn reads_bearer_headers() {
        assert_eq!(bearer_token("Bearer abc"), Some("abc"));
        assert_eq!(bearer_token("  bearer   abc  "), Some("abc"));
        assert_eq!(bearer_token("BEARER\tabc"), Some("abc"));
        assert_eq!(bearer_token("Basic abc"), None);
        assert_eq!(bearer_token("Bearer"), None);
        assert_eq!(bearer_token("Bearer "), None);
        assert_eq!(bearer_token("Bearer a b"), None);
        assert_eq!(bearer_token("abc"), None);
        assert_eq!(bearer_token(&format!("Bearer {}", "a".repeat(600))), None);
    }

    #[test]
    fn add_check_and_revoke() {
        let dir = token_dir("revoke");
        let path = dir.join("tokens.json");
        let token = token_add(&path, "dashboard", None, NOW).unwrap();
        let header = format!("Bearer {token}");
        assert_eq!(authorize(&path, Some(&header)), Ok("dashboard".into()));

        // Same id, wrong secret.
        let (id, _) = parse_token(&token).unwrap();
        let forged = format!("pqa_{id}_{}", "0".repeat(64));
        let entries = load_tokens(&path).unwrap();
        assert_eq!(check_token(&entries, &forged, NOW), Err(TokenDenied::Unknown));
        assert_eq!(authorize(&path, None), Err(TokenDenied::Missing));
        assert_eq!(authorize(&path, Some("Bearer nope")), Err(TokenDenied::Malformed));

        // Never stored in clear text.
        let file = fs::read_to_string(&path).unwrap();
        assert!(!file.contains(&token[TOKEN_PREFIX.len() + 17..]));

        assert_eq!(token_remove(&path, "dashboard", false, NOW), Ok(1));
        assert!(matches!(authorize(&path, Some(&header)), Err(TokenDenied::NoTokens(_))));
        assert!(token_remove(&path, "dashboard", false, NOW).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rotates_with_and_without_grace() {
        let dir = token_dir("rotate");
        let path = dir.join("tokens.json");
        let first = token_add(&path, "ci", Some(30 * 24 * HOUR), NOW).unwrap();

        // No grace: the old token stops at once; the lifetime carries over.
        let second = token_rotate(&path, "ci", 0, None, NOW).unwrap();
        let entries = load_tokens(&path).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].expires, Some(NOW + 30 * 24 * HOUR));
        assert_eq!(check_token(&entries, &first, NOW), Err(TokenDenied::Unknown));
        assert_eq!(check_token(&entries, &second, NOW), Ok("ci".into()));

        // One hour of grace: both work, then only the new one.
        let third = token_rotate(&path, "ci", HOUR, Some(None), NOW).unwrap();
        let entries = load_tokens(&path).unwrap();
        assert_eq!(check_token(&entries, &second, NOW + HOUR - 1), Ok("ci".into()));
        assert_eq!(check_token(&entries, &third, NOW + HOUR - 1), Ok("ci".into()));
        assert_eq!(check_token(&entries, &second, NOW + HOUR), Err(TokenDenied::Expired));
        assert_eq!(
            check_token(&entries, &third, NOW + 365 * 24 * HOUR),
            Ok("ci".into()),
            "--expires never"
        );
        assert!(render_token_list(&entries, NOW).contains("old key, works until"));

        // Rotating again during grace keeps a single new key.
        let fourth = token_rotate(&path, "ci", HOUR, None, NOW + 60).unwrap();
        let entries = load_tokens(&path).unwrap();
        assert_eq!(entries.iter().filter(|e| !e.rotated).count(), 1);
        assert_eq!(check_token(&entries, &fourth, NOW + 60), Ok("ci".into()));
        // The first grace window is not extended by the second rotation.
        assert_eq!(check_token(&entries, &second, NOW + HOUR), Err(TokenDenied::Expired));

        // End the grace early.
        assert_eq!(token_remove(&path, "ci", true, NOW + 120), Ok(2));
        let entries = load_tokens(&path).unwrap();
        assert_eq!(check_token(&entries, &third, NOW + 120), Err(TokenDenied::Unknown));
        assert_eq!(check_token(&entries, &fourth, NOW + 120), Ok("ci".into()));
        assert!(token_remove(&path, "ci", true, NOW + 120).is_err());
        assert!(token_rotate(&path, "missing", 0, None, NOW).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn expired_tokens_are_refused_and_cleaned_up() {
        let dir = token_dir("expire");
        let path = dir.join("tokens.json");
        let token = token_add(&path, "short", Some(HOUR), NOW).unwrap();
        let entries = load_tokens(&path).unwrap();
        assert_eq!(check_token(&entries, &token, NOW + HOUR - 1), Ok("short".into()));
        assert_eq!(check_token(&entries, &token, NOW + HOUR), Err(TokenDenied::Expired));
        assert!(render_token_list(&entries, NOW + HOUR).contains("expired"));
        // Any later change drops it, and its name can be used again.
        token_add(&path, "short", None, NOW + HOUR).unwrap();
        assert_eq!(load_tokens(&path).unwrap().len(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn refuses_bad_names_durations_and_duplicates() {
        let dir = token_dir("names");
        let path = dir.join("tokens.json");
        for bad in ["", "has space", "slash/name", "ünï", &"x".repeat(65)] {
            assert!(token_add(&path, bad, None, NOW).is_err(), "{bad:?}");
        }
        token_add(&path, "ci.main-2_b", None, NOW).unwrap();
        assert!(
            token_add(&path, "ci.main-2_b", None, NOW)
                .unwrap_err()
                .contains("rotate")
        );

        assert_eq!(parse_duration("90m"), Ok(Some(90 * 60)));
        assert_eq!(parse_duration("12h"), Ok(Some(12 * HOUR)));
        assert_eq!(parse_duration("30d"), Ok(Some(30 * 24 * HOUR)));
        assert_eq!(parse_duration("never"), Ok(None));
        for bad in ["", "5", "0d", "-1d", "1y", "d", "1.5h", "99999999999999999999d"] {
            assert!(parse_duration(bad).is_err(), "{bad:?}");
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn broken_or_missing_files_refuse_everything() {
        let dir = token_dir("broken");
        let path = dir.join("tokens.json");
        let token = format!("pqa_{}_{}", "a".repeat(16), "b".repeat(64));
        let header = format!("Bearer {token}");
        assert!(
            matches!(authorize(&path, Some(&header)), Err(TokenDenied::NoTokens(_))),
            "missing file"
        );

        for broken in [
            "not json",
            "{}",
            r#"{"version": 2, "tokens": []}"#,
            r#"{"version": 1}"#,
            r#"{"version": 1, "tokens": [{"name": "x"}]}"#,
            r#"{"version": 1, "tokens": [{"name": "bad name", "id": "a", "hash": "b"}]}"#,
        ] {
            fs::write(&path, broken).unwrap();
            assert!(load_tokens(&path).is_err(), "{broken}");
            assert!(matches!(authorize(&path, Some(&header)), Err(TokenDenied::NoTokens(_))));
            // Changes refuse to overwrite a broken file.
            assert!(token_add(&path, "new", None, NOW).is_err());
        }

        // A hash made another way never matches.
        let entry = json!({"name": "x", "id": "a".repeat(16), "hash": format!("md5:{}", "0".repeat(32))});
        fs::write(&path, json!({"version": 1, "tokens": [entry]}).to_string()).unwrap();
        assert_eq!(authorize(&path, Some(&header)), Err(TokenDenied::Unknown));
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn tokens_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = token_dir("private");
        let path = dir.join("tokens.json");
        token_add(&path, "ci", None, NOW).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(!dir.join("tokens.json.lock").exists(), "lock released");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn parallel_changes_are_not_lost() {
        let dir = token_dir("parallel");
        let path = dir.join("tokens.json");
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || token_add(&path, &format!("t{i}"), None, NOW).unwrap())
            })
            .collect();
        let tokens: Vec<String> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        let entries = load_tokens(&path).unwrap();
        assert_eq!(entries.len(), 8);
        for (i, token) in tokens.iter().enumerate() {
            assert_eq!(check_token(&entries, token, NOW), Ok(format!("t{i}")));
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_lock_from_a_killed_command_is_removed() {
        let dir = token_dir("stale");
        let path = dir.join("tokens.json");
        let lock = dir.join("tokens.json.lock");
        fs::write(&lock, b"").unwrap();
        let old = std::time::SystemTime::now() - Duration::from_secs(120);
        fs::File::options()
            .write(true)
            .open(&lock)
            .unwrap()
            .set_modified(old)
            .unwrap();
        token_add(&path, "ci", None, NOW).unwrap();
        assert!(!lock.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn formats_utc_dates() {
        assert_eq!(utc_date(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc_date(NOW), "2026-11-11 06:06 UTC");
        assert_eq!(utc_date(951_782_400), "2000-02-29 00:00 UTC");
        assert_eq!(utc_date(4_102_444_800), "2100-01-01 00:00 UTC");
    }

    // ------------------------------------------------------------ HTTP API

    struct Api {
        dir: PathBuf,
        reports: PathBuf,
        tokens: PathBuf,
        token: String,
    }

    impl Api {
        fn new(test: &str) -> Api {
            let dir = token_dir(&format!("api-{test}"));
            let tokens = dir.join("tokens.json");
            let token = token_add(&tokens, "dashboard", None, now_secs()).unwrap();
            let reports = dir.join("reports");
            fs::create_dir_all(&reports).unwrap();
            Api {
                dir,
                reports,
                tokens,
                token,
            }
        }

        fn request(&self, method: &str, url: &str, auth: Option<&str>, range: Option<&str>) -> Reply {
            let req = ApiRequest {
                method,
                url,
                authorization: auth,
                range,
            };
            route(&req, &self.reports, &self.tokens).0
        }

        fn get(&self, url: &str) -> Reply {
            let auth = format!("Bearer {}", self.token);
            self.request("GET", url, Some(&auth), None)
        }

        fn run(&self, n: usize, run_json: Option<&str>) -> PathBuf {
            let dir = self.reports.join(format!("run-{n:03}"));
            fs::create_dir_all(&dir).unwrap();
            if let Some(text) = run_json {
                fs::write(dir.join("run.json"), text).unwrap();
            }
            dir
        }
    }

    impl Drop for Api {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn body(reply: Reply) -> Vec<u8> {
        match reply.body {
            Body::Bytes(bytes) => bytes,
            Body::File { mut file, start, len } => {
                use std::io::Seek;
                file.seek(std::io::SeekFrom::Start(start)).unwrap();
                let mut out = Vec::new();
                file.take(len).read_to_end(&mut out).unwrap();
                out
            }
        }
    }

    /// Parallel tests start processes all the time, and a child holds a copy of every open file
    /// for a moment before exec, locks included. So a freed lock can look held for a moment.
    fn eventually(check: impl Fn() -> bool) -> bool {
        (0..100).any(|_| {
            check() || {
                sleep(Duration::from_millis(10));
                false
            }
        })
    }

    fn json_body(reply: Reply) -> Value {
        serde_json::from_slice(&body(reply)).unwrap()
    }

    fn header<'a>(reply: &'a Reply, name: &str) -> Option<&'a str> {
        reply.headers.iter().find(|(k, _)| *k == name).map(|(_, v)| v.as_str())
    }

    fn run_json(status: &str, bugs: Value) -> String {
        json!({"version": 1, "run": 0, "status": status, "device": "Pixel, Android 14 (API 34)",
               "bugs": bugs, "session_video": null})
        .to_string()
    }

    #[test]
    fn api_public_routes_and_request_checks() {
        let api = Api::new("public");
        let health = api.request("GET", "/health", None, None);
        assert_eq!(health.status, 200);
        assert_eq!(json_body(health)["ok"], true);
        assert_eq!(api.request("GET", "/health/", None, None).status, 200, "trailing slash");
        assert_eq!(api.request("HEAD", "/health", None, None).status, 200);
        let spec = json_body(api.request("GET", "/openapi.json", None, None));
        assert_eq!(spec["openapi"], "3.1.0");

        assert_eq!(api.request("GET", "/nope", None, None).status, 404);
        let post = api.request("POST", "/v1/runs", None, None);
        assert_eq!(post.status, 405);
        assert_eq!(header(&post, "Allow"), Some("GET, HEAD"));
        assert_eq!(api.request("DELETE", "/health", None, None).status, 405);
        let long = format!("/v1/runs?{}", "x".repeat(3000));
        assert_eq!(api.request("GET", &long, None, None).status, 414);
    }

    #[test]
    fn api_needs_a_valid_token_and_hides_routes_without_one() {
        let api = Api::new("auth");
        for auth in [
            None,
            Some("Bearer nope"),
            Some("Basic abc"),
            Some("Bearer pqa_0000000000000000_"),
        ] {
            let reply = api.request("GET", "/v1/runs", auth, None);
            assert_eq!(reply.status, 401, "{auth:?}");
            assert_eq!(header(&reply, "WWW-Authenticate"), Some("Bearer realm=\"phantomqa\""));
            assert_eq!(json_body(reply)["error"]["code"], "unauthorized");
        }
        assert_eq!(api.request("GET", "/v1/secret-route", None, None).status, 401);
        assert_eq!(api.get("/v1/runs").status, 200);
        assert_eq!(api.get("/v1/secret-route").status, 404);

        // Revoked: refused on the very next request.
        token_remove(&api.tokens, "dashboard", false, now_secs()).unwrap();
        assert_eq!(api.get("/v1/runs").status, 401);
    }

    #[test]
    fn api_lists_runs_newest_first() {
        let api = Api::new("list");
        api.run(1, None);
        api.run(2, Some(&run_json("finished", json!([{"bug": 1}, {"bug": 2}]))));
        api.run(10, Some(&run_json("finished", json!([]))));
        // Not runs: a non-canonical name, another folder, and a file.
        fs::create_dir_all(api.reports.join("run-03")).unwrap();
        fs::create_dir_all(api.reports.join("notes")).unwrap();
        fs::write(api.reports.join("run-004"), b"").unwrap();

        let runs = json_body(api.get("/v1/runs"))["runs"].clone();
        assert_eq!(
            runs,
            json!([
                {"run": 10, "status": "finished", "bugs": 0},
                {"run": 2, "status": "finished", "bugs": 2},
                {"run": 1, "status": "unknown", "bugs": 0},
            ])
        );
        let limited = json_body(api.get("/v1/runs?limit=2&other=x"));
        assert_eq!(limited["runs"].as_array().unwrap().len(), 2);
        for bad in ["0", "abc", "1001", "-1", ""] {
            let reply = api.get(&format!("/v1/runs?limit={bad}"));
            assert_eq!(reply.status, 400, "limit={bad}");
            assert_eq!(json_body(reply)["error"]["code"], "bad_request");
        }

        // No reports folder at all: an empty list.
        fs::remove_dir_all(&api.reports).unwrap();
        assert_eq!(json_body(api.get("/v1/runs"))["runs"], json!([]));
    }

    #[test]
    fn api_run_status_notices_killed_sessions() {
        let api = Api::new("status");
        let bug = json!([{"bug": 1, "title": "t", "severity": "high", "app": "a", "files": {"log": "bug-01.log"}}]);
        let finished = api.run(2, Some(&run_json("finished", bug.clone())));
        fs::write(finished.join("report.md"), "# report").unwrap();
        fs::write(finished.join(RUN_LOCK), b"").unwrap();
        fs::create_dir_all(finished.join("video")).unwrap();
        fs::write(finished.join("video").join("part-001.mp4"), b"v").unwrap();

        let detail = json_body(api.get("/v1/runs/2"));
        assert_eq!(detail["status"], "finished");
        assert_eq!(detail["bugs"], bug);
        assert_eq!(detail["device"], "Pixel, Android 14 (API 34)");
        assert_eq!(
            detail["files"],
            json!(["report.md", "run.json", "video/part-001.mp4"]),
            "no hidden files"
        );
        assert_eq!(json_body(api.get("/v1/runs/0002"))["run"], 2);

        let running = api.run(3, Some(&run_json("running", json!([]))));
        let held = fs::File::create(running.join(RUN_LOCK)).unwrap();
        held.lock_shared().unwrap();
        assert_eq!(json_body(api.get("/v1/runs/3"))["status"], "running");
        drop(held);
        assert!(
            eventually(|| json_body(api.get("/v1/runs/3"))["status"] == "interrupted"),
            "session gone, lock free"
        );
        api.run(4, Some(&run_json("running", json!([]))));
        assert_eq!(
            json_body(api.get("/v1/runs/4"))["status"],
            "interrupted",
            "no lock file at all"
        );

        api.run(5, Some("{not json"));
        let broken = json_body(api.get("/v1/runs/5"));
        assert_eq!(broken["status"], "broken");
        assert!(broken["error"].as_str().unwrap().contains("not valid JSON"));
        api.run(6, Some(r#"{"version": 9, "bugs": []}"#));
        assert_eq!(json_body(api.get("/v1/runs/6"))["status"], "broken");
        api.run(7, None);
        assert_eq!(json_body(api.get("/v1/runs/7"))["status"], "unknown");

        for missing in [
            "/v1/runs/9",
            "/v1/runs/0",
            "/v1/runs/abc",
            "/v1/runs/-1",
            "/v1/runs/9999999999",
            "/v1/runs/2/x",
        ] {
            assert_eq!(api.get(missing).status, 404, "{missing}");
        }
    }

    #[test]
    fn api_serves_only_real_visible_files() {
        let api = Api::new("files");
        let dir = api.run(1, Some(&run_json("finished", json!([]))));
        fs::write(dir.join("report.md"), "# report").unwrap();
        fs::write(dir.join(RUN_LOCK), b"").unwrap();
        fs::create_dir_all(dir.join("video")).unwrap();
        fs::write(dir.join("video").join("part-001.mp4"), b"0123456789").unwrap();

        let report = api.get("/v1/runs/1/files/report.md");
        assert_eq!(report.status, 200);
        assert_eq!(header(&report, "Content-Type"), Some("text/markdown; charset=utf-8"));
        assert_eq!(header(&report, "Accept-Ranges"), Some("bytes"));
        assert_eq!(body(report), b"# report");
        assert_eq!(api.get("/v1/runs/1/files/video/part-001.mp4").status, 200);
        assert_eq!(
            header(&api.get("/v1/runs/1/files/run.json"), "Content-Type"),
            Some("application/json; charset=utf-8")
        );

        for bad in [
            "/v1/runs/1/files/.lock",
            "/v1/runs/1/files/..",
            "/v1/runs/1/files/../run.json",
            "/v1/runs/1/files/%2e%2e",
            "/v1/runs/1/files/video",
            "/v1/runs/1/files/missing.png",
            "/v1/runs/1/files/video/../report.md",
            "/v1/runs/1/files/a%2Fb",
            "/v1/runs/2/files/report.md",
        ] {
            assert_eq!(api.get(bad).status, 404, "{bad}");
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/hostname", dir.join("outside.txt")).unwrap();
            assert_eq!(
                api.get("/v1/runs/1/files/outside.txt").status,
                404,
                "links are not followed"
            );
            std::os::unix::fs::symlink(&dir, api.reports.join("run-002")).unwrap();
            assert_eq!(api.get("/v1/runs/2").status, 404, "a linked run folder is not served");
        }
    }

    #[test]
    fn api_byte_ranges() {
        use ByteRange::*;
        let cases = [
            (None, 10, Whole),
            (Some("bytes=0-3"), 10, Part(0, 3)),
            (Some("bytes=5-"), 10, Part(5, 9)),
            (Some("bytes=-3"), 10, Part(7, 9)),
            (Some("bytes=-30"), 10, Part(0, 9)),
            (Some("bytes=8-100"), 10, Part(8, 9)),
            (Some("bytes=10-"), 10, Unsatisfiable),
            (Some("bytes=10-20"), 10, Unsatisfiable),
            (Some("bytes=-0"), 10, Unsatisfiable),
            (Some("bytes=0-"), 0, Unsatisfiable),
            (Some("bytes=5-3"), 10, Whole),
            (Some("bytes=0-1,4-5"), 10, Whole),
            (Some("items=0-1"), 10, Whole),
            (Some("bytes=a-b"), 10, Whole),
            (Some("bytes=-"), 10, Whole),
            (Some("bytes=+1-2"), 10, Whole),
        ];
        for (header, size, expected) in cases {
            assert_eq!(parse_range(header, size), expected, "{header:?} of {size}");
        }

        let api = Api::new("ranges");
        let dir = api.run(1, None);
        fs::write(dir.join("bug-01.mp4"), b"0123456789").unwrap();
        let auth = format!("Bearer {}", api.token);
        let url = "/v1/runs/1/files/bug-01.mp4";
        let part = api.request("GET", url, Some(&auth), Some("bytes=2-4"));
        assert_eq!(part.status, 206);
        assert_eq!(header(&part, "Content-Range"), Some("bytes 2-4/10"));
        assert_eq!(header(&part, "Content-Type"), Some("video/mp4"));
        assert_eq!(body(part), b"234");
        let tail = api.request("GET", url, Some(&auth), Some("bytes=-2"));
        assert_eq!(body(tail), b"89");
        let outside = api.request("GET", url, Some(&auth), Some("bytes=50-"));
        assert_eq!(outside.status, 416);
        assert_eq!(header(&outside, "Content-Range"), Some("bytes */10"));
    }

    #[test]
    fn openapi_lists_exactly_the_routes() {
        let spec: Value = serde_json::from_str(OPENAPI).unwrap();
        let mut documented: Vec<&str> = spec["paths"].as_object().unwrap().keys().map(String::as_str).collect();
        let mut routes = ROUTES.to_vec();
        documented.sort();
        routes.sort();
        assert_eq!(documented, routes);
        for (path, item) in spec["paths"].as_object().unwrap() {
            assert!(item["get"]["responses"]["200"].is_object(), "{path} documents its 200");
        }
    }

    #[test]
    fn mcp_session_writes_run_json_for_the_api() {
        let api = Api::new("session");
        let mut state = State::new(api.reports.clone());
        state.video_checked = true;
        report_dir(&mut state).unwrap();
        assert_eq!(
            json_body(api.get("/v1/runs/1"))["status"],
            "running",
            "the session holds the lock"
        );

        let bug = Bug {
            title: "Login fails".into(),
            severity: "high".into(),
            package: "com.x".into(),
            body: "b".into(),
        };
        save_bug(&mut state, &bug, "").unwrap();
        let detail = json_body(api.get("/v1/runs/1"));
        assert_eq!(detail["bugs"][0]["title"], "Login fails");
        assert_eq!(detail["bugs"][0]["severity"], "high");
        assert_eq!(detail["bugs"][0]["files"]["log"], "bug-01.log");

        finish_session(&mut state);
        let detail = json_body(api.get("/v1/runs/1"));
        assert_eq!(detail["status"], "finished");
        assert_eq!(detail["session_video"], Value::Null);
        assert_eq!(json_body(api.get("/v1/runs"))["runs"][0]["bugs"], 1);
    }

    #[test]
    fn reads_test_files() {
        let base = Path::new("/tests");
        let test = parse_test(r#"{"app": "com.example.shop", "goal": " Test the cart. "}"#, base).unwrap();
        assert_eq!(test.app, "com.example.shop");
        assert_eq!(test.goal, "Test the cart.");
        assert_eq!(
            (test.model.as_str(), test.max_minutes),
            (DEFAULT_MODEL, DEFAULT_MAX_MINUTES)
        );
        assert!(test.apk.is_none() && test.avoid.is_empty() && test.login.is_none());

        let full = r#"{"app": "com.x", "goal": "g", "apk": "build/app.apk", "avoid": ["Pay now"], "model": "haiku",
            "max_minutes": 5, "login": {"user": "qa@example.com", "password_env": "SHOP_QA_PASSWORD"}}"#;
        let test = parse_test(full, base).unwrap();
        assert_eq!(
            test.apk.as_deref(),
            Some(Path::new("/tests/build/app.apk")),
            "relative to the file"
        );
        assert_eq!(test.avoid, ["Pay now"]);
        assert_eq!(
            test.login,
            Some(Login {
                user: "qa@example.com".into(),
                password_env: "SHOP_QA_PASSWORD".into()
            })
        );
        let saved = test.to_json();
        assert_eq!(saved["apk"], "app.apk", "no local paths in run.json");
        assert_eq!(saved["login"]["password_env"], "SHOP_QA_PASSWORD");

        for (bad, why) in [
            ("[]", "must be a JSON object"),
            ("{nope", "not valid JSON"),
            (r#"{"goal": "g"}"#, "\"app\" is required"),
            (r#"{"app": "com.x"}"#, "\"goal\" is required"),
            (r#"{"app": "com.x", "goal": "  "}"#, "non-empty string"),
            (r#"{"app": "com x", "goal": "g"}"#, "not a package name"),
            (
                r#"{"app": "com.x", "goal": "g", "modle": "haiku"}"#,
                "unknown field \"modle\"",
            ),
            (r#"{"app": "com.x", "goal": "g", "model": "a b"}"#, "not a model name"),
            (r#"{"app": "com.x", "goal": "g", "max_minutes": 0}"#, "from 1 to"),
            (r#"{"app": "com.x", "goal": "g", "max_minutes": 2.5}"#, "whole number"),
            (r#"{"app": "com.x", "goal": "g", "max_minutes": "5"}"#, "whole number"),
            (
                r#"{"app": "com.x", "goal": "g", "avoid": "Pay"}"#,
                "list of button texts",
            ),
            (
                r#"{"app": "com.x", "goal": "g", "avoid": ["Pay", ""]}"#,
                "list of button texts",
            ),
            (r#"{"app": "com.x", "goal": "g", "login": "me"}"#, "must be an object"),
            (
                r#"{"app": "com.x", "goal": "g", "login": {"user": "u"}}"#,
                "needs \"password_env\"",
            ),
            (
                r#"{"app": "com.x", "goal": "g", "login": {"password_env": "P"}}"#,
                "needs \"user\"",
            ),
            (
                r#"{"app": "com.x", "goal": "g", "login": {"user": "u", "password_env": "P", "password": "x"}}"#,
                "unknown field \"password\"",
            ),
            (
                r#"{"app": "com.x", "goal": "g", "login": {"user": "u", "password_env": "hunter2!"}}"#,
                "env var name",
            ),
            (
                r#"{"app": "com.x", "goal": "g", "login": {"user": "ü", "password_env": "P"}}"#,
                "printable ASCII",
            ),
        ] {
            let error = parse_test(bad, base).unwrap_err();
            assert!(error.contains(why), "{bad}: {error}");
        }
    }

    #[test]
    fn example_test_file_is_valid() {
        let test = load_test(Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/settings.json"
        )))
        .unwrap();
        assert_eq!(
            (test.name.as_str(), test.app.as_str()),
            ("settings.json", "com.android.settings")
        );
    }

    #[test]
    fn test_prompt_has_the_rules_but_no_password() {
        let mut test = parse_test(
            r#"{"app": "com.x", "goal": "Check login.", "avoid": ["Pay now"], "login": {"user": "qa@x.com", "password_env": "PQA_PROMPT_PW"}}"#,
            Path::new("."),
        )
        .unwrap();
        let prompt = test.prompt();
        for part in [
            "com.x",
            "Check login.",
            "\"Pay now\"",
            "\"qa@x.com\"",
            "type_password",
            "30 minutes",
        ] {
            assert!(prompt.contains(part), "{part}");
        }
        assert!(!prompt.contains("PQA_PROMPT_PW"), "the agent does not need the env var");
        test.avoid.clear();
        test.login = None;
        let prompt = test.prompt();
        assert!(!prompt.contains("Never press") && !prompt.contains("type_password"));
    }

    #[test]
    fn avoid_list_blocks_taps_and_typing() {
        let mut state = test_state();
        state.elements = parse_screen(SAMPLE).1;
        state.avoid = vec!["wi-fi".into(), "DARK".into()];
        assert_eq!(
            avoided(&state.avoid, "Network / Wi-Fi settings"),
            Some("wi-fi"),
            "ignores case"
        );
        assert_eq!(avoided(&state.avoid, "Bluetooth"), None);
        let row = state.elements.iter().position(|e| e.label.contains("Wi")).unwrap() as i64 + 1;
        let error = tap(&mut state, row).unwrap_err();
        assert!(error.contains("refused") && error.contains("avoid list"), "{error}");
        let switch = state.elements.iter().position(|e| e.label == "Dark mode").unwrap() as i64 + 1;
        assert!(type_text(&mut state, switch, "x").unwrap_err().contains("refused"));
        assert!(state.history.is_empty(), "refused actions are not in the history");
    }

    #[test]
    fn type_password_only_types_into_password_fields() {
        let mut state = test_state();
        state.elements = parse_screen(SAMPLE).1;
        let email = state.elements.iter().position(|e| e.rid == "email").unwrap() as i64 + 1;
        assert!(
            type_password(&mut state, email)
                .unwrap_err()
                .contains("no test password")
        );

        state.password_env = Some("PQA_TEST_PASSWORD_MISSING".into());
        assert!(type_password(&mut state, email).unwrap_err().contains("is not set"));

        // SAFETY: tests use their own variable names; nothing else reads them.
        unsafe { std::env::set_var("PQA_TEST_PASSWORD_A", "s3cret-pw") };
        state.password_env = Some("PQA_TEST_PASSWORD_A".into());
        let error = type_password(&mut state, email).unwrap_err();
        assert!(
            error.contains("not a password field") && !error.contains("s3cret"),
            "{error}"
        );

        unsafe { std::env::set_var("PQA_TEST_PASSWORD_B", "pässword") };
        state.password_env = Some("PQA_TEST_PASSWORD_B".into());
        let error = type_password(&mut state, email).unwrap_err();
        assert!(!error.contains('ä'), "never quote the password: {error}");
        assert!(state.history.is_empty());
    }

    #[test]
    fn hides_the_test_password_in_output() {
        let mut state = test_state();
        assert_eq!(redact(&state, "no password set".into()), "no password set");
        // SAFETY: tests use their own variable names; nothing else reads them.
        unsafe { std::env::set_var("PQA_TEST_PASSWORD_C", "hunter22") };
        unsafe { std::env::set_var("PQA_TEST_PASSWORD_D", "abc") };
        state.password_env = Some("PQA_TEST_PASSWORD_C".into());
        assert_eq!(
            redact(&state, "[2] EditText \"hunter22\" edit".into()),
            "[2] EditText \"(hidden)\" edit"
        );
        state.password_env = Some("PQA_TEST_PASSWORD_D".into());
        assert_eq!(
            redact(&state, "abc is too short to hide".into()),
            "abc is too short to hide"
        );
    }

    #[test]
    fn run_folder_is_shared_by_run_and_session() {
        let api = Api::new("adopt");
        let test = parse_test(
            r#"{"app": "com.x", "goal": "Check it.", "avoid": ["Pay now"], "login": {"user": "u", "password_env": "PQA_ADOPT_PW"}}"#,
            Path::new("."),
        )
        .unwrap();
        let mut made = create_run(&api.reports, test.to_json()).unwrap();
        let header = fs::read_to_string(made.dir.join("report.md")).unwrap();
        assert!(header.contains("- App: com.x\n") && header.contains("- Goal: Check it.\n"));
        assert_eq!(
            json_body(api.get("/v1/runs/1"))["status"],
            "running",
            "run holds the lock"
        );

        let mut state = State::new(api.reports.clone());
        state.video_checked = true;
        state.run_dir = Some(made.dir.clone());
        assert_eq!(report_dir(&mut state).unwrap(), made.dir, "no new folder");
        assert_eq!(state.avoid, ["Pay now"]);
        assert_eq!(state.password_env.as_deref(), Some("PQA_ADOPT_PW"));
        made.lock = None;
        assert_eq!(
            json_body(api.get("/v1/runs/1"))["status"],
            "running",
            "the session holds it too"
        );

        let bug = Bug {
            title: "Cart is empty".into(),
            severity: "high".into(),
            package: "com.x".into(),
            body: "b".into(),
        };
        save_bug(&mut state, &bug, "").unwrap();
        finish_session(&mut state);
        assert!(eventually(|| !run_is_active(&made.dir)));
        let detail = json_body(api.get("/v1/runs/1"));
        assert_eq!(detail["status"], "finished");
        assert_eq!(detail["bugs"][0]["title"], "Cart is empty");
        assert_eq!(
            detail["test"]["goal"], "Check it.",
            "the session keeps the test settings"
        );
        assert!(detail.get("agent").is_none());
        assert_eq!(next_run_number(&api.reports), 2);

        let mut lost = State::new(api.reports.clone());
        lost.run_dir = Some(api.reports.join("run-009"));
        let reply = call_tool(&mut lost, &json!({"name": "back", "arguments": {}}));
        assert_eq!(reply["isError"], true, "no folder, no actions");
    }

    fn utc(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    /// The scheduler for one routine from `from` to `to`, one check every `step` seconds.
    fn simulate(when: When, tz: &TimeZone, from: &str, to: &str, step: i64) -> Vec<Due> {
        let (mut now, end) = (utc(from), utc(to));
        let start = now;
        let mut timer = new_timer(when, tz, now);
        let mut before = now;
        let mut out = Vec::new();
        while now <= end {
            let uptime = Duration::from_secs((now.as_second() - start.as_second()) as u64);
            out.extend(tick(&mut timer, when, tz, before, now, uptime));
            before = now;
            now = Timestamp::from_second(now.as_second() + step).unwrap();
        }
        out
    }

    #[test]
    fn reads_routines() {
        let base = Path::new("/qa");
        let list = parse_routines(
            r#"{"routines": [
                {"name": "nightly", "test": "shop.json", "at": "3:05"},
                {"name": "smoke-1", "test": "/abs/smoke.json", "every": "90m"},
                {"name": "often", "test": "t.json", "every": " 2h "}]}"#,
            base,
        )
        .unwrap();
        assert_eq!(
            list[0],
            Routine {
                name: "nightly".into(),
                test: "/qa/shop.json".into(),
                when: When::At(185)
            }
        );
        assert_eq!(list[1].test, Path::new("/abs/smoke.json"));
        assert_eq!((list[1].when, list[2].when), (When::Every(90), When::Every(120)));
        assert_eq!(When::At(185).describe(), "at 03:05");
        assert_eq!(When::Every(120).describe(), "every 2h");
        assert_eq!(When::Every(90).describe(), "every 90m");
        assert!(parse_routines(r#"{"routines": []}"#, base).unwrap().is_empty());

        for (every, minutes) in [
            ("1m", 1),
            ("15m", 15),
            ("45m", 45),
            ("60m", 60),
            ("1h", 60),
            ("8h", 480),
            ("12h", 720),
        ] {
            assert_eq!(parse_every(every), Ok(minutes), "{every}");
        }
        for every in [
            "0m", "7m", "25m", "5h", "24h", "1440m", "1d", "m", "6", "6H", "-6h", "+6h", "1.5h", "99999m", "６h",
        ] {
            assert!(parse_every(every).is_err(), "{every}");
        }
        assert_eq!(
            (parse_at("00:00"), parse_at("23:59"), parse_at("7:30")),
            (Ok(0), Ok(1439), Ok(450))
        );
        for at in [
            "24:00",
            "03:60",
            "3",
            "03:0",
            "003:00",
            "03:00:00",
            "-1:00",
            "3:00pm",
            "03:００",
            ":30",
        ] {
            assert!(parse_at(at).is_err(), "{at}");
        }

        let routine = |fields: &str| format!(r#"{{"routines": [{{{fields}}}]}}"#);
        for (bad, why) in [
            ("[]".to_string(), "must be a JSON object"),
            ("{".into(), "not valid JSON"),
            ("{}".into(), "\"routines\" must be a list"),
            (r#"{"routines": {}}"#.into(), "\"routines\" must be a list"),
            (r#"{"routines": [], "more": 1}"#.into(), "unknown field \"more\""),
            (r#"{"routines": ["x"]}"#.into(), "routine 1: must be an object"),
            (routine(r#""test": "t.json", "at": "03:00""#), "routine 1: \"name\" is required"),
            (routine(r#""name": 5, "test": "t.json", "at": "03:00""#), "routine 1: \"name\" must be a non-empty string"),
            (routine(r#""name": "a b", "test": "t.json", "at": "03:00""#), "only letters"),
            (routine(&format!(r#""name": "{}", "test": "t.json", "at": "03:00""#, "a".repeat(65))), "up to 64"),
            (routine(r#""name": "a", "at": "03:00""#), "routine \"a\": \"test\" is required"),
            (routine(r#""name": "a", "test": "t.json""#), "needs \"at\""),
            (routine(r#""name": "a", "test": "t.json", "at": "03:00", "every": "6h""#), "not both"),
            (routine(r#""name": "a", "test": "t.json", "at": "03:00", "every": null"#), "\"every\" must be a non-empty string"),
            (routine(r#""name": "a", "test": "t.json", "at": 300"#), "\"at\" must be a non-empty string"),
            (routine(r#""name": "a", "test": "t.json", "at": "25:00""#), "time of day"),
            (routine(r#""name": "a", "test": "t.json", "every": "7m""#), "split a day evenly"),
            (routine(r#""name": "a", "test": "t.json", "at": "03:00", "days": "mon""#), "unknown field \"days\""),
            (
                r#"{"routines": [{"name": "a", "test": "t.json", "at": "03:00"}, {"name": "a", "test": "u.json", "every": "1h"}]}"#.into(),
                "routine \"a\": the name is used twice",
            ),
        ] {
            let error = parse_routines(&bad, base).unwrap_err();
            assert!(error.contains(why), "{bad}: {error}");
        }
    }

    #[test]
    fn example_routines_file_is_valid() {
        let dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/examples"));
        let text = read_routines_file(&dir.join("routines.json")).unwrap().unwrap();
        let routines = parse_routines(&text, dir).unwrap();
        assert!(!routines.is_empty());
        for routine in routines {
            load_test(&routine.test).unwrap();
        }
    }

    #[test]
    fn reads_the_routines_file() {
        let dir = std::env::temp_dir().join(format!("phantomqa-routines-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("routines.json");
        assert_eq!(read_routines_file(&path), Ok(None), "no file, no routines");
        fs::write(&path, "{}").unwrap();
        assert_eq!(read_routines_file(&path), Ok(Some("{}".into())));
        assert!(read_routines_file(&dir).unwrap_err().contains("is not a file"));
        fs::write(&path, vec![b' '; MAX_ROUTINES_FILE as usize + 1]).unwrap();
        assert!(read_routines_file(&path).unwrap_err().contains("bigger than 1 MB"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn routine_times_follow_the_local_clock() {
        let t = utc("2026-11-11T13:00:00Z");
        let zone = TimeZone::UTC;
        assert_eq!(next_time(When::Every(360), &zone, t), Some(utc("2026-11-11T18:00:00Z")));
        assert_eq!(last_time(When::Every(360), &zone, t), Some(utc("2026-11-11T12:00:00Z")));
        assert_eq!(next_time(When::At(180), &zone, t), Some(utc("2026-11-12T03:00:00Z")));
        assert_eq!(last_time(When::At(180), &zone, t), Some(utc("2026-11-11T03:00:00Z")));
        let on = utc("2026-11-11T18:00:00Z");
        assert_eq!(
            last_time(When::Every(360), &zone, on),
            Some(on),
            "a time that is now has come"
        );
        assert_eq!(
            next_time(When::Every(360), &zone, on),
            Some(utc("2026-11-12T00:00:00Z"))
        );
        assert_eq!(
            next_time(When::At(0), &zone, utc("2028-12-31T23:59:00Z")),
            Some(utc("2029-01-01T00:00:00Z"))
        );
        assert_eq!(
            next_time(When::At(0), &zone, utc("2028-02-28T12:00:00Z")),
            Some(utc("2028-02-29T00:00:00Z"))
        );

        // 03:00 in Tbilisi (UTC+4) is 23:00 UTC the day before.
        let tbilisi = TimeZone::posix("<+04>-4").unwrap();
        assert_eq!(next_time(When::At(180), &tbilisi, t), Some(utc("2026-11-11T23:00:00Z")));
        assert_eq!(
            next_time(When::Every(360), &tbilisi, t),
            Some(utc("2026-11-11T14:00:00Z"))
        );
        assert_eq!(local_time(t, &tbilisi), "2026-11-11 17:00");
        assert_eq!(zone_label(&tbilisi, t), "UTC+04:00", "a zone without a name");
        assert_eq!(zone_label(&zone, t), "UTC (+00:00)");
    }

    #[test]
    fn routines_handle_daylight_saving() {
        let new_york = TimeZone::posix("EST5EDT,M3.2.0,M11.1.0").unwrap();
        // 2026-03-08: 02:00 jumps to 03:00, so 02:30 runs at 03:30.
        assert_eq!(
            next_time(When::At(150), &new_york, utc("2026-03-08T05:00:00Z")),
            Some(utc("2026-03-08T07:30:00Z"))
        );
        let runs = |times: &[&str]| -> Vec<Due> { times.iter().map(|t| Due::Run(utc(t))).collect() };
        // Every 30m through that night: each time once, none twice.
        assert_eq!(
            simulate(
                When::Every(30),
                &new_york,
                "2026-03-08T04:00:00Z",
                "2026-03-08T09:00:30Z",
                7
            ),
            runs(&[
                "2026-03-08T04:30:00Z",
                "2026-03-08T05:00:00Z",
                "2026-03-08T05:30:00Z",
                "2026-03-08T06:00:00Z",
                "2026-03-08T06:30:00Z",
                "2026-03-08T07:00:00Z",
                "2026-03-08T07:30:00Z",
                "2026-03-08T08:00:00Z",
                "2026-03-08T08:30:00Z",
                "2026-03-08T09:00:00Z",
            ])
        );
        // 2026-11-01: 01:00 to 02:00 happens twice. 01:30 runs the first time only.
        assert_eq!(
            simulate(
                When::At(90),
                &new_york,
                "2026-11-01T04:00:00Z",
                "2026-11-01T08:00:00Z",
                7
            ),
            runs(&["2026-11-01T05:30:00Z"])
        );
        assert_eq!(
            simulate(
                When::Every(30),
                &new_york,
                "2026-11-01T03:50:00Z",
                "2026-11-01T07:30:30Z",
                7
            ),
            runs(&[
                "2026-11-01T04:00:00Z",
                "2026-11-01T04:30:00Z",
                "2026-11-01T05:00:00Z",
                "2026-11-01T05:30:00Z",
                "2026-11-01T07:00:00Z",
                "2026-11-01T07:30:00Z",
            ])
        );
    }

    #[test]
    fn routines_skip_missed_times_and_clock_jumps() {
        let zone = TimeZone::UTC;
        let when = When::At(180);
        // `awake`: minutes since serve started, on the clock that never jumps.
        let mut timer = new_timer(when, &zone, utc("2026-11-11T03:00:00Z"));
        let mut step = |before: &str, now: &str, awake: u64| {
            tick(
                &mut timer,
                when,
                &zone,
                utc(before),
                utc(now),
                Duration::from_secs(awake * 60),
            )
        };
        assert_eq!(
            step("2026-11-11T03:00:00Z", "2026-11-11T03:00:01Z", 0),
            None,
            "added at 03:00: waits for tomorrow"
        );
        assert_eq!(
            step("2026-11-12T02:59:59Z", "2026-11-12T03:00:00Z", 1440),
            Some(Due::Run(utc("2026-11-12T03:00:00Z")))
        );
        assert_eq!(step("2026-11-12T03:00:00Z", "2026-11-12T03:00:01Z", 1440), None, "once");
        // The clock goes back half an hour: 03:00 does not run again.
        assert_eq!(step("2026-11-12T03:00:01Z", "2026-11-12T02:30:01Z", 1441), None);
        assert_eq!(
            step("2026-11-12T02:59:59Z", "2026-11-12T03:00:02Z", 1471),
            None,
            "not twice"
        );
        // Asleep from 02:00 to 05:00: 03:00 is missed, said once, and not made up.
        assert_eq!(
            step("2026-11-13T02:00:00Z", "2026-11-13T05:00:00Z", 2800),
            Some(Due::Missed(utc("2026-11-13T03:00:00Z")))
        );
        assert_eq!(step("2026-11-13T05:00:00Z", "2026-11-13T05:00:01Z", 2800), None);
        // Waking up less than a minute late still runs it; later is missed.
        assert_eq!(
            step("2026-11-14T02:00:00Z", "2026-11-14T03:01:00Z", 4000),
            Some(Due::Run(utc("2026-11-14T03:00:00Z")))
        );
        assert_eq!(
            step("2026-11-15T02:00:00Z", "2026-11-15T03:01:01Z", 5400),
            Some(Due::Missed(utc("2026-11-15T03:00:00Z")))
        );

        // The clock was a day ahead, then set right: the same 03:00 runs again a real day later.
        let mut timer = new_timer(when, &zone, utc("2026-11-12T02:00:00Z"));
        let mut step = |before: &str, now: &str, awake: u64| {
            tick(
                &mut timer,
                when,
                &zone,
                utc(before),
                utc(now),
                Duration::from_secs(awake * 60),
            )
        };
        assert_eq!(
            step("2026-11-12T02:59:59Z", "2026-11-12T03:00:00Z", 60),
            Some(Due::Run(utc("2026-11-12T03:00:00Z")))
        );
        assert_eq!(
            step("2026-11-12T05:00:00Z", "2026-11-11T05:00:00Z", 180),
            None,
            "set back"
        );
        assert_eq!(
            step("2026-11-12T02:59:59Z", "2026-11-12T03:00:00Z", 60 + 1440),
            Some(Due::Run(utc("2026-11-12T03:00:00Z")))
        );

        // Every 15m, asleep for two hours: one missed note, then on time again.
        let found = simulate(
            When::Every(15),
            &zone,
            "2026-11-11T10:00:00Z",
            "2026-11-11T10:20:00Z",
            5,
        );
        assert_eq!(found, [Due::Run(utc("2026-11-11T10:15:00Z"))]);
        let when = When::Every(15);
        let mut timer = new_timer(when, &zone, utc("2026-11-11T10:00:00Z"));
        let mut step = |before: &str, now: &str, awake: u64| {
            tick(
                &mut timer,
                when,
                &zone,
                utc(before),
                utc(now),
                Duration::from_secs(awake * 60),
            )
        };
        assert_eq!(
            step("2026-11-11T10:00:00Z", "2026-11-11T12:05:00Z", 3),
            Some(Due::Missed(utc("2026-11-11T12:00:00Z")))
        );
        assert_eq!(
            step("2026-11-11T12:05:00Z", "2026-11-11T12:15:00Z", 13),
            Some(Due::Run(utc("2026-11-11T12:15:00Z")))
        );
        // Every 15m, the clock goes back 30 minutes: the times in between run again, since real
        // time passes; only the same time within half a period is skipped.
        assert_eq!(step("2026-11-11T12:15:00Z", "2026-11-11T11:45:01Z", 13), None);
        assert_eq!(
            step("2026-11-11T11:59:59Z", "2026-11-11T12:00:00Z", 28),
            Some(Due::Run(utc("2026-11-11T12:00:00Z")))
        );
        assert_eq!(
            step("2026-11-11T12:14:59Z", "2026-11-11T12:15:00Z", 43),
            Some(Due::Run(utc("2026-11-11T12:15:00Z"))),
            "30 real minutes after it ran"
        );
    }

    #[test]
    fn worker_saves_the_agent_part_when_phantomqa_is_gone() {
        let root = std::env::temp_dir().join(format!("phantomqa-runner-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let folder = |name: &str| {
            let dir = root.join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("run.json"),
                json!({"status": "finished", "bugs": [], "test": {}}).to_string(),
            )
            .unwrap();
            fs::write(dir.join("report.md"), "# report\n").unwrap();
            dir
        };
        let agent_of = |dir: &Path| read_json(&dir.join("run.json")).unwrap()["agent"].clone();
        let sections = |dir: &Path| {
            fs::read_to_string(dir.join("report.md"))
                .unwrap()
                .matches("## Agent")
                .count()
        };

        // phantomqa is gone: the worker says the run was stopped.
        let gone = folder("gone");
        drop(hold_runner_lock(&gone).unwrap());
        stand_in_for_runner(&gone, "haiku");
        assert_eq!(agent_of(&gone)["ended"], "stopped");
        assert_eq!(agent_of(&gone)["model"], "haiku");
        assert!(
            fs::read_to_string(gone.join("report.md"))
                .unwrap()
                .contains("- Ended: stopped")
        );

        // phantomqa is alive: the worker waits, and never overwrites its part.
        let alive = folder("alive");
        let holder = hold_runner_lock(&alive).unwrap();
        let writer = {
            let alive = alive.clone();
            std::thread::spawn(move || {
                sleep(Duration::from_millis(300));
                let done = AgentRun {
                    ended: "done",
                    summary: "all good".into(),
                    cost_usd: Some(0.01),
                    turns: Some(3),
                    error: None,
                };
                save_agent(&alive, "haiku", &done);
                drop(holder);
            })
        };
        stand_in_for_runner(&alive, "haiku");
        writer.join().unwrap();
        assert_eq!(agent_of(&alive)["ended"], "done");
        assert_eq!(sections(&alive), 1);

        // A run folder without the lock file: nothing to do.
        let old = folder("old");
        stand_in_for_runner(&old, "haiku");
        assert!(agent_of(&old).is_null());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn running_list_frees_a_routine_when_it_ends() {
        let running: Arc<Mutex<Vec<String>>> = Arc::default();
        running.lock().unwrap().extend(["a".to_string(), "b".to_string()]);
        drop(Running(running.clone(), "a".into()));
        assert_eq!(*running.lock().unwrap(), ["b"]);
    }
}
