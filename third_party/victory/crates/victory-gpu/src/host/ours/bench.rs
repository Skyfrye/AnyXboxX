//! Device benchmarks: our own passes of a frame of the world, by their
//! counts in a frame line of the RTX 5080's measured run (steady block of
//! 2026-11-26 21:49: 212 sends up of 39.4 MB, 163 hash passes over 650
//! pieces, 43 compares over 11,449 pieces, 6,150 steps done on the card of
//! 2.6 MB, 11 + 10 twin copies at 2560x1440), timed with [`super`]'s pairs;
//! and the card kept awake (`VICTORY_CARD_AWAKE`) beside a load of draws
//! and dispatches. Ignored by default; run them with
//! `cargo test -p victory-gpu --release --lib host::ours::bench -- --ignored --nocapture --test-threads=1`.

use super::*;
use crate::host::{CARD_COMPARE, transition};
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};

/// The test's own queue, fence and list; `run` records, sends and waits.
struct Bench {
    queue: ID3D12CommandQueue,
    fence: ID3D12Fence,
    event: windows::Win32::Foundation::HANDLE,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    sent: u64,
}

impl Bench {
    fn new(host: &Host) -> Bench {
        // SAFETY: plain object creation.
        unsafe {
            let queue: ID3D12CommandQueue = host
                .device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })
                .unwrap();
            let allocator: ID3D12CommandAllocator =
                host.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
            let list: ID3D12GraphicsCommandList =
                host.device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).unwrap();
            list.Close().unwrap();
            Bench {
                queue,
                fence: host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap(),
                event: CreateEventW(None, false, false, None).unwrap(),
                allocator,
                list,
                sent: 0,
            }
        }
    }

    /// Records with `record`, sends, and waits: milliseconds waited.
    fn run(&mut self, record: impl FnOnce(&ID3D12GraphicsCommandList)) -> f64 {
        // SAFETY: the list's last run was waited for; our fence and event.
        unsafe {
            self.allocator.Reset().unwrap();
            self.list.Reset(&self.allocator, None).unwrap();
            record(&self.list);
            self.list.Close().unwrap();
            let started = std::time::Instant::now();
            self.queue.ExecuteCommandLists(&[Some(self.list.cast().unwrap())]);
            self.sent += 1;
            self.queue.Signal(&self.fence, self.sent).unwrap();
            self.fence.SetEventOnCompletion(self.sent, self.event).unwrap();
            WaitForSingleObject(self.event, INFINITE);
            started.elapsed().as_secs_f64() * 1e3
        }
    }
}

fn pipeline(host: &Host, source: &str) -> (ID3D12RootSignature, ID3D12PipelineState) {
    let code = crate::host::compile(source, windows::core::s!("main"), windows::core::s!("cs_5_1")).unwrap();
    // SAFETY: plain object creation; the code lives through the calls.
    unsafe {
        let root: ID3D12RootSignature = host.device.CreateRootSignature(0, &code).unwrap();
        let pso = host
            .device
            .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: code.as_ptr().cast(),
                    BytecodeLength: code.len(),
                },
                ..Default::default()
            })
            .unwrap();
        (root, pso)
    }
}

fn texture(host: &Host, format: DXGI_FORMAT, flags: D3D12_RESOURCE_FLAGS, state: D3D12_RESOURCE_STATES) -> ID3D12Resource {
    let mut made: Option<ID3D12Resource> = None;
    let clear = D3D12_CLEAR_VALUE {
        Format: format,
        Anonymous: D3D12_CLEAR_VALUE_0 {
            DepthStencil: D3D12_DEPTH_STENCIL_VALUE { Depth: 1.0, Stencil: 0 },
        },
    };
    let depth = flags.contains(D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL);
    // SAFETY: plain object creation.
    unsafe {
        host.device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                    Width: 2560,
                    Height: 1440,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    Format: format,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Flags: flags,
                    ..Default::default()
                },
                state,
                depth.then_some(&clear as *const _),
                &mut made,
            )
            .unwrap();
    }
    made.unwrap()
}

