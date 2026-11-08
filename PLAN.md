# Nanoleaf Sync: Implementation Plan

As of 2026-11-08.

Build a small Windows tray app that captures PC audio, turns it into per-panel colors, and streams them straight to the Shapes over Nanoleaf's documented local API. It needs no cloud service, account, or Nanoleaf app.

The local API is confirmed reachable on your panels. Streaming on firmware 12.4.1 has not been tested yet, so Task 2 is a go/no-go gate.

## 1. Scope

**v1 includes**

- System audio capture from the default Windows output device.
- Four effect modes: Spectrum, Bass Pulse, Beat Hop, Ripple.
- Editable color palettes, plus sensitivity, smoothing, brightness and update-rate controls.
- Saved presets.
- System tray with close-to-tray and an optional start-with-Windows setting.
- Discovery by mDNS, manual IP fallback, and a guided pairing screen.
- Restoring the panels' previous effect when sync stops or the app quits.

**Not in v1:** microphone or line-in input, user-drawn panel zones, multiple controllers, screen mirroring, macOS or Linux, code signing.

Effects still use the real panel positions. The app reads the x/y layout from the device and maps panels automatically, with no layout editor.

## 2. How we talk to the panels

**Your device**

| Field | Value |
|---|---|
| Name | Shapes 4278 |
| IP | 192.168.2.104 (may change; rediscover by device ID) |
| Model | NL42 |
| Firmware | 12.4.1 |
| Device ID | AE:23:F8:81:57:74 |

**Protocol facts**

| Topic | Detail | Evidence |
|---|---|---|
| Discovery | mDNS `_nanoleafapi._tcp`; SSDP `ST: nanoleaf:nl42` | Probed: both answered from this PC |
| REST base | `http://<ip>:16021/api/v1/<token>/...` | Probed: returns 401 with no token, as expected |
| Pairing | Hold the power button 5-7 s until the LED flashes, then `POST /api/v1/new` within 30 s. Returns 200 `{"auth_token": ...}`; errors 401, 403, 422 | Official docs |
| Layout | `GET .../panelLayout/layout` returns `positionData` with `panelId`, `x`, `y`, `o`, `shapeType`. Infer side length from `shapeType`; the `sideLength` field is deprecated | Official docs; endpoint path to confirm in Task 2 |
| Shape types | 7 Hexagon, 8 Triangle, 9 Mini Triangle, 12 Shapes Controller | Official docs |
| Controllers | Exclude `shapeType` 12 from color frames. The docs list no side length for it; confirm in Task 2 that it has no light | Inference |
| Start streaming | `PUT .../effects` with `{"write":{"command":"display","animType":"extControl","extControlVersion":"v2"}}`. Shapes return no body | Official docs |
| Stream target | UDP to `<ip>:60222` | Official docs; two open-source clients agree |
| Rate limit | Docs: "must not stream data at a rate higher than 10Hz". One client library reports about 50 ms (20 Hz) as the floor on Shapes | Official docs; community |
| Transition | Units of 100 ms. The docs say values above 1 fade over that period; behavior at 0 and 1 is to be measured | Official docs; Task 2 |
| Restore | Read the current effect with `GET .../effects/select`, restore with `PUT .../effects` and `{"select":"<name>"}` | From memory: confirm in Task 2 |
| Leaving stream mode | The docs do not describe a timeout or exit. Selecting an effect is expected to exit | Unverified: Task 2 |

**v2 UDP frame.** Multi-byte fields are big-endian. Size is `2 + 8 * nPanels` bytes.

```text
nPanels         u16
repeat nPanels times:
  panelId       u16
  R, G, B, W    u8 each (W is always 0)
  transition    u16 (units of 100 ms)
```

Example for panel 374, magenta, 1.2 s fade, as the panel's bytes: `1 118 255 0 255 0 0 12`.

UDP has no acknowledgement. Code can confirm a packet was sent, but only a person watching the panels can confirm it worked.

## 3. Decisions

| Decision | Choice | Source |
|---|---|---|
| Audio source | System audio through WASAPI loopback | Kyler |
| Stack | Tauri 2 with a Rust core | Kyler |
| v1 extras | Saved presets; tray and start with Windows | Kyler |
| Update rate | Test up to 20 Hz, default to fastest stable, user slider to lower it | Kyler |
| Frontend | React, TypeScript, Vite | Default; easy to change before Task 1 |
| Distribution | Windows only, personal use, unsigned NSIS installer | Default; signing is needed before sharing widely |
| Token storage | Plain JSON under `%APPDATA%\NanoleafSync`, never in the repo. It only grants LAN light control | Default |

