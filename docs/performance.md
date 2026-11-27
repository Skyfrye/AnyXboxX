# What this line changed for CPU, GPU, RAM and VRAM

This page explains every performance change of our line of AnyXboxX and of its engine, VIctory (`third_party/victory`), since the combined base of 26 November 2026. For each change it says which resource it acts on, why it was needed, what it does, the switch that turns it on, and what it measured. It also lists what we tried and left out, and where the time goes now.

## How the numbers were taken

- **Machine.** RTX 5080 (16 GB), Ryzen 7 9800X3D (8 cores, 16 threads), 31 GB of RAM, Windows 11.
- **Scene.** The first cutscene, Series X profile, 2560x1440 native (`VICTORY_OUTPUT=2560x1440`, `VICTORY_RENDER_SCALE=1.0`), unless a line says otherwise.
- **Frame rate.** It comes from the engine's own frame counter (`frame=N` lines). Captures of the window stall a CPU-bound game for about a second each, so they are never used for fps.
- **Frames over 50 ms.** These come from the engine's frame-time line, logged every 5 seconds. A frame over 50 ms is one you can feel as a hitch.
- **Same part of the scene.** Runs reach different shots at a given second, because the automatic button presses land at different moments. So two runs are compared over the same shot, recognised by its number of draws a frame. The prison-cell shot, for example, has about 5,000.
- **Caveats.** Most builds were measured once, on one machine. Figures are given as measured, with their run names, so they can be checked against the logs.

## Where the time goes

The game is **CPU-bound** on this machine:

- **The submitting thread** prepares and sends each frame's work to the card. It takes about 32-40 ms of every frame. The frame cannot be faster than it.
- **List translation** turns the console's command lists into Direct3D 12 calls on 8 threads. It costs about 187 ms of CPU time a frame, about 42,700 calls at about 4.4 µs each.
- **The card** needs about 28-33 ms a frame since the card budgets below. Before them it needed 82.8 ms.
- **The title's own threads** run the game itself and share the same 16 logical processors.

This is why lowering the render resolution (DLSS, or a smaller render scale) gains little here. It saves card time, and the card is not the limit.

## Summary

| Change | Acts on | Effect (measured) | Switch | In our set |
|---|---|---|---|---|
| Card budgets with a reservation | VRAM, GPU | Card work 82.8 → 28.5 ms a frame; first cutscene 9.9 → 20.8 fps | `VICTORY_CARD_RESERVE_BUDGETS=1` and budgets | yes |
| Submitting thread placement | CPU | 20.8 → 22.8 fps over the scene, lows 16.1 → 18.6 | `VICTORY_LANE_CORE`, `VICTORY_HELPERS_ABOVE`, `VICTORY_BACKGROUND_CORES` | yes |
| Submitting thread, round 2 | CPU | 0.2-1.2 ms a frame each, see below | most always on; `VICTORY_PORT_JOURNAL=1` | yes |
| Card steps no longer wait for every flight | CPU, hitches | Refused submissions 0.1-0.2 a frame (about 30 ms each) → 0 | `VICTORY_PLACED_SOURCE=1` | yes |
| Memory line off the submitting thread | CPU, hitches | One 300-460 ms frame every 10 s → none; frames over 50 ms in the cell shot 11.9 % → 5.1 % | always on | yes |
| Preludes on the list's own allocator | RAM | About 1 GB less driver memory | `VICTORY_SHARE_PRELUDE=1` | yes |
| RAM relief under pressure | RAM | Idle copies and pools let go when the PC is short of memory | `VICTORY_RESIDENCY=1` | yes |
| Paired wave64 form on NVIDIA | GPU | Title's card work 42.5 → 38.5 ms in a replayed frame | always on | yes |
| Light tiles in rows on NVIDIA | GPU (correctness) | No lost tiles past 65,535 groups (blocks and stripes of light) | always on | yes |
| Quiet pages for ray tracing | GPU, PCIe | Ray tracing on: about 6 → 14 fps | always on | yes |
| No keep-awake loop on NVIDIA | GPU | The loop halved the drawing queue on NVIDIA | `VICTORY_CARD_AWAKE` ignored on NVIDIA | yes |

## VRAM

### Card budgets with a reservation (`5e58a54c`)