/// Of a frame of the world: what our passes cost the card, by kind, as
/// recorded today and in the forms the fixes give them.
#[test]
#[ignore = "benchmark: wants the card (see the module notes)"]
fn bench_our_passes_of_a_frame() {
    let Some(host) = host() else {
        return;
    };
    if clock().is_none() {
        return;
    }
    let mut bench = Bench::new(host);
    const MB: u64 = 1 << 20;
    // Copies in the card's memory: 256 MB of them (more than the card's
    // cache), their hashes, and the compare's results.
    let card = host.uav_buffer(256 * MB).unwrap();
    let hashes = host.uav_buffer(16 * 4096).unwrap();
    let (differs, _) = host.shared_buffer(MB).unwrap();
    let (upload, data) = host.upload_buffer(64 * MB).unwrap();
    // SAFETY: a fresh mapping of 64 MiB.
    unsafe { std::ptr::write_bytes(data, 7, (64 * MB) as usize) };
    let bounce = host.uav_buffer(64 * MB).unwrap();
    let between = host.uav_buffer(4 * MB).unwrap();
    let compare = pipeline(host, CARD_COMPARE);
    let depth = texture(host, DXGI_FORMAT_D32_FLOAT, D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL, D3D12_RESOURCE_STATE_DEPTH_WRITE);
    let colour = texture(host, DXGI_FORMAT_R32_FLOAT, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COMMON);
    let size = (256 * MB) as u32;
    let pass = |list: &ID3D12GraphicsCommandList, first: u32, pieces: u32, mode: u32| {
        let constants = [first, 0, size, pieces, 0, mode];
        // SAFETY: an open list with the compare bound.
        unsafe {
            list.SetComputeRoot32BitConstants(0, 6, constants.as_ptr().cast(), 0);
            list.Dispatch(pieces, 1, 1);
        }
    };
    let bind = |list: &ID3D12GraphicsCommandList| {
        // SAFETY: an open list; live objects.
        unsafe {
            list.SetComputeRootSignature(&compare.0);
            list.SetPipelineState(&compare.1);
            list.SetComputeRootShaderResourceView(1, card.GetGPUVirtualAddress());
            list.SetComputeRootUnorderedAccessView(2, hashes.GetGPUVirtualAddress());
            list.SetComputeRootUnorderedAccessView(3, differs.GetGPUVirtualAddress());
        }
    };
    let uav = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                pResource: std::mem::ManuallyDrop::new(None),
            }),
        },
    };
    // Steps done on the card: 6,150 fills of 424 bytes on average at
    // places of a ring 512 bytes apart.
    const FILLS: u64 = 6150;
    let fills: Vec<((), u64, u64, u64)> = (0..FILLS).map(|i| ((), 32 * MB + i * 512, i * 512, 424)).collect();
    let (count, names) = (7, ["sends from the PC's memory", "sends from the card's", "hashes", "compares", "steps", "steps in runs", "twins"]);
    let mut sums = vec![[0.0f64; 2]; count];
    let frames = 6;
    let _ = take();
    for frame in 0..frames {
        let mut timed: Vec<f64> = Vec::new();
        for kind in 0..count {
            let wall = bench.run(|list| {
                let mut pairs = Pairs::default();
                let mark = pairs.begin(list, Kind::ALL[1 + kind % (KINDS - 1)]);
                // SAFETY: an open list; live buffers, the ranges lie in them.
                unsafe {
                    match kind {
                        0 | 1 => {
                            let from = if kind == 0 { &upload } else { &bounce };
                            for i in 0..212u64 {
                                list.CopyBufferRegion(&card, i * 190_000, from, (i * 190_000) % (40 * MB), 186_000);
                            }
                            list.ResourceBarrier(std::slice::from_ref(&uav));
                        }
                        2 => {
                            bind(list);
                            for i in 0..163u32 {
                                pass(list, (i * 25) % 4000, 4, 0);
                            }
                        }
                        3 => {
                            bind(list);
                            for i in 0..43u32 {
                                pass(list, (i * 266) % (4096 - 266), 266, 1);
                            }
                        }
                        4 => {
                            list.ResourceBarrier(&[transition(&between, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_DEST)]);
                            for &(_, from, to, len) in &fills {
                                list.CopyBufferRegion(&between, to, &card, from, len);
                            }
                            list.ResourceBarrier(&[transition(&between, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON)]);
                        }
                        5 => {
                            list.ResourceBarrier(&[transition(&between, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_DEST)]);
                            for (_, from, to, len) in super::super::card_steps::runs_of(&fills, |_, _| true) {
                                list.CopyBufferRegion(&between, to, &card, from, len);
                            }
                            list.ResourceBarrier(&[transition(&between, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON)]);
                        }
                        _ => {
                            let (src, dst) = (D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COPY_DEST);
                            let dsv = host.dsv(&depth, None);
                            for i in 0..21 {
                                list.ClearDepthStencilView(dsv, D3D12_CLEAR_FLAG_DEPTH, 0.5 + i as f32 / 64.0, 0, None);
                                list.ResourceBarrier(&[transition(&depth, D3D12_RESOURCE_STATE_DEPTH_WRITE, src), transition(&colour, D3D12_RESOURCE_STATE_COMMON, dst)]);
                                list.CopyResource(&colour, &depth);
                                list.ResourceBarrier(&[transition(&depth, src, D3D12_RESOURCE_STATE_DEPTH_WRITE), transition(&colour, dst, D3D12_RESOURCE_STATE_COMMON)]);
                            }
                        }
                    }
                }
                pairs.end(list, mark);
                pairs.resolve(list);
            });
            let sums_now = take().unwrap();
            let card_ms = sums_now[1 + kind % (KINDS - 1)].0;
            if frame > 0 {
                sums[kind][0] += card_ms;
                sums[kind][1] += wall;
            }
            timed.push(card_ms);
        }
        println!("frame {frame}: {timed:.3?}");
    }
    for (kind, [card, wall]) in sums.iter().enumerate() {
        let n = f64::from(frames - 1);
        println!("{:>28}: {:.3} ms on the card, {:.3} ms waited", names[kind], card / n, wall / n);
    }
}