## 4. Architecture

```text
WASAPI loopback thread
   | mono f32 samples (lock-free ring buffer)
   v
Analysis thread: FFT, bands, onset, beat counter
   | latest AudioFeatures (shared slot)
   v
Engine tick at N Hz: Effect::render -> Frame -> LightSink::send
   |                                               |
   | meter events to UI (max 30/s)                 v
   v                                          UDP :60222 to panels
Tauri IPC <-> React UI                        REST :16021 for setup and restore
```

Rendering is separate from analysis so a beat between ticks is never lost. Effects compare `beats_total` with the last value they saw, rather than reading a one-tick flag.

**Repository layout**

```text
Cargo.toml                 workspace
crates/core/               nanoleaf-core: no Tauri dependency
  src/types.rs             shared contracts (below)
  src/nanoleaf/            discovery, REST client, UDP sink
  src/audio/               WASAPI loopback
  src/analysis/            FFT, bands, beat detection
  src/effects/             the four effects
  src/engine/              real-time loop and lifecycle
  src/config/              settings and presets
  examples/                nl_probe, audio_meter, sync_cli
src-tauri/                 desktop shell, IPC, tray
src/                       React UI
docs/hardware-notes.md     measured device behavior
```

**Shared contracts.** Task 1 creates these in `crates/core/src/types.rs`. Parallel tasks code against them and do not change them without flagging it in their report.

```rust
pub type PanelId = u16;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rgb { pub r: u8, pub g: u8, pub b: u8 }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Panel { pub id: PanelId, pub x: i32, pub y: i32, pub o: i32, pub shape_type: u8 }

/// Light-emitting panels only; controllers are already removed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Layout { pub panels: Vec<Panel>, pub global_orientation: i32 }

#[derive(Clone, Debug)]
pub struct Frame {
    pub colors: Vec<(PanelId, Rgb)>,
    /// Units of 100 ms, matching the wire format.
    pub transition: u16,
}

pub const BAND_COUNT: usize = 16;

#[derive(Clone, Debug, Default, Serialize)]
pub struct AudioFeatures {
    pub rms: f32,                     // 0.0..=1.0 after gain control
    pub bands: [f32; BAND_COUNT],     // low to high, 0.0..=1.0
    pub bass: f32,
    pub mid: f32,
    pub treble: f32,
    pub onset: f32,
    pub beats_total: u64,             // monotonic
    pub captured_at_micros: u64,      // for latency measurement
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EffectParams {
    pub palette: Vec<Rgb>,            // 2 to 8 stops
    pub background: Rgb,
    pub sensitivity: f32,             // 0.1..=3.0
    pub smoothing: f32,               // 0.0..=1.0
    pub brightness: f32,              // 0.0..=1.0
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Preset { pub name: String, pub effect_id: String, pub params: EffectParams }

pub trait Effect: Send {
    fn id(&self) -> &'static str;
    fn render(&mut self, audio: &AudioFeatures, layout: &Layout,
              params: &EffectParams, dt_secs: f32) -> Frame;
}

pub trait LightSink: Send {
    fn send(&mut self, frame: &Frame) -> Result<(), SinkError>;
}
```

## 5. Implementation tasks

| Task | Name | Depends on | Can run alongside | Needs Kyler |
|---|---|---|---|---|
| 1 | Scaffold and contracts | None | None | No |
| 2 | Device client and hardware gate | 1 | 3, 4, 5, 7 | Yes |
| 3 | Audio capture | 1 | 2, 4, 5, 7 | No |
| 4 | Audio analysis | 1 | 2, 3, 5, 7 | No |
| 5 | Effects | 1 | 2, 3, 4, 7 | No |
| 6 | Engine and lifecycle | 2, 3, 4, 5 | 7 | Brief check |
| 7 | Settings and presets | 1 | 2, 3, 4, 5, 6 | No |
| 8 | Desktop app and UI | 6, 7 | None | Brief check |
| 9 | Tray and autostart | 8 | None | Brief check |
| 10 | Tuning, soak test, installer | 9 | None | Yes |

