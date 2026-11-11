//! PhantomQA: an AI agent tests an Android app on an emulator and reports bugs.
//!
//! `phantomqa mcp` runs an MCP server over stdio (for Claude Code and other agents).
//! `phantomqa screen | tap N | type N TEXT | back | launch PACKAGE | crashes` does the same by hand.
//!
//! Everything goes through `adb`. Set ANDROID_SERIAL when more than one device is connected.
//! Bug reports go to ./reports/<start time>/ (change the root with PHANTOMQA_REPORTS).

use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    /// When this run started, for the report header.
    started: String,
    /// Created on the first bug.
    report: Option<Report>,
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
            started: host_time("%Y-%m-%d %H:%M:%S"),
            report: None,
        }
    }

    fn remember(&mut self, action: String) {
        self.history.push(action);
        if self.history.len() > MAX_HISTORY {
            self.history.remove(0);
        }
    }
}

// ---------------------------------------------------------------- adb

fn adb_path() -> String {
    if let Ok(path) = std::env::var("ADB") {
        return path;
    }
    for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Ok(home) = std::env::var(var) {
            let path = format!("{home}/platform-tools/adb");
            if std::path::Path::new(&path).exists() {
                return path;
            }
        }
    }
    "adb".into()
}

/// Runs adb with a timeout and returns its stdout as text.
fn adb(args: &[&str]) -> Result<String, String> {
    adb_bytes(args).map(|out| String::from_utf8_lossy(&out).into_owned())
}

/// Runs adb with a timeout and returns its raw stdout.
fn adb_bytes(args: &[&str]) -> Result<Vec<u8>, String> {
    let mut child = Command::new(adb_path())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run adb ({}): {e}. Set ADB or ANDROID_HOME.", adb_path()))?;

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

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() > ADB_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("adb {} timed out after {}s", args.join(" "), ADB_TIMEOUT.as_secs()));
            }
            Ok(None) => sleep(Duration::from_millis(10)),
            Err(e) => return Err(format!("adb {} failed: {e}", args.join(" "))),
        }
    };

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
    !package.is_empty() && package.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
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
        let cmd = "uiautomator dump /sdcard/phantomqa-dump.xml >/dev/null && cat /sdcard/phantomqa-dump.xml";
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
    let end = out.rfind("</hierarchy>")? + "</hierarchy>".len();
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
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()).unwrap_or("")
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
        nodes.push(Node { depth, attrs: parse_attrs(tag) });
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
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= MAX_LABEL {
        return one_line;
    }
    let cut: String = one_line.chars().take(MAX_LABEL - 1).collect();
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

        let Some((l, t, r, b)) = parse_bounds(node.get("bounds")) else { continue };
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

/// `input text` reads "%s" as a space and cannot type non-ASCII characters.
fn input_text_arg(text: &str) -> Result<String, String> {
    if text.is_empty() {
        return Err("text is empty".into());
    }
    if let Some(c) = text.chars().find(|c| !c.is_ascii() || c.is_ascii_control()) {
        return Err(format!("cannot type {c:?}: only printable ASCII is supported for now"));
    }
    Ok(shell_quote(&text.replace(' ', "%s")))
}

fn type_text(state: &mut State, id: i64, text: &str) -> Result<String, String> {
    let arg = input_text_arg(text)?;
    let e = element(state, id)?;
    state.remember(format!("type_text {}: {text:?}", describe(id, &e)));
    let (x, y) = e.center;
    adb(&["shell", "input", "tap", &x.to_string(), &y.to_string()])?;
    sleep(Duration::from_millis(300));
    adb(&["shell", "input", "text", &arg])?;
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
    ])?;
    let component = resolved.lines().map(str::trim).rfind(|l| l.contains('/'));
    let Some(component) = component else {
        return Err(format!("{package} is not installed or has no launcher screen"));
    };
    let started = adb(&["shell", "am", "start", "-W", "-n", &shell_quote(component)])?;
    if let Some(line) = started.lines().find(|l| l.starts_with("Error")) {
        return Err(format!("could not start {package}: {}", line.trim()));
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
        format!("{kind} in {}: {}", self.process, self.summary)
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
            let summary = if kind == "crash" { String::new() } else { l.msg.to_string() };
            crashes.push(Crash { kind, process, summary, lines: vec![l.msg.to_string()] });
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
        Ok(crashes) => crashes,
        Err(e) => return format!("\n(could not check for crashes: {e})\n"),
    };
    let mut out = String::new();
    for crash in &crashes {
        out.push('\n');
        out.push_str(&render_crash(crash));
        match save_crash(state, crash) {
            Ok(n) => out.push_str(&format!("   Saved to the report as bug {n}. Do not report it again.\n")),
            Err(e) => out.push_str(&format!("   Could not save it to the report: {e}\n")),
        }
    }
    out
}

