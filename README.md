# PhantomQA

AI agents test Android apps on an emulator and report what breaks.

PhantomQA is one small Rust binary. It lets an AI agent (for example Claude Code) see the screen, tap, type and go back, like a person. It catches crashes by itself, and the agent files bug reports with a screenshot and the app log. Everything goes through `adb`.

## Requirements

| What | Needed | Why |
|---|---|---|
| [Rust](https://rustup.rs) | to build | |
| `adb` from [Android platform-tools](https://developer.android.com/tools/releases/platform-tools) | yes | talks to the device |
| An Android emulator or phone with USB debugging | yes | the app under test runs here |
| [ffmpeg](https://ffmpeg.org) | no | video clips per bug (planned) |

Check your setup:

```sh
cargo build --release
./target/release/phantomqa doctor
```

`doctor` prints one line per check and a fix for each problem:

```
ok    adb: Android Debug Bridge version 1.0.41 (adb)
ok    device: sdk_gphone64_x86_64, Android 14 (API 34)
ok    screen reading: works (16 elements on screen)
ok    screenrecord: /system/bin/screenrecord
warn  ffmpeg: not found
      fix: install ffmpeg, or set --ffmpeg or FFMPEG; ...
ok    reports: reports is writable
```

## Use from Claude Code (MCP)

```sh
claude mcp add phantomqa -- /path/to/phantomqa mcp
```

Or copy `.mcp.json.example` to `.mcp.json` in your project. Add options to `args` if needed, for example `["--adb", "/path/to/adb", "mcp"]`.

Then ask Claude Code something like: "Test the app com.example.app. Try the main features and report every bug."

Tools the agent gets:

| Tool | What it does |
|---|---|
| `launch_app` | start an installed app by package name |
| `screen` | read the screen as a numbered list |
| `tap` | tap element N |
| `type_text` | tap element N and type text |
| `back` | press the back button |
| `crashes` | check for new crashes and ANRs |
| `report_bug` | save a bug: title, steps, expected, actual, severity |

Every action returns the new screen, so the agent needs no extra call to see the result. The screen looks like this:

```
app: com.android.settings
[1] EditText "Search settings" #search_action_bar edit
[2] LinearLayout "Network & internet / Mobile, Wi‑Fi, hotspot" click
[3] RecyclerView #recycler_view scroll
```

## Use by hand

```sh
phantomqa launch com.android.settings   # start an app, print its screen
phantomqa screen                        # print the current screen
phantomqa tap 3                         # tap element [3]
phantomqa type 2 hello world            # tap element [2] and type text
phantomqa back                          # press back
phantomqa crashes                       # list crashes and ANRs in the device's crash log
```

## Crashes and bug reports

- After every action, PhantomQA reads the device's crash log. A new crash, native crash or ANR is shown to the agent and saved to the report automatically.
- The agent calls `report_bug` for other problems it finds.
- Each bug gets a screenshot, the app's log and the list of recent actions, so a person can follow the same path.

Each test run gets the next numbered folder:

```
reports/run-001/
  report.md      one section per bug
  bug-01.png     screenshot
  bug-01.log     crash lines and the app's recent log
```

## Options

Put options before the command. Each option also has an env var, which is handy in MCP configs. An option wins over its env var.

| Option | Env var | Default |
|---|---|---|
| `--adb PATH` | `ADB` | `$ANDROID_HOME/platform-tools/adb` (or `ANDROID_SDK_ROOT`), then `adb` on PATH |
| `--serial ID` | `ANDROID_SERIAL` | the only connected device |
| `--reports DIR` | `PHANTOMQA_REPORTS` | `reports` |
| `--ffmpeg PATH` | `FFMPEG` | `ffmpeg` on PATH |
| `--no-video` | `PHANTOMQA_VIDEO=0` | video on |

## Limits

- `type_text` types printable ASCII only.
- Apps that draw their own UI (games, Flutter without semantics) show few or no elements.
- Reading the screen with `uiautomator dump` takes about 1-3 seconds.

## License

[MIT](LICENSE)
