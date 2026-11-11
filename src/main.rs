//! PhantomQA, step 1: let an AI agent see and use an Android emulator.
//!
//! `phantomqa mcp` runs an MCP server over stdio (for Claude Code and other agents).
//! `phantomqa screen | tap N | type N TEXT | back | launch PACKAGE` does the same by hand.
//!
//! Everything goes through `adb`. Set ANDROID_SERIAL when more than one device is connected.

use serde_json::{Value, json};
use std::io::{BufRead, Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// How long to wait after an action before reading the screen again.
const SETTLE: Duration = Duration::from_millis(500);
/// An adb command that runs longer than this is killed.
const ADB_TIMEOUT: Duration = Duration::from_secs(30);
/// Longer labels are cut, to keep the screen list small.
const MAX_LABEL: usize = 80;

/// One thing on the screen the agent can refer to by number.
#[derive(Debug, Clone, PartialEq)]
struct Element {
    kind: String,
    label: String,
    rid: String,
    flags: Vec<&'static str>,
    center: (i32, i32),
}

/// The elements from the last screen read. Numbers in `tap N` index into this list.
#[derive(Default)]
struct State {
    elements: Vec<Element>,
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

/// Runs adb with a timeout and returns its stdout.
fn adb(args: &[&str]) -> Result<String, String> {
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

    let stdout = String::from_utf8_lossy(&out_reader.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err_reader.join().unwrap_or_default()).into_owned();
    if !status.success() {
        let detail = format!("{} {}", stderr.trim(), stdout.trim());
        return Err(format!("adb {} failed: {}", args.join(" "), detail.trim()));
    }
    Ok(stdout)
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
    Ok(text)
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
    let (x, y) = e.center;
    adb(&["shell", "input", "tap", &x.to_string(), &y.to_string()])?;
    sleep(Duration::from_millis(300));
    adb(&["shell", "input", "text", &arg])?;
    sleep(SETTLE);
    read_screen(state)
}

fn back(state: &mut State) -> Result<String, String> {
    adb(&["shell", "input", "keyevent", "KEYCODE_BACK"])?;
    sleep(SETTLE);
    read_screen(state)
}

fn launch_app(state: &mut State, package: &str) -> Result<String, String> {
    let valid = !package.is_empty() && package.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_');
    if !valid {
        return Err(format!("not a valid package name: {package:?}"));
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

// ---------------------------------------------------------------- MCP server

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
    ])
}

fn call_tool(state: &mut State, params: &Value) -> Value {
    let name = params["name"].as_str().unwrap_or("");
    let args = &params["arguments"];
    let int = |key: &str| args[key].as_i64().ok_or_else(|| format!("argument {key:?} must be an integer"));
    let text = |key: &str| {
        args[key].as_str().map(str::to_string).ok_or_else(|| format!("argument {key:?} must be a string"))
    };
    let result = match name {
        "screen" => read_screen(state),
        "tap" => int("id").and_then(|id| tap(state, id)),
        "type_text" => int("id").and_then(|id| text("text").and_then(|t| type_text(state, id, &t))),
        "back" => back(state),
        "launch_app" => text("package").and_then(|p| launch_app(state, &p)),
        _ => Err(format!("unknown tool: {name}")),
    };
    match result {
        Ok(text) => json!({"content": [{"type": "text", "text": text}]}),
        Err(error) => json!({"content": [{"type": "text", "text": error}], "isError": true}),
    }
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
fn serve_mcp() {
    let mut state = State::default();
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
  phantomqa launch PACKAGE      start an app";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut state = State::default();
    let id = |s: &str| s.parse::<i64>().map_err(|_| format!("not a number: {s}"));

    let result = match args[..] {
        ["mcp"] => {
            serve_mcp();
            return;
        }
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
        let state = State { elements: parse_screen(SAMPLE).1 };
        assert!(element(&state, 1).is_ok());
        assert!(element(&state, 0).is_err());
        assert!(element(&state, 6).is_err());
        assert!(element(&state, -1).is_err());
    }

    #[test]
    fn mcp_handshake_and_errors() {
        let mut state = State::default();
        let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}});
        let reply = handle_message(&mut state, &init).unwrap();
        assert_eq!(reply["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(reply["result"]["serverInfo"]["name"], "phantomqa");

        let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert!(handle_message(&mut state, &note).is_none());

        let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        let tools = handle_message(&mut state, &list).unwrap();
        assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 5);

        let bad_tap = json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "tap", "arguments": {"id": "x"}}});
        let reply = handle_message(&mut state, &bad_tap).unwrap();
        assert_eq!(reply["result"]["isError"], true);

        let unknown = json!({"jsonrpc": "2.0", "id": 4, "method": "nope"});
        assert_eq!(handle_message(&mut state, &unknown).unwrap()["error"]["code"], -32601);
    }
}
