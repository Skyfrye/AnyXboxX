# Graphical launcher: install and use

French version: [fr/lanceur.md](fr/lanceur.md). Code: `crates/anyxboxx-launcher`.

`anyxboxx-launcher.exe` is a window that checks your machine, verifies your game folder, builds or finds VIctory, and starts the game with this line's measured settings (`anyxboxx run --profile diffbase` on NVIDIA). It does not contain or download any game file, and it never writes into the game folder.

## What you need

- Windows 10 or 11, x64, a CPU with AVX2, a Direct3D 12 GPU with shader model 6.6 and mesh shaders (an NVIDIA RTX card is recommended: this line is measured on an RTX 5080), 32 GB of RAM. Full list: [running-gta6.md](running-gta6.md#requirements).
- Your own legally obtained copy of the Xbox Series 1.0.2.1 build: a folder that contains `game_scarlett_master.exe`. Not provided.
- [Git](https://git-scm.com/), [Rust through rustup](https://rustup.rs/), Visual Studio 2022 or 2026 Build Tools with the "Desktop development with C++" workload. The launcher builds VIctory from source with cargo the first time you press PLAY, unless a `victory.exe` sits next to it.
- Free disk space: the VIctory build needs a few GB, and the shader cache grows as you play. Keep more than 30 GB free on the system drive (with 10 GB left, loading never ends).

## Install

```powershell
git clone --recurse-submodules https://github.com/Skyfrye/anyXboxx-diffbase.git
cd anyXboxx-diffbase
cargo build --release -p anyxboxx-launcher
```

Then start `target\release\anyxboxx-launcher.exe`. VIctory is included in `third_party/victory`: there is nothing else to download.

The launcher finds the repository by walking up from its own folder until it meets `third_party\victory\Cargo.toml`. If you keep the executable somewhere else, set `ANYXBOXX_REPO` to the clone folder (`setx ANYXBOXX_REPO C:\path\to\anyXboxx-diffbase`, then open a new window). A `victory.exe` placed next to `anyxboxx-launcher.exe` is used as it is, and then no repository is needed; `--victory-exe <path>` does the same for one elsewhere.

## First start

1. Double-click `anyxboxx-launcher.exe`.
2. **Setup** page: every line must be green or explained. Rust, the Direct3D 12 runtime, the VC++ redistributable and the GPU driver are checked; the Windows SDK `dxc.exe` is only needed for the repository's GPU tests. Links go to official pages only; the launcher installs nothing itself.
3. **Game folder** page: pick the folder that holds `game_scarlett_master.exe` (folder picker, drop on the window, or automatic detection). The quick check reads only that folder's own files.
4. **Home**: press **PLAY**. The first time, a full-window screen shows the stages: building VIctory (several minutes, once), then starting the game.
5. The game starts in its own window and the launcher closes, unless "Keep the launcher open" is on.

The first run after a new build or a new setting translates hundreds of shaders and runs at well under 1 fps for a while: judge the speed on the second run. To translate them before playing instead, turn on "Translate the shaders before playing" on the Play page (it runs `victory shaders` on the game's archives when the cache is stale), or press "Warm now" on the Shaders page.

## Options

- **Display**: the output size (`VICTORY_OUTPUT`). "Screen size", the default, is the largest of the title's own sizes (1280x720, 1664x936, 1920x1080, 2560x1440, 3840x2160) that fits the main screen. "Native render" (`VICTORY_RENDER_SCALE=1.0`, on by default) renders at that size instead of at two thirds of it. "Title's own" leaves both unset: the console's 3840x2160, rendered at two thirds. Window mode: windowed or borderless.
- **Controls**: keyboard layout (`VICTORY_KEYBOARD`), and the in-game keys: click the picture to take the mouse (camera, triggers), F9 to take it or let it go, F3 for the stats overlay.
- **Features**:
  - Stats overlay (`VICTORY_PANEL=1`, on): frame rate, frame time, memory and textures on the picture; F3 shows or hides it.
  - Ray-traced light (`VICTORY_BVH_BUILDER=1`): the title's own BVH builder, interiors lit right, about 14 fps.
  - Natural eyes (`VICTORY_AMBIENT_SPECULAR=1.5`, only with ray tracing off): the ambient stand-in bounds the reflections.
  - Skin confetti fix (`VICTORY_FIRST_UPLOAD_FRONT=1`, experimental).
  - NVIDIA DLSS (`VICTORY_DLSS=1`, NVIDIA only, experimental and not measured in game): your own `nvngx_dlss.dll` or its folder (`VICTORY_DLSS_DLL`, "Browse" picks it), and the mode (`VICTORY_DLSS_MODE`: Quality, Balanced, Performance, Ultra performance, DLAA).
- **Performance**: Fast (the profile itself), Quality (draw-data windows read fresh each frame, fewer copies on the card), 12 GB card (1024 MB of buffers and 512 MB of windows reserved on the card) or 8 GB card (512 MB of buffers, no windows, 85 % of the card's budget for textures). Only Fast is measured. Shader translation before playing, its worker count, automatic Enter/A presses through the start screens (`VICTORY_KEYS`), NVIDIA Smooth Motion.
- **Diagnostics**: VIctory's log level, "Keep the launcher open", the running-game check, and the logs folder.
- **Advanced** panel: overrides of single `VICTORY_*` variables; they always win over the profile and the options.

NVIDIA Smooth Motion (RTX 40 and 50 series) is the driver's own frame generation. It is not a VIctory variable: at each PLAY the launcher writes it into the NVIDIA driver profile that holds `victory.exe` (made if none does), through NVAPI with the setting ids NVIDIA Profile Inspector uses (`0xB0D384C0` enable, `0xB0CC0875` allowed APIs: Direct3D 12). No other driver setting is touched, and no administrator rights are needed.

Settings are profiles per host and GPU vendor (`crates/anyxboxx-launcher/profiles.toml`). Windows + NVIDIA uses this line's measured set (`anyxboxx::run::DIFFBASE`): VIctory's `play.ps1` set without `VICTORY_CARD_AWAKE`, plus the card budgets, RAM relief, placed order and submission-thread switches. Windows + AMD uses VIctory's own `scripts/play.ps1` set, measured on an RX 7900 XTX. Linux (Proton) + AMD adds four measured values (RX 9070 XT) and needs `tools/linux` ([running-gta6.md](running-gta6.md)). Intel and other GPUs get VIctory's defaults.

## Files it writes

| Path | Content |
| --- | --- |
| `logs\game-*.log` next to the launcher | The log of each game run: the command and every VIctory setting, then VIctory's output. When that folder cannot be written: `%LOCALAPPDATA%\AnyXboxX-diffbase\logs` |
| `%LOCALAPPDATA%\AnyXboxX-diffbase\launcher\settings.toml` | Game folder and options |
| `%LOCALAPPDATA%\AnyXboxX\launcher\splash\` | Optional: your own PNG or JPEG pictures, shown as a slideshow instead of the launcher's own |
| `%LOCALAPPDATA%\victory\data\` | VIctory's saves, data and `crashes.log` |
| `third_party\victory\target\release\` | The VIctory build |
| NVIDIA driver settings | With Smooth Motion switched on or off: the Smooth Motion settings of the profile holding `victory.exe` (`AnyXboxX (victory.exe)` when the launcher makes it) |

## Troubleshooting

- **"repository not found"**: the executable is outside the clone, `ANYXBOXX_REPO` is not set, and no `victory.exe` sits next to it.
- **The VIctory build fails**: run `cargo build --release -p victory --manifest-path third_party\victory\Cargo.toml` in a terminal to see cargo's message; usually the MSVC workload is missing.
- **The launcher build fails on `xma`**: the TomBox submodule is missing; run `git submodule update --init`.
- **A game is already running**: the launcher allows one game at a time, because two would share the save folder.
- **The window closed by itself**: the log says `window closed: suspending the title` when you closed it; a crash is written to `crashes.log` under `%LOCALAPPDATA%\victory\data\`.
- **Windows error 1455 in `crashes.log`**: the paging file is too small; close other applications or enlarge it.
- **DLSS does not engage**: the log says why (no `nvngx_dlss.dll` found, a card NGX does not run DLSS on); the title's own upscaler then runs as before.

Under Proton or Wine the launcher cannot build VIctory (no cargo): use `anyxboxx run` as described in [running-gta6.md](running-gta6.md).