**Problem.** The title keeps its buffers in its own memory. We keep host copies of them on the card, within a budget (`VICTORY_CARD_BUFFERS_MB`, `VICTORY_CARD_WINDOWS_MB`). Textures took the room first, so with larger budgets the buffers and windows still stayed in the PC's memory. Every shader read of them then went over the PCIe bus: 82.8 ms of card work a frame in the first cutscene.

**Fix.** With `VICTORY_CARD_RESERVE_BUDGETS=1`, the texture queue keeps room for the buffers' copies that are not made yet, so an explicit buffer budget is kept in full. Our set uses:

```
VICTORY_CARD_RESERVE_BUDGETS=1 VICTORY_CARD_BUFFERS_MB=1536 VICTORY_CARD_WINDOWS_MB=1024
VICTORY_CARD_BUFFERS_MIN_KB=64 VICTORY_PLACED_ORDER=1
```

The idea comes from Vice-Tay's card budgets (credited in `THIRD_PARTY.md`).

**Measured.** Card work went from 82.8 to 28.5 ms a frame. Over the first cutscene the frame rate went from 9.9 to 20.8 fps, together with RAM relief (below).

### New card memory without a wait (`90701551`)

Memory is made resident when it is made. The copy that tells when the card has it mapped is sent and not waited for. Only a slot that shows memory made a moment ago waits, and only for that copy.

### No keep-awake loop on NVIDIA (`7b87feac`)

`VICTORY_CARD_AWAKE=1` keeps two small dispatches queued so that RDNA3 cards do not idle between submissions. On NVIDIA that loop halved the drawing queue's throughput, so it no longer starts on NVIDIA cards. `VICTORY_CARD_AWAKE=always` still forces it.

### What the panel's VRAM means

The stats panel (F3) shows VRAM used against the budget Windows gives the process: about 14.1-14.9 GB of the 5080's 16 GB. "Shared" is card memory placed in the PC's RAM, which the card reads over the bus. The card budgets above exist to keep the hot data out of it.

## GPU

### Paired wave64 form on NVIDIA (`acdb4601`, `6e338a3a`, `e0a02f27`)

**Problem.** The console runs compute programs in waves of 64 lanes. NVIDIA's waves have 32 lanes. A console wave64 program was split into two host waves ("halves") that meet at every group barrier. Each meeting stalls both halves. Some programs could not be split at all, and their dispatches were dropped.

**Fix.** A "paired" form runs each console wave in one host wave of 32, with each host lane doing the work of two console lanes. The translator counts how often the halves form would meet. It takes the paired form when that saves 32 meetings or more, since below that the paired form is slower (up to twice).

**Measured.** In a replayed cutscene frame, the title's card work went from 42.5 to 38.5 ms. 30 of the title's 280 wave64 compute programs now take the paired form. It is always on.

### Light tiles in rows on NVIDIA (`df4b6be6`)

The title's indirect diffuse lighting dispatches one group per 8x4 tile of the picture: up to about 114,000 groups at 2560x1440. A Direct3D 12 dispatch dimension holds at most 65,535 groups on every card but AMD's, so the tiles past the cut kept old light. That showed as blocks and stripes of light. Such dispatches now run in rows. This is a correctness fix, and it costs no time. `VICTORY_GROUP_ROWS=0` turns it off.

### Ray tracing: quiet pages (`74b1a8fa`)

With the ray-tracing structure builder on, every ray noted every page it could not find. The note was a store to a buffer in the PC's memory, over the bus, for every lane and every frame, even for pages that will never come. Such pages now point to a read-only zero page and are no longer noted. With ray tracing on (`VICTORY_BVH_BUILDER=1`), the first cutscene went from about 6 to about 14 fps.

### Card steps read the constants in runs (`a4f7e6d5`)

Steps the card does for us (`VICTORY_CARD_STEPS`) read the title's ring of constants. Reads that lie close together now go as one copy instead of one copy each.

## CPU: the submitting thread

This thread is the frame's limit, so every millisecond taken off it is a millisecond off the frame.

### Where it runs (`e19b250c`, `bc9f7541`, `35c25704`)

