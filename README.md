# PhantomQA

AI agents test Android apps on an emulator and report what breaks.

One Rust binary lets an agent see and use the emulator, find crashes and write a bug report. It works through MCP or by hand. Everything goes through `adb`.

## Build

```sh
cargo build --release
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

The screen is a short numbered list, for example:

```
app: com.android.settings
[1] EditText "Search settings" #search_action_bar edit
[2] LinearLayout "Network & internet / Mobile, Wi‑Fi, hotspot" click
[3] RecyclerView #recycler_view scroll
```

Every action prints the new screen, so the agent does not need a second call to see the result.

## Use from Claude Code (MCP)

```sh
claude mcp add phantomqa -e ANDROID_HOME=$ANDROID_HOME -- $PWD/target/release/phantomqa mcp
```

Then ask Claude Code something like: "Open com.android.settings, turn on dark mode and tell me what you see."

Tools: `screen`, `tap`, `type_text`, `back`, `launch_app`, `crashes`, `report_bug`.

## Crashes and bug reports

- After every action, PhantomQA reads the device's crash log. A new crash, native crash or ANR is shown to the agent and saved to the report automatically.
- The agent calls `report_bug` (title, steps, expected, actual, severity) for problems it finds.
- Each bug gets a screenshot and the app's log. The report also lists the recent actions, so a person can follow the same path.

Reports go to `reports/<start time>/`:

```
reports/2026-11-11_10-12-31/
  report.md      one section per bug
  bug-01.png     screenshot
  bug-01.log     crash lines and the app's recent log
```

## Settings

- `ANDROID_SERIAL`: which device to use when more than one is connected.
- `ADB`: path to the adb binary. Otherwise `$ANDROID_HOME/platform-tools/adb`, then `adb` on PATH.
- `PHANTOMQA_REPORTS`: folder for reports. Default: `reports`.

## Limits for now

- `type_text` types printable ASCII only.
- Apps that draw their own UI (games, Flutter without semantics) show few or no elements. Screenshots come in a later step.
- Reading the screen with `uiautomator dump` takes about 1-3 seconds. A faster reader comes in a later step.