// ---------------------------------------------------------------- report

/// Local time from the host `date` command, or epoch seconds if that fails.
fn host_time(format: &str) -> String {
    Command::new("date")
        .arg(format!("+{format}"))
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            secs.to_string()
        })
}

fn device_name() -> String {
    match adb(&["shell", "getprop ro.product.model; getprop ro.build.version.release"]) {
        Ok(out) => {
            let mut lines = out.lines().map(str::trim);
            let model = lines.next().unwrap_or("?");
            let version = lines.next().unwrap_or("?");
            format!("{model}, Android {version}")
        }
        Err(_) => "unknown".into(),
    }
}

/// The report folder. Creates it with report.md on the first bug.
fn report_dir(state: &mut State) -> Result<PathBuf, String> {
    if let Some(report) = &state.report {
        return Ok(report.dir.clone());
    }
    let root = &state.report_root;
    fs::create_dir_all(root).map_err(|e| format!("cannot create {}: {e}", root.display()))?;
    // "2026-11-11 10:11:19" -> "2026-11-11_10-11-19"
    let stamp = state.started.replace(' ', "_").replace(':', "-");
    let mut dir = root.join(&stamp);
    let mut n = 1;
    loop {
        match fs::create_dir(&dir) {
            Ok(()) => break,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                n += 1;
                dir = root.join(format!("{stamp}-{n}"));
            }
            Err(e) => return Err(format!("cannot create {}: {e}", dir.display())),
        }
    }
    let header = format!("# PhantomQA test report\n\n- Started: {}\n- Device: {}\n", state.started, device_name());
    fs::write(dir.join("report.md"), header).map_err(|e| format!("cannot write report.md: {e}"))?;
    state.report = Some(Report { dir: dir.clone(), bugs: 0 });
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

fn render_bug(n: usize, bug: &Bug, time: &str, history: &[String], attachments: &str) -> String {
    let mut out = format!("\n## Bug {n}: {}\n\n", bug.title);
    out.push_str(&format!("- Severity: {}\n", bug.severity));
    if !bug.package.is_empty() {
        out.push_str(&format!("- App: {}\n", bug.package));
    }
    out.push_str(&format!("- Time: {time}\n\n{}\n", bug.body.trim_end()));
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
    let n = state.report.as_ref().map_or(0, |r| r.bugs) + 1;
    let name = format!("bug-{n:02}");

    let shot = match screenshot().and_then(|png| {
        fs::write(dir.join(format!("{name}.png")), png).map_err(|e| e.to_string())
    }) {
        Ok(()) => format!("![screenshot]({name}.png)"),
        Err(e) => format!("(no screenshot: {e})"),
    };
    let log = format!("{extra_log}{}", app_log(&bug.package));
    let log_link = match fs::write(dir.join(format!("{name}.log")), log) {
        Ok(()) => format!("Log: [{name}.log]({name}.log)"),
        Err(e) => format!("(no log: {e})"),
    };

    let section = render_bug(n, bug, &host_time("%H:%M:%S"), &state.history, &format!("{shot}\n\n{log_link}"));
    fs::OpenOptions::new()
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
        body: format!("Found automatically in logcat.\n\n```\n{lines}\n```\n"),
    };
    save_bug(state, &bug, &format!("--- {} ---\n{lines}\n\n--- recent log ---\n", crash.kind))
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
    let title = text("title")?;
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
    Ok(Bug { title, severity, package: package.to_string(), body })
}