- **`VICTORY_LANE_CORE=1`** gives the submitting thread a core of its own (both its hardware threads), with the process's other threads kept off it. On processors where all cores are alike, such as the 9800X3D, the switch did nothing before.
- **`VICTORY_HELPERS_ABOVE=1`** raises the threads the submitting thread hands work to above normal priority. These are the wide threads (10-14 ms a frame of refreshes on five threads), the threads for the title's bytes under the card's writes, and the heaps' records thread. At normal priority they waited for a turn beside the title's threads and the translators, and the submitting thread waited for them: 2.2-2.3 ms a frame.
- **`VICTORY_BACKGROUND_CORES=1`** keeps the Direct3D runtime's four background threads, which compile shaders for the driver, on the first core. They took 2.6-2.9 cores for minutes of the opening cutscene. They run at idle priority, but an idle thread on a core still takes execution units from the busy thread beside it.

**Measured, the three together.** First cutscene 20.8 → 22.8 fps over the scene, lows (10th percentile) 16.1 → 18.6.

Related, always on:

- **Priority bands (`49858e17`).** Our background work runs below every thread of the title. The title's own threads keep their order, but none goes below "lowest".
- **Pipeline workers sized by physical cores (`9f2a6fc8`).** The workers are counted from the physical cores, less the three our frame waits for. Before, the count was two thirds of the logical processors.
- **No draw waits on a background pipeline save (`090dc8f6`).** The periodic save of the 1.3 GB pipeline library held a lock that workers needed before their draws could use new pipelines.

### Round 2: less work and fewer locks

Each item below is always on, measured by profiling the first cutscene. "ms" is time of the submitting thread per frame.

| Commit | What changed | Measured |
|---|---|---|
| `754f0c3d` | A host list taken for a title's list stays open until the title resets it, instead of being closed and reset again | List taking 1.4 → 0.2 ms |
| `604dc0a5` | The plan of the card's steps made to decide where to cut a submission is reused when the submission takes its steps | Plan 1.5 → 0.7 ms; reads and writes 0.6 → 0.2 ms |
| `4a225873` | "May shaders write this copy?", asked about 4,500 times a frame, is answered from an atomic count without the copy's lock | 4.2 → 3.4 ms |
| `8bcded54` | A refresh's look at a copy takes no lock where nothing is declared | 1.4 → 1.2 ms |
| `ff959b7a` | The line keeps the ranges the parts it joins write, instead of going over all their lists again a dozen times a frame | 0.5 → 0.3 ms |
| `99c7a0ef` | The refreshes in front of a submission are planned on the threads beside it | 3 % of the thread's time moved off it |
| `de86bb67` | The scans of a submission's 20,000 refreshes a frame skip the table lookup when the same copy comes several times in a row | |
| `a030b8a6` | Scratch chunks let go under memory pressure are released on a thread of their own | About 30 ms once a second removed (2.9 % of the thread) |
| `VICTORY_PORT_JOURNAL=1` | Renderer port 7: the heaps' records journal, fewer lock waits | Lock waits cut; in our set |

### Hitches

**Card steps no longer wait for every flight (`711003ba`, `VICTORY_PLACED_SOURCE=1`).** 0.1-0.2 times a frame, the card's steps refused a submission for one reason: a fill of the title's ring of constants behind a copy into the same ring, in the same list. A refused submission waited until no work in flight declared a write over anything it reads. The ring lies in a 174 MB heap that passes of every frame declare written whole, so that meant waiting for about every flight: about 30 ms each time. Such steps now read where the copy reads from. Measured (run rf-on2): refusals → 0 a frame, median 25.7 fps.

**The memory line (`5c361240`).** The memory-by-owner line (`VICTORY_RAM_LINE`, every 10 s) summed the process heap with `HeapSummary`. That walks the heap under its lock for 300-460 ms, and every thread that allocated waited, including the one that sends the frames. So every 10-11 s the card ran dry for a frame of 296-459 ms. It was 20 % of the sending thread's profile samples in the first cutscene, and 23.5 % in the open world. The line no longer sums the process heap (`VICTORY_RAM_HEAP=1` still does), and it is made on a thread of its own. Measured in the prison-cell shot: frames over 50 ms went from 11.9 % of frames (run warm2, line on) to 5.1 % (run cb, line off).

