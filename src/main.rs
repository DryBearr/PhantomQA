//! PhantomQA: an AI agent tests an Android app on an emulator and reports bugs.
//!
//! `phantomqa mcp` runs an MCP server over stdio (for Claude Code and other agents).
//! `phantomqa screen | tap N | type N TEXT | back | launch PACKAGE | crashes` does the same by hand.
//!
//! Everything goes through `adb`. Run `phantomqa doctor` to check the setup.
//! Bug reports go to ./reports/run-NNN/. See `phantomqa --help` for the paths you can set.

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
}

struct Report {
    dir: PathBuf,
    bugs: usize,
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
    let (mut adb, mut ffmpeg, mut reports, mut serial, mut tokens) = (None, None, None, None, None);
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

/// Runs adb with a timeout and returns its raw stdout.
fn adb_bytes(args: &[&str]) -> Result<Vec<u8>, String> {
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

    let status = wait_with_timeout(&mut child, ADB_TIMEOUT).map_err(|e| format!("adb {}: {e}", args.join(" ")))?;

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

fn tap(state: &mut State, id: i64) -> Result<String, String> {
    let e = element(state, id)?;
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
    let args = input_text_args(text)?;
    let e = element(state, id)?;
    // Keep passwords out of the report.
    let shown = if e.flags.contains(&"password") {
        "(hidden)".to_string()
    } else {
        format!("{text:?}")
    };
    state.remember(format!("type_text {}: {shown}", describe(id, &e)));
    let (x, y) = e.center;
    adb(&["shell", "input", "tap", &x.to_string(), &y.to_string()])?;
    sleep(Duration::from_millis(300));
    for arg in &args {
        adb(&["shell", "input", "text", arg])?;
    }
    sleep(SETTLE);
    read_screen(state)
}

fn back(state: &mut State) -> Result<String, String> {
    state.remember("back".into());
    adb(&["shell", "input", "keyevent", "KEYCODE_BACK"])?;
    sleep(SETTLE);
    read_screen(state)
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
            let installed = adb(&["shell", "pm", "path", package]).is_ok_and(|p| p.contains("package:"));
            if !installed {
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

/// The report folder, reports/run-NNN. Creates it with report.md on first use.
fn report_dir(state: &mut State) -> Result<PathBuf, String> {
    if let Some(report) = &state.report {
        return Ok(report.dir.clone());
    }
    let root = &state.report_root;
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
    let header = format!("# PhantomQA test report: run {run}\n\n- Device: {device}\n");
    fs::write(dir.join("report.md"), header).map_err(|e| format!("cannot write report.md: {e}"))?;
    state.report = Some(Report {
        dir: dir.clone(),
        bugs: 0,
    });
    Ok(dir)
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
    if config().screenshots {
        let saved =
            screenshot().and_then(|png| fs::write(dir.join(format!("{name}.png")), png).map_err(|e| e.to_string()));
        attachments.push(match saved {
            Ok(()) => format!("![screenshot]({name}.png)"),
            Err(e) => format!("(no screenshot: {e})"),
        });
    }
    let log = format!("{extra_log}{}", app_log(&bug.package));
    attachments.push(match fs::write(dir.join(format!("{name}.log")), log) {
        Ok(()) => format!("Log: [{name}.log]({name}.log)"),
        Err(e) => format!("(no log: {e})"),
    });
    if let Some(video) = &mut state.video {
        attachments.push(video_clip(video, &dir, &name));
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
fn video_clip(video: &mut Video, run_dir: &Path, name: &str) -> String {
    let parts = match video.cut() {
        Ok(parts) => parts,
        Err(e) => return format!("(no video: {e})"),
    };
    let clip = run_dir.join(format!("{name}.mp4"));
    match make_video(&parts, &clip) {
        Ok(()) => format!("Video: [{name}.mp4]({name}.mp4)"),
        Err(e) => {
            video.parts_linked = true;
            format!("Video parts ({e}): {}", part_links(&parts))
        }
    }
}

/// Ends the session: stops recording, makes session.mp4 and adds a video note to the report.
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
            None => return,
        },
    };
    // Only write into a report that exists; video off and no bugs means no folder.
    if let Some(report) = &state.report {
        let _ = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(report.dir.join("report.md"))
            .and_then(|mut file| file.write_all(format!("\n## Video\n\n{note}\n").as_bytes()));
    }
}

// ---------------------------------------------------------------- MCP server

const INSTRUCTIONS: &str = "PhantomQA lets you test an Android app on an emulator like a real user. \
Start with launch_app, read the numbered screen list, and use tap, type_text and back. \
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
        start_video(state);
    }
    // Start the crash clock before the first action, so a crash it causes is not missed.
    if state.crash_since.is_none() {
        state.crash_since = device_now().ok();
    }
    let result = match name {
        "screen" => read_screen(state),
        "tap" => int("id").and_then(|id| tap(state, id)),
        "type_text" => int("id").and_then(|id| text("text").and_then(|t| type_text(state, id, &t))),
        "back" => back(state),
        "launch_app" => text("package").and_then(|p| launch_app(state, &p)),
        "crashes" => Ok(String::new()),
        "report_bug" => report_bug(state, args),
        _ => Err(format!("unknown tool: {name}")),
    };
    let checks_crashes = matches!(name, "screen" | "tap" | "type_text" | "back" | "launch_app" | "crashes");
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

// ---------------------------------------------------------------- doctor

/// Prints one line per check. Returns false if something required is missing.
fn doctor() -> bool {
    let config = config();
    let mut ok = true;
    let mut check = |required: bool, what: &str, result: Result<String, String>, fix: &str| match result {
        Ok(detail) => println!("ok    {what}: {detail}"),
        Err(error) => {
            let status = if required { "FAIL" } else { "warn" };
            println!("{status:<5} {what}: {error}\n      fix: {fix}");
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

    let on_off = |on: bool| if on { "on" } else { "off" };
    println!(
        "info  video: {}, screenshots: {}",
        on_off(config.video),
        on_off(config.screenshots)
    );
    ok
}

// ---------------------------------------------------------------- CLI

const USAGE: &str = "usage: phantomqa [options] <command>

commands:
  mcp                 MCP server over stdio (for Claude Code and other agents)
  doctor              check adb, the device, ffmpeg and the reports folder
  screen              print the current screen
  tap N               tap element N
  type N TEXT         tap element N and type TEXT
  back                press back
  launch PACKAGE      start an app
  crashes             list crashes and ANRs in the device's crash log
  token ...           API tokens: add, rotate, remove, list, check (see `phantomqa token`)

options (each also has an env var):
  --adb PATH          adb binary            ADB  (default: $ANDROID_HOME/platform-tools/adb, then PATH)
  --serial ID         device to use         ANDROID_SERIAL  (default: the only device connected)
  --reports DIR       reports folder        PHANTOMQA_REPORTS  (default: reports)
  --ffmpeg PATH       ffmpeg, optional      FFMPEG  (default: PATH)
  --no-video          do not record video   PHANTOMQA_VIDEO=0
  --no-screenshots    no screenshots        PHANTOMQA_SCREENSHOTS=0
  --tokens PATH       API tokens file       PHANTOMQA_TOKENS  (default: tokens.json)";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args
        .iter()
        .take_while(|a| a.starts_with('-'))
        .any(|a| a == "--help" || a == "-h")
    {
        println!("{USAGE}");
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
            serve_mcp(state);
            return;
        }
        ["doctor"] => std::process::exit(if doctor() { 0 } else { 1 }),
        ["token", ref rest @ ..] => token_command(rest),
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
        Ok(text) => print!("{text}"),
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
        assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 7);

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
            _ => None,
        };
        let (config, _) = load_config(&[], &env).unwrap();
        assert_eq!(config.adb, "/env/adb");
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
        assert!(report.ends_with("\n## Video\n\nVideo was off: turned off.\n"));
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
}