fn report_bug(state: &mut State, args: &Value) -> Result<String, String> {
    let bug = bug_from_args(args, &state.package)?;
    let n = save_bug(state, &bug, "")?;
    let path = state.report.as_ref().map(|r| r.dir.join("report.md")).unwrap_or_default();
    Ok(format!("Saved bug {n} to {}, with a screenshot and the app log.", path.display()))
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
            "description": "Save a bug to the test report. A screenshot of the current screen and the app log are attached automatically, so call this while the bug is on screen. Crashes are saved automatically; do not report them again.",
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
    let int = |key: &str| args[key].as_i64().ok_or_else(|| format!("argument {key:?} must be an integer"));
    let text = |key: &str| {
        args[key].as_str().map(str::to_string).ok_or_else(|| format!("argument {key:?} must be a string"))
    };
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
    let notice = if checks_crashes { crash_notice(state) } else { String::new() };

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
}

// ---------------------------------------------------------------- CLI

const USAGE: &str = "usage:
  phantomqa mcp                 MCP server over stdio
  phantomqa screen              print the current screen
  phantomqa tap N               tap element N
  phantomqa type N TEXT         tap element N and type TEXT
  phantomqa back                press back
  phantomqa launch PACKAGE      start an app
  phantomqa crashes             list crashes and ANRs in the device's crash log";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let report_root = std::env::var_os("PHANTOMQA_REPORTS").map_or_else(|| PathBuf::from("reports"), PathBuf::from);
    let mut state = State::new(report_root);
    let id = |s: &str| s.parse::<i64>().map_err(|_| format!("not a number: {s}"));

    let result = match args[..] {
        ["mcp"] => {
            serve_mcp(state);
            return;
        }
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
        State::new(std::env::temp_dir().join("phantomqa-test-unused"))
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
    }

    #[test]
    fn unescapes_entities() {
        assert_eq!(unescape("a &amp; b &lt;c&gt; &quot;d&quot; &#39;e&#x27;"), "a & b <c> \"d\" 'e'");
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
        assert_eq!(input_text_arg("hi there; rm -rf").unwrap(), "'hi%sthere;%srm%s-rf'");
        assert!(input_text_arg("").is_err());
        assert!(input_text_arg("héllo").is_err());
        assert!(input_text_arg("a\nb").is_err());
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
        let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}});
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
        assert_eq!(crashes[0].lines.len(), 4, "the other process's line is not part of this crash");
        assert!(crashes[0].lines[3].starts_with("\tat android.app"));

        assert_eq!(crashes[1].kind, "ANR");
        assert_eq!(crashes[1].process, "com.example.app");
        assert_eq!(crashes[1].summary, "Input dispatching timed out");

        assert_eq!(crashes[2].kind, "native crash");
        assert_eq!(crashes[2].process, "com.example.game");
        assert_eq!(crashes[2].lines.len(), 3);
        assert_eq!(crashes[2].title(), format!("Native crash in com.example.game: {}", crashes[2].lines[0]));

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

        let as_text = json!({"title": "t", "steps": "1) First\n2) Second", "expected": "e", "actual": "a", "severity": "HIGH"});
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
        let history = vec!["launch_app com.example.app".to_string(), "tap [3] Button \"Log in\"".to_string()];
        let text = render_bug(2, &bug, "15:06:12", &history, "![screenshot](bug-02.png)");
        assert_eq!(
            text,
            "\n## Bug 2: Login does nothing\n\n- Severity: high\n- App: com.example.app\n- Time: 15:06:12\n\n\
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
    fn report_folders_never_collide() {
        let root = std::env::temp_dir().join(format!("phantomqa-test-{}", std::process::id()));
        let mut a = State::new(root.clone());
        let mut b = State::new(root.clone());
        let dir_a = report_dir(&mut a).unwrap();
        let dir_b = report_dir(&mut b).unwrap();
        assert_ne!(dir_a, dir_b);
        assert_eq!(report_dir(&mut a).unwrap(), dir_a, "same run reuses its folder");
        let header = fs::read_to_string(dir_a.join("report.md")).unwrap();
        assert!(header.starts_with("# PhantomQA test report\n"));
        fs::remove_dir_all(root).unwrap();
    }
}