**Reads under the card's writes (`eac23533`, `9c1bc45f`).** A submission that reads title memory the card may be writing now waits only for the card's fence behind the last flight that writes there, not for every flight to be landed. While it waits, the thread that lands flights keeps landing the ones ahead.

## CPU: list translation

These changes reduce the cost of each translated call and the locks the 8 translating threads share. All are always on and give the same results.

| Commit | What changed |
|---|---|
| `d409ca8d` | Page generations tell most writes with no lock and no lookup; the filter has 2^20 buckets instead of 2^16 |
| `f4aa2cb3` | Views' own buffer copies are made outside the lock every lookup takes; idle copies are looked for once a frame, not at every copy made |
| `639960c6` | Those copies are placed in 64 MiB blocks made once, instead of a committed buffer each |
| `0b3c70ba` | No raw twin is made for a list's own buffer views on hosts whose heaps have no twins, as on NVIDIA: 13,386 twins a frame were made for nothing |
| `579499dc` | A slot made again from the same record keeps its resource without a reference taken and given back (0.7 % of the translators' samples), and a thread remembers the placed textures it found |
| `2b248f9e` | The placed-filler bookkeeping, which only fed a diagnostics count, runs only in diagnostics builds |
| `d61477f8` | The shape index of placed views is keyed by a 64-bit hash, not by a second full copy of each shape |

## RAM

### Preludes on the list's own allocator (`299df246`, `VICTORY_SHARE_PRELUDE=1`)

A host list's prelude (work recorded at submission, run with the list) took a command allocator of its own. On the RTX 5080 an allocator with its list takes 0.9 MB of the driver's private memory when made, and gives none of it back when dropped. The prelude now records on the list's own allocator, which is safe because it runs with the list. Measured: about 1 GB less RAM.

### RAM relief under pressure (`VICTORY_RESIDENCY=1`)

When Windows reports the PC is short of memory:

- **Idle texture copies** go after 5 seconds instead of ten minutes, and are made again on their next use (`d048a9ac`).
- **The snapshot pool** empties when idle and keeps only its smallest 32 MiB under pressure; before, its last 512 MiB stayed for the rest of the run (`2ee52d46`). The pool is also kept sorted, so a take is a binary search under its lock, not a scan (`072b4ccc`).
- **A read's fetch memory** above 1 MiB is let go after the fetch; it used to grow to 16 MiB and stay (`9596c3b5`).

### Reading the panel's RAM

The panel's RAM is the process's private bytes: about 25-30 GB in play on a 31 GB PC. About 11.8 GB of it is the driver's backing for card memory that is never touched. It is counted against the system's commit but is mostly not in physical memory ("in memory" on the panel). `VICTORY_RAM_LINE=<seconds>` logs who holds the rest, by owner (`ecb8d841`). `VICTORY_RAM_MAP=<seconds>` walks the address space on its own thread.

## Picture fixes that touch the same paths

These are not speed changes, but they change how textures reach the card, so they belong here:

- **`VICTORY_FIRST_UPLOAD_FRONT=1` (`d63dd5ae`).** A texture's first upload goes in front of the list that samples it. An upload recorded in an earlier list but sent after a copy into the texture wiped what the copy wrote. That showed as coloured "confetti" on skin and tattoos.
- **The decoration bake (`b5a10c24`, `6de0bd3a`, `35009614`).** A bake's pass covers only what was written before it began, on its own heap.
- **Stand-ins (`c028950b`).** A texture slot waiting for its upload shows an opaque stand-in, not zeros.
- **`VICTORY_AMBIENT_SPECULAR=1.5` (`830ec9d9`).** With ray tracing off, reflections bounded by the light around them, so eyes are not lit white.
- **Pipeline libraries (`53389734`).** A host pipeline library that another setup rejects is kept beside the new one, so neither setup starts with every pipeline to make: 0.1-1.5 fps for two minutes otherwise.
- **Loops' iteration limit (`6b383feb`, `ce677898`, `VICTORY_LOOP_ONE_EXIT`, on).** The translator stops every loop after a number of iterations so that bad data cannot hang the card. A lane stopped at an unconditional back edge used to jump straight to a return, which gave the loop a second way out. The block after the loop then no longer post-dominated it, so the driver's compiler could not bring the lanes back together there, and the wave operations after the loop (ballots, first-lane reads, the exchanges of a wave64 program run as two 32-lane waves) saw only some of the lanes. The open world's water lost whole 8x4-tile groups of its transmitted light (dark blocks on the docks' water): its tile list is appended to with one atomic per console wave, and the two halves wrote over each other. Now both ways out of such a loop go to one new block, which sends a stopped lane on to the return and the others to the exit. A first form that let stopped lanes run on with undefined values lit the opening's hair orange and was replaced. About a fifth of the programs translate differently, so `TRANSLATOR_VERSION` is 4 and the first start after updating translates and builds them again. To do that ahead of play:
  - `victory translate --warm all --jobs 16` with `VICTORY_HOST_WAVE=32` and `VICTORY_VIEW_LIMIT` set as the run's log says (999999 for a 1,000,000-view heap; about 90 s);
  - one run with `VICTORY_PIPELINE_BOOK=1` and `VICTORY_PIPELINE_WORKERS=12`;
  - then `victory warm --used`.