If Task 2 fails its gate, pause Tasks 6 onward and revisit the plan.

**Prompt for each agent**

```text
Read PLAN.md sections 2 to 4, then implement only Task N from section 5.
Stay within the files that task owns. Do not change the shared contracts
without flagging it in your report. Run every check in "Done when" and report
actual results, including failures and anything you could not run.
Never commit the auth token or anything from %APPDATA%\NanoleafSync.
```

Task 1 makes the initial commit, because parallel agents in git worktrees need one. Later tasks commit only if your dispatch prompt asks them to.

### Task 1: Scaffold and contracts

- **Goal:** A building workspace that every other task can drop code into.
- **Owns:** All root config, `crates/core/src/lib.rs`, `crates/core/src/types.rs`, module stubs, the initial `src-tauri/` and `src/`.
- **Build:**
    - Run `git init`, add a `.gitignore`, and create the layout from section 4.
    - Create the Tauri 2 React TypeScript app. The Tauri CLI is not installed, so add it as an npm dev dependency.
    - Implement section 4's contracts, plus a `MockSink` that records frames.
    - Add every expected dependency now so parallel tasks do not collide in `Cargo.toml`: `tauri`, `wasapi`, `realfft`, `rtrb`, `mdns-sd`, `ureq` or `reqwest`, `serde`, `serde_json`, `thiserror`, `tracing`.
    - Add empty module files for every Task 2 to 7 area, each already declared in `lib.rs`.
- **Done when:** `cargo test --workspace` passes, `cargo clippy --workspace` is clean, `npm run tauri dev` opens an empty window, and the initial commit exists.

### Task 2: Device client and hardware gate

- **Goal:** Prove streaming works on firmware 12.4.1 and measure its limits.
- **Owns:** `crates/core/src/nanoleaf/**`, `crates/core/examples/nl_probe.rs`, `docs/hardware-notes.md`.
- **Build:**
    - mDNS discovery with SSDP fallback; connect by manual IP; resolve a stored device ID to its current IP.
    - Pairing that retries `POST /new` for up to 60 s. Store the token in `%APPDATA%\NanoleafSync\devices.json`, keyed by device ID, so later tasks reuse it.
    - REST calls for layout, on/off state, brightness, current effect, select effect, and enable extControl.
    - `UdpSink` implementing `LightSink`, with a v2 encoder covered by byte-exact unit tests including the docs example.
    - `nl_probe` subcommands: `discover`, `pair`, `layout`, `stream --hz H --seconds S --transition T`, `restore`. The stream pattern is one lit panel chasing across the layout, so stalls are easy to see.
- **Measure and record in `docs/hardware-notes.md`:**
    - Fastest stable rate among 5, 10, 15 and 20 Hz, using Kyler's visual report.
    - Visible behavior at transition 0, 1 and 2.
    - What happens if UDP stops for 5 s and 60 s, and whether extControl must be re-sent.
    - Whether selecting an effect exits stream mode and restores cleanly, and whether panels must be on first.
    - Whether `shapeType` 12 emits any light, and the confirmed restore and layout endpoints.
- **Done when:** Unit tests pass, the chase pattern ran visibly on the real panels, the previous effect came back after `restore`, and the notes are written.
- **Gate:** If panels do not respond to UDP after extControl, stop and report what was tried.

### Task 3: Audio capture

- **Goal:** A clean mono sample stream from the default output device.
- **Owns:** `crates/core/src/audio/**`, `crates/core/examples/audio_meter.rs`.
- **Build:**
    - Event-driven WASAPI loopback on its own thread, using the device mix format.
    - Downmix to mono f32 and push through an `rtrb` ring buffer with no allocation or locks on the capture path.
    - Loopback delivers no packets while nothing is playing, so a wait timeout must count as silence and must not be an error.
    - Recover when the default output device changes or disconnects.
    - Clean start and stop.
- **Done when:** `audio_meter` shows RMS above a stated threshold while a test sound plays and near zero when it stops, capture survives 60 s of silence, and it recovers from a default device switch. Report that last check as skipped if only one output device is available.

### Task 4: Audio analysis