/// Draws and dispatches like a frame of the world's: full-screen triangles
/// with some work per pixel, then passes of a compute shader.
const LOAD: &str = r#"
#define RS "RootConstants(num32BitConstants=1, b0), UAV(u0)"
cbuffer C : register(b0) { uint n; };
RWByteAddressBuffer sum : register(u0);
[RootSignature(RS)]
float4 vs(uint v : SV_VertexID) : SV_Position {
    float2 p = float2((v << 1) & 2, v & 2);
    return float4(p * 2.0 - 1.0, 0.0, 1.0);
}
[RootSignature(RS)]
float4 ps(float4 at : SV_Position) : SV_Target {
    float x = at.x * 0.001 + at.y * 0.0007;
    [loop] for (uint i = 0; i < n; i++) x = frac(x * 1.7 + 0.31);
    return float4(x, x, x, 1.0);
}
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void cs(uint3 id : SV_DispatchThreadID) {
    uint x = id.x;
    [loop] for (uint i = 0; i < n; i++) x = x * 1664525u + 1013904223u;
    sum.Store((id.x & 1023u) * 4u, x);
}
"#;

/// The card kept awake (`VICTORY_CARD_AWAKE=1`) beside a load of draws and
/// dispatches on the drawing queue, against the same load alone: frames of
/// 28 submissions, sent with a gap of the CPU's between them (as the
/// submitting thread leaves) and with none, the two by turns.
#[test]
#[ignore = "benchmark: wants the card (see the module notes)"]
fn bench_card_awake_beside_draws_and_dispatches() {
    let Some(host) = host() else {
        return;
    };
    if clock().is_none() {
        return;
    }
    let mut bench = Bench::new(host);
    let compile = |entry: windows::core::PCSTR, target: windows::core::PCSTR| crate::host::compile(LOAD, entry, target).unwrap();
    let (vs, ps, cs) = (
        compile(windows::core::s!("vs"), windows::core::s!("vs_5_1")),
        compile(windows::core::s!("ps"), windows::core::s!("ps_5_1")),
        compile(windows::core::s!("cs"), windows::core::s!("cs_5_1")),
    );
    let target = {
        let mut made: Option<ID3D12Resource> = None;
        // SAFETY: plain object creation.
        unsafe {
            host.device
                .CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_DEFAULT,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &D3D12_RESOURCE_DESC {
                        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                        Width: 2560,
                        Height: 1440,
                        DepthOrArraySize: 1,
                        MipLevels: 1,
                        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                        Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                        ..Default::default()
                    },
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                    None,
                    &mut made,
                )
                .unwrap();
        }
        made.unwrap()
    };
    let sum = host.uav_buffer(1 << 16).unwrap();
    // SAFETY: plain object creation; the code lives through the calls.
    let (root, draw, dispatch) = unsafe {
        let root: ID3D12RootSignature = host.device.CreateRootSignature(0, &vs).unwrap();
        let mut desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            VS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: vs.as_ptr().cast(),
                BytecodeLength: vs.len(),
            },
            PS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: ps.as_ptr().cast(),
                BytecodeLength: ps.len(),
            },
            SampleMask: u32::MAX,
            RasterizerState: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_NONE,
                DepthClipEnable: true.into(),
                ..Default::default()
            },
            PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            NumRenderTargets: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        desc.BlendState.RenderTarget[0].RenderTargetWriteMask = D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8;
        desc.RTVFormats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
        let draw: ID3D12PipelineState = host.device.CreateGraphicsPipelineState(&desc).unwrap();
        let dispatch: ID3D12PipelineState = host
            .device
            .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: cs.as_ptr().cast(),
                    BytecodeLength: cs.len(),
                },
                ..Default::default()
            })
            .unwrap();
        (root, draw, dispatch)
    };
    let uav = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                pResource: std::mem::ManuallyDrop::new(None),
            }),
        },
    };
    // One submission: twelve draws, a wait, six dispatches, a wait; each
    // pixel and thread `n` steps.
    let record = |list: &ID3D12GraphicsCommandList, n: u32| {
        let rtv = host.rtv(Some(&target), None);
        // SAFETY: an open list; live objects.
        unsafe {
            list.SetGraphicsRootSignature(&root);
            list.SetComputeRootSignature(&root);
            list.SetGraphicsRoot32BitConstant(0, n, 0);
            list.SetGraphicsRootUnorderedAccessView(1, sum.GetGPUVirtualAddress());
            list.SetComputeRoot32BitConstant(0, 80 * n, 0);
            list.SetComputeRootUnorderedAccessView(1, sum.GetGPUVirtualAddress());
            list.SetPipelineState(&draw);
            list.OMSetRenderTargets(1, Some(&rtv), false, None);
            list.RSSetViewports(&[D3D12_VIEWPORT {
                Width: 2560.0,
                Height: 1440.0,
                MaxDepth: 1.0,
                ..Default::default()
            }]);
            list.RSSetScissorRects(&[windows::Win32::Foundation::RECT {
                left: 0,
                top: 0,
                right: 2560,
                bottom: 1440,
            }]);
            list.IASetPrimitiveTopology(windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY(4));
            for _ in 0..12 {
                list.DrawInstanced(3, 1, 0, 0);
            }
            list.ResourceBarrier(std::slice::from_ref(&uav));
            list.SetPipelineState(&dispatch);
            for _ in 0..6 {
                list.Dispatch(256, 1, 1);
            }
            list.ResourceBarrier(std::slice::from_ref(&uav));
        }
    };
    // The 28 lists of a frame, recorded once and sent again each frame: a
    // light frame (some 9 ms of the card's) and one like the world's (some
    // 80 ms).
    let lists = |n: u32| -> Vec<(ID3D12CommandAllocator, ID3D12GraphicsCommandList)> {
        (0..28)
        .map(|_| {
            // SAFETY: plain object creation; the list is recorded and closed.
            unsafe {
                let allocator: ID3D12CommandAllocator =
                    host.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
                let list: ID3D12GraphicsCommandList =
                    host.device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).unwrap();
                record(&list, n);
                list.Close().unwrap();
                (allocator, list)
            }
        })
        .collect()
    };
    let mut frame = |lists: &[(ID3D12CommandAllocator, ID3D12GraphicsCommandList)], gap_us: u64| -> (f64, f64) {
        let _ = take();
        let started = std::time::Instant::now();
        for (_, list) in lists {
            let (front, behind) = whole().unwrap();
            // SAFETY: closed lists; a list is sent again only when the
            // frame before has run (waited below).
            unsafe {
                bench.queue.ExecuteCommandLists(&[Some(front), Some(list.cast().unwrap()), Some(behind)]);
            }
            let until = std::time::Instant::now() + std::time::Duration::from_micros(gap_us);
            while std::time::Instant::now() < until {
                std::hint::spin_loop();
            }
        }
        bench.run(|_| {});
        let wall = started.elapsed().as_secs_f64() * 1e3;
        let card = take().unwrap()[Kind::Submission.index()].0;
        (wall, card)
    };
    let median = |mut v: Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    // The submitting thread leaves the card some 7 ms a frame between 27
    // submissions (run nbc-measure): about 260 us each.
    for (n, gap_us) in [(24, 0u64), (24, 260), (240, 0), (240, 260)] {
        let lists = lists(n);
        // Warm, then by turns: off, on.
        for _ in 0..4 {
            frame(&lists, 0);
        }
        let mut by: [[Vec<f64>; 2]; 2] = Default::default();
        for turn in 0..8 {
            let awake = turn % 2 == 1;
            let kept = if awake { super::super::awake::start(&host.device, || true) } else { None };
            if awake {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            for _ in 0..6 {
                let (wall, card) = frame(&lists, gap_us);
                by[usize::from(awake)][0].push(wall);
                by[usize::from(awake)][1].push(card);
            }
            drop(kept);
        }
        for awake in [false, true] {
            let [wall, card] = by[usize::from(awake)].clone();
            println!(
                "{n} steps a pixel, {gap_us} us between submissions, card kept awake {awake}: frame {:.2} ms (median of {}), the drawing queue's lists {:.2} ms on the card",
                median(wall.clone()),
                wall.len(),
                median(card)
            );
        }
    }
}

