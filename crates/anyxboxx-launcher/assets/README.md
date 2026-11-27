# Launcher art

The launcher's own pictures, made for this project on 27 November 2026 with local image models. They contain no art, logo, character, place or trademark from any game. They are embedded in `anyxboxx-launcher.exe` (`src/art.rs`) and are covered by the repository's licence (GPL-3.0-or-later).

| File | Use | Made with | Prompt |
|---|---|---|---|
| `hero-1.jpg` | Home hero and launch screen, first slide (1536x864) | asset-forge, `illustration` profile, seed 7 | Cinematic abstract key art for a software launcher: a translucent glass microchip floating above a dark violet landscape, streams of magenta and warm amber light flowing into it like rivers of data, soft volumetric haze, deep night purple sky, orange glow on the far horizon, wide composition with calm dark empty space on the left third, high detail, no text, no letters, no logos, no people, no vehicles, no buildings |
| `hero-2.jpg` | Second slide (1536x864) | asset-forge, `illustration` profile, seed 11 | Cinematic abstract key art for a software launcher: glowing circuit-board traces etched into a dark violet plain, converging into a single bright path that runs toward a warm amber dawn on the horizon, magenta light bloom along the traces, faint floating particles, deep purple night sky, low camera angle, wide composition with calm dark empty space on the left third, high detail, no text, no letters, no logos, no people, no vehicles, no buildings |
| `icon.png`, `icon.ico` | Window and taskbar icon; the executable's icon (16 to 256 px) | sprite-forge, `icon` profile (FLUX.2 klein 4B, background removed), seed 5 | a translucent violet glass octagonal microchip with short pins around its edge, in its centre a glowing warm amber media play button: a solid triangle pointing to the right, magenta rim light, bold simple silhouette |

The hero pictures leave the left third dark, where the Home page draws its text over a shade. To use your own pictures instead, put PNG or JPEG files in `%LOCALAPPDATA%\AnyXboxX\launcher\splash\`, or point `ANYXBOXX_LAUNCHER_SPLASH` at a file or a folder: the launcher shows them as a slideshow and leaves these out.