- **Goal:** Turn samples into `AudioFeatures`. This is pure code with no devices.
- **Owns:** `crates/core/src/analysis/**`.
- **Build:**
    - Starting values, to be tuned in Task 10: 2048-sample Hann FFT, 512-sample hop, 16 log-spaced bands from 40 Hz to 16 kHz, bass under 150 Hz, mid 150 Hz to 2 kHz, treble above 2 kHz.
    - Slow automatic gain so quiet and loud tracks both use the full range, without pumping up silence.
    - Spectral-flux onset with an adaptive threshold and a refractory period. Increment `beats_total` on each beat.
    - No allocation per hop after setup.
- **Done when:** Tests with synthetic audio pass:
    - A 120 BPM click track detects at least 95% of clicks, each within 25 ms.
    - A steady sine and silence produce no beats.
    - A 100 Hz sine peaks in the bass bands, and an 8 kHz sine in the treble bands.
    - Output stays within documented ranges at 44.1 and 48 kHz.

### Task 5: Effects

- **Goal:** Four effects implementing `Effect`, tested with no hardware.
- **Owns:** `crates/core/src/effects/**`.
- **Build:**
    - **Spectrum:** Sort panels by rotated x position and map them low to high across the bands.
    - **Bass Pulse:** All panels pulse with `bass`, color sampled from the palette.
    - **Beat Hop:** Each new beat advances palette colors across the panels.
    - **Ripple:** Each beat starts a ring moving outward from the layout's center.
    - Shared helpers for gradient sampling, smoothing and brightness. Use a seeded generator for any randomness so tests are deterministic.
    - An `effects::registry()` that lists IDs and constructs effects.
- **Done when:** Tests prove every effect returns exactly one color per panel for 1, 9 and 30 panels. Also test that brightness 0 gives black, that output is stable between identical inputs, that Beat Hop changes only when `beats_total` does, and that Spectrum ordering follows x position.

### Task 6: Engine and lifecycle

- **Goal:** Connect the pipeline and always leave the panels in a good state.
- **Owns:** `crates/core/src/engine/**`, `crates/core/examples/sync_cli.rs`.
- **Build:**
    - Analysis thread and an engine tick at the configured rate. Allow 1 to 20 Hz, defaulting to the Task 2 result.
    - Start sequence: record current effect and on/off state, enable extControl, begin ticks.
    - Stop sequence: end ticks, restore recorded effect and state. Run it on normal exit, Ctrl+C, and panic hook where possible.
    - Live changes to effect and params without restarting. Publish a status: stopped, starting, running, reconnecting, error.
    - Re-establish extControl and rediscover IP after device loss, as `docs/hardware-notes.md` indicates.
    - Record audio-to-send latency using `captured_at_micros`.
- **Done when:**
    - Tests with `MockSink` cover tick rate, start/stop order, restore on error, and live parameter changes.
    - `sync_cli` runs 5 minutes on the real panels with music and restores the previous effect at the end.
    - Report p50 and p95 latency against a target of p95 under 100 ms, and CPU use.
    - Kyler confirms the panels visibly follow the music.

### Task 7: Settings and presets

- **Goal:** Reliable persistence for settings, devices and presets.
- **Owns:** `crates/core/src/config/**`.
- **Build:**
    - Versioned JSON under `%APPDATA%\NanoleafSync`, with a path override for tests.
    - Atomic writes using a temp file and rename.
    - Keep the `devices.json` format from Task 2's contract: a map from device ID to token and last IP.
    - A corrupt file is moved aside and defaults are loaded, never a crash.
    - Preset create, rename, delete, list and duplicate-name handling.
    - Ship 4 to 6 built-in presets across the four effects. They are read-only and cannot be deleted.
- **Done when:** Tests cover save/load round trips, corrupt files, missing directories, schema version handling, and built-in preset protection.

### Task 8: Desktop app and UI

- **Goal:** A usable app over the engine.
- **Owns:** `src-tauri/src/**` except `tray.rs`, and `src/**`.
- **Build:**
    - Typed Tauri commands for discovery, pairing, start, stop, effect and param updates, and presets. Emit status and meter events capped at 30 per second.
    - Setup screen: discovered devices, manual IP entry, and a pairing step with instructions and a visible 30 s countdown.
    - Main screen: Start/Stop, effect picker, palette editor, sensitivity, smoothing, brightness and update-rate sliders, live band meter, connection status.
    - Preset save, load, rename and delete.
    - Clear error states for device unreachable, pairing timeout, and no audio device.