/// What the pairs themselves cost the card: a list of 1,000 short passes,
/// each behind a wait as a clamp is, with a pair around each and without.
#[test]
#[ignore = "benchmark: wants the card (see the module notes)"]
fn bench_what_a_thousand_pairs_cost() {
    let Some(host) = host() else {
        return;
    };
    if clock().is_none() {
        return;
    }
    let mut bench = Bench::new(host);
    let (root, pso) = pipeline(host, super::tests::LONG);
    let sum = host.uav_buffer(1 << 16).unwrap();
    let uav = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                pResource: std::mem::ManuallyDrop::new(None),
            }),
        },
    };
    let clock = clock().unwrap();
    let mut by = [Vec::new(), Vec::new(), Vec::new()];
    for turn in 0..30 {
        let (timed, resolved) = (turn % 3 != 0, turn % 3 == 1);
        let wall = bench.run(|list| {
            let mut pairs = Pairs::default();
            // SAFETY: an open list; live objects.
            unsafe {
                list.SetComputeRootSignature(&root);
                list.SetPipelineState(&pso);
                list.SetComputeRootUnorderedAccessView(0, sum.GetGPUVirtualAddress());
                for i in 0..1000u32 {
                    let mark = if resolved { pairs.begin(list, Kind::Clamps) } else { None };
                    if timed && !resolved {
                        list.EndQuery(&clock.heap, D3D12_QUERY_TYPE_TIMESTAMP, 2 * i);
                    }
                    list.ResourceBarrier(std::slice::from_ref(&uav));
                    list.Dispatch(1, 1, 1);
                    list.ResourceBarrier(std::slice::from_ref(&uav));
                    pairs.end(list, mark);
                    if resolved {
                        pairs.resolve(list);
                    }
                    if timed && !resolved {
                        list.EndQuery(&clock.heap, D3D12_QUERY_TYPE_TIMESTAMP, 2 * i + 1);
                    }
                }
                if timed && !resolved {
                    list.ResolveQueryData(&clock.heap, D3D12_QUERY_TYPE_TIMESTAMP, 0, 2000, &clock.buffer, 0);
                }
            }
        });
        let _ = take();
        if turn >= 3 {
            by[usize::from(timed) + usize::from(resolved)].push(wall);
        }
    }
    for (how, walls) in ["no pairs", "pairs resolved once at the end", "pairs resolved each"].iter().zip(by.iter_mut()) {
        walls.sort_by(f64::total_cmp);
        println!("1000 short passes, {how}: {:.3} ms (median)", walls[walls.len() / 2]);
    }
}
