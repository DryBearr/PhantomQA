# PhantomQA

AI agents test Android apps on an emulator and report what breaks.

PhantomQA is one small Rust binary. It lets an AI agent (for example Claude Code) see the screen, tap, type and go back, like a person. It catches crashes by itself, and the agent files bug reports with a screenshot, a video clip and the app log. Everything goes through `adb`.

## Requirements

| What | Needed | Why |
|---|---|---|
| [Rust](https://rustup.rs) | to build | |
| `adb` from [Android platform-tools](https://developer.android.com/tools/releases/platform-tools) | yes | talks to the device |
| An Android emulator or phone with USB debugging | yes | the app under test runs here |
| [ffmpeg](https://ffmpeg.org) | no | joins video parts into one clip per bug and `session.mp4` |

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

- After every action, PhantomQA reads the device's crash log. A new crash, native crash or ANR of the app under test is shown to the agent and saved to the report automatically.
- The same crash is saved once, even in a crash loop. Crashes of other apps are mentioned to the agent, not saved.
- The agent calls `report_bug` for other problems it finds.
- Each bug gets a screenshot, a video clip, the app's log and the list of recent actions, so a person can follow the same path. Text typed into password fields is hidden in the report.

Each test run gets the next numbered folder:

```
reports/run-001/
  report.md      one section per bug, and the video at the end
  bug-01.png     screenshot
  bug-01.mp4     video of the last 15-60 seconds before the bug
  bug-01.log     crash lines and the app's recent log
  session.mp4    the whole session
```

## Video

In an MCP session, PhantomQA records the screen with `screenrecord` in 30-second parts. When a bug is saved, the current part ends there, so the clip shows what led to the bug. When the agent closes the session, the parts are joined into `session.mp4`.

- Without ffmpeg, a clip of one part is still made. Clips that need two parts, and the session, stay as parts in `video/`, linked from the report.
- Android only records a frame when the screen changes, so still moments are shorter in the video than in real time.
- Screens that block screenshots (some banking apps, password screens) record as black.
- If PhantomQA is killed instead of closed, `session.mp4` is not made; the parts stay in `video/`.
- Turn video off with `--no-video`, screenshots with `--no-screenshots`.

## API tokens

Tokens will protect the HTTP API (next step). Give each client, such as a dashboard or CI, its own token.

```sh
phantomqa token add dashboard --expires 90d   # prints pqa_... once; store it in the dashboard
phantomqa token list                          # names, ids and expiry, never the tokens
phantomqa token rotate dashboard --grace 1h   # new token; the old one works for 1 more hour
phantomqa token remove dashboard --old        # end that hour early
phantomqa token remove dashboard              # revoke now
echo "$TOKEN" | phantomqa token check         # does this token work?
```

- Clients send `Authorization: Bearer pqa_...`.
- `tokens.json` keeps only SHA-256 hashes, is readable only by you, and is written atomically under a lock.
- Removing or rotating a token takes effect on the next request. A broken `tokens.json` refuses every token.
- Tokens go to stdout alone, so `TOKEN=$(phantomqa token add ci)` works.

## Options

Put options before the command. Each option also has an env var, which is handy in MCP configs. An option wins over its env var.

| Option | Env var | Default |
|---|---|---|
| `--adb PATH` | `ADB` | `$ANDROID_HOME/platform-tools/adb` (or `ANDROID_SDK_ROOT`), then `adb` on PATH |
| `--serial ID` | `ANDROID_SERIAL` | the only connected device |
| `--reports DIR` | `PHANTOMQA_REPORTS` | `reports` |
| `--ffmpeg PATH` | `FFMPEG` | `ffmpeg` on PATH |
| `--no-video` | `PHANTOMQA_VIDEO=0` | video on |
| `--no-screenshots` | `PHANTOMQA_SCREENSHOTS=0` | screenshots on |
| `--tokens PATH` | `PHANTOMQA_TOKENS` | `tokens.json` |

Example: `phantomqa --serial emulator-5554 --reports ~/qa --no-video mcp`

## Limits

- `type_text` types printable ASCII only.
- No swipe or scroll yet, so long lists can only be tested as far as they show.
- Apps that draw their own UI (games, Flutter without semantics) show few or no elements.
- Reading the screen with `uiautomator dump` takes about 1-3 seconds.

## License

[MIT](LICENSE)