- **Done when:** `npm run tauri dev` runs through first-time setup, start, live slider changes, preset save and reload, and stop on the real panels, with the previous effect restored. Frontend type check and Rust tests pass. Kyler confirms the slider changes are visible on the panels.

### Task 9: Tray and autostart

- **Goal:** The app can live in the background.
- **Owns:** `src-tauri/src/tray.rs`, Tauri capability and plugin config.
- **Build:**
    - Tray icon with menu: Show, Start/Stop Sync, preset submenu, Quit.
    - Closing the window hides to tray. Quit stops sync and restores the panels.
    - A start-with-Windows setting off by default, using the Tauri autostart plugin, launching minimized.
    - A single-instance guard.
- **Done when:** Each menu item works, the panels restore on Quit, autostart toggles the actual Windows startup entry on and off, and a second launch focuses the first. Kyler confirms after a real sign-out and sign-in.

### Task 10: Tuning, soak test, installer

- **Goal:** It feels good, stays stable, and installs cleanly.
- **Owns:** `README.md`, tuning constants, bundle config.
- **Build:**
    - Tune analysis and effect defaults with Kyler across at least three genres.
    - Run a 2-hour soak with music, including sleep and resume, device power cycle, and Wi-Fi loss if practical. Track memory and CPU.
    - Build the NSIS installer with `npm run tauri build` and test install and uninstall.
    - Write a README covering setup, pairing, troubleshooting, and the SmartScreen warning for unsigned builds.
- **Done when:** The soak shows no crash or sustained memory growth, the installer works on this PC, and Kyler signs off on how it looks. Report any soak scenario that was not run.

## 6. What I need from you

1. **Leave the panels on** and connected during hardware tasks. A DHCP reservation for 192.168.2.104 is helpful but optional.
2. **Task 2, one button press.** When the agent says it is ready, hold the power button on the controller for 5-7 seconds until the LED flashes. The token is saved, so later tasks should not need this again.
3. **Task 2, watch the panels.** You'll report whether the chase pattern stays smooth at each rate. Budget about 10 minutes.
4. **Close the Nanoleaf desktop and mobile apps** during hardware tests so they do not compete for control.
5. **Tasks 6, 8, 9: quick visual checks.** A few minutes each, with music playing.
6. **Task 10: listen and judge.** The feel of the effects is your call, so this is the longest session.

## 7. Risks

| Risk | Effect | Response |
|---|---|---|
| extControl changed on firmware 12.4.1 | Direct streaming fails | Task 2 gate runs before dependent work |
| 20 Hz is above the documented 10 Hz | Stutter or stalls | Measure in Task 2, default to fastest stable, slider to lower |
| Nanoleaf changes the local API in later firmware | App stops working after an update | Keep protocol code isolated in `nanoleaf/` and record tested firmware |
| App exits without restoring | Panels stay on the last streamed frame | Stop sequence, panic hook, and a restore action at next launch |
| Windows Firewall blocks discovery for the new app | Device not found | Manual IP entry; note in README |
| Lighting changes can affect photosensitive viewers | Strobing from Beat Hop or Bass Pulse | Smoothing defaults avoid hard full-panel flashes; document in README |

## Sources

- [Nanoleaf Light Panels Open API Documentation](https://nanoleaf.atlassian.net/wiki/spaces/nlapid/pages/2789310530) (official)
- [Nanoleaf API documentation overview](https://nanoleaf.atlassian.net/wiki/spaces/nlapid/overview)
- [List of services used by Nanoleaf devices](https://nanoleaf.atlassian.net/wiki/spaces/nlapid/pages/3410591752)
- [rowak/nanoleaf-api `NanoleafDevice.java`](https://raw.githubusercontent.com/rowak/nanoleaf-api/master/src/main/java/io/github/rowak/nanoleafapi/NanoleafDevice.java)
- [MylesMor/nanoleafapi `nanoleaf.py`](https://raw.githubusercontent.com/MylesMor/nanoleafapi/master/nanoleafapi/nanoleaf.py)
- [leafpipe write-up](https://half-shot.uk/blog/leafpipe/)
- [`wasapi` Rust crate](https://docs.rs/wasapi)