## Tried and left out

| Switch or idea | Result | Why it is out |
|---|---|---|
| `VICTORY_MOVIE_PLANES=1` | Fixes the intro video's colour-only frames | It takes every linear R8 texture for a movie plane and copies it on the submitting thread: 2-3 ms more a frame, and 35-177 ms stalls at the first cutscene's cuts. Back once it copies only planes that changed. |
| `VICTORY_COPIED_AT_ONCE=1`, `VICTORY_TABLED_ORPHANS=1` | Textures picked by index shown about twice as soon (199 → 107 ms) | Faces turned to black masks in 2 runs of 6. The likely cause: a texture's first upload on the copy queue can land after the bake wrote it. |
| `VICTORY_KEPT_FIRST=1` | Holds back uploads into a host copy another slot still shows | Textures became later (329 ms mean against 199) |
| `VICTORY_OWN_HEAP=1` | A heap of our own for the translators | No gain: translation time unchanged. The profiler had overstated the heap lock. |
| `VICTORY_D3D12_BACKGROUND=0` | Runtime background threads off | No gain |
| `VICTORY_WINDOW_KB=1024`, `VICTORY_WINDOW_TAILS=1` | 1.3 GB less RAM | More hitches |
| `VICTORY_FRAME_INTERVAL_MS=40` | The game paced at 25 fps | Frames no steadier: the uneven frames are the ones that take longer than 40 ms on the CPU |
| `VICTORY_CARD_AWAKE=1` on NVIDIA | Keeps the card busy between submissions | Halved the drawing queue on NVIDIA |
| DLSS (`VICTORY_DLSS=1`) | Works, experimental | Saves card time, which is not the limit here; small gain at best |

## What is next

- **Translation.** Indirect executions are 60 % of translation: about 5,000 a frame at 22 µs each, 20 of which are binding tables. Most tables are made afresh in every list that binds them (about 9,400 a frame), so the next design change is to share a made table across lists.
- **Scene cuts.** The first cutscene's cuts stall for 240-570 ms. Part of that was `VICTORY_MOVIE_PLANES` (now out). The rest is translation waiting for pipelines that are in the early list and already started, but not finished in time.
- **One-frame texture swaps.** Two title textures at one address with one description share a host copy, so a slot kept on one can show the other's bytes for a frame. The fix is to stop sharing the copy.
- **Water.** Dark rectangular blocks on the water in the open world. They show the same way every time in the establishing shot when the docks save loads.

## Switches for measuring

| Switch | What it gives |
|---|---|
| `VICTORY_PANEL=1` (F3) | The stats panel, in release builds too |
| (always on) | A line every 5 s of frame times, with those over 50 and 100 ms |
| `VICTORY_FRAME_BOOK=<ms>` | A line for each frame slower than `<ms>`, with what grew in it |
| `VICTORY_THIN_TIMES=1` | Translation time by call kind and by part of a bind |
| `VICTORY_PASS_TIMES=1` | The title's card work timed by pipeline |
| `VICTORY_RAM_LINE=<s>`, `VICTORY_RAM_MAP=<s>` | Memory by owner; a census of the address space |
| `VICTORY_PROFILE` | Stack sampling, in release builds too |
