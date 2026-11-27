//! DLSS on the card, on frames of ours. Ignored by default: it needs an
//! NVIDIA RTX card, its driver's NGX runtime and a DLSS model of the user's:
//!
//! ```text
//! VICTORY_DLSS_DLL=<folder or nvngx_dlss.dll> cargo test -p victory-gpu --lib dlss -- --ignored
//! ```
//!
//! Without the model it says why and passes; with it, NGX must start, make
//! the feature and upscale.

use super::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

/// The colour of every rendered pixel.
const COLOUR: [f32; 4] = [0.5, 0.25, 0.125, 1.0];

fn texture(
    device: &ID3D12Device,
    (w, h): (u32, u32),
    format: DXGI_FORMAT,
    flags: D3D12_RESOURCE_FLAGS,
    state: D3D12_RESOURCE_STATES,
) -> ID3D12Resource {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: u64::from(w),
        Height: h,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Flags: flags,
        ..Default::default()
    };
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: plain creation into our out value.
    unsafe {
        device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut made)
    }
    .unwrap();
    made.unwrap()
}

/// Half floats to floats (normal numbers and zero: what DLSS writes here).
fn half(bits: u16) -> f32 {
    let (sign, exp, man) = (
        (bits >> 15) as u32,
        ((bits >> 10) & 0x1f) as u32,
        (bits & 0x3ff) as u32,
    );
    if exp == 0 {
        let sign = if sign == 1 { -1.0 } else { 1.0 };
        return sign * man as f32 * 2f32.powi(-24);
    }
    f32::from_bits(sign << 31 | (exp + 112) << 23 | man << 13)
}

#[test]
#[ignore = "needs an NVIDIA RTX card, NGX from its driver and VICTORY_DLSS_DLL"]
fn dlss_upscales_synthetic_frames_on_the_card() {
    let model = std::env::var_os("VICTORY_DLSS_DLL").map(PathBuf::from);
    let setup = match check(Some("1"), Some("quality"), model.as_deref(), ngx::core_path) {
        Readiness::Ready(setup) => setup,
        other => {
            eprintln!("DLSS device test skipped: {other:?}");
            return;
        }
    };
    let host = crate::host::host().expect("a host GPU");
    let ngx = ngx_with(&setup).expect("NGX starts on the host device and runs DLSS");
    let (display, mode) = ((1920, 1080), Mode::Quality);
    let render = render_size(display, mode.scale());
    let flags = Flags {
        hdr: true,
        depth_inverted: true,
        auto_exposure: true,
        ..Flags::default()
    };
    let feature = Feature::made(
        ngx,
        Create {
            render,
            display,
            flags,
            mode,
        },
    )
    .expect("the DLSS feature is made");
    eprintln!(
        "DLSS feature made: {render:?} to {display:?}, DLSS's own render size {:?}",
        feature.optimal
    );
    assert!(
        feature
            .optimal
            .is_some_and(|(w, h)| w > 0 && h > 0 && w <= display.0 && h <= display.1)
    );
    // Our scales give DLSS's own render sizes at the user's 2560x1440.
    for mode in [
        Mode::Quality,
        Mode::Balanced,
        Mode::Performance,
        Mode::UltraPerformance,
        Mode::Dlaa,
    ] {
        let ours = render_size((2560, 1440), mode.scale());
        let dlss = optimal_size(ngx, (2560, 1440), mode).expect("DLSS's optimal settings");
        eprintln!("{}: ours {ours:?}, DLSS's {dlss:?}", mode.name());
        assert_eq!(ours, dlss, "{}", mode.name());
    }

    let d = &host.device;
    let target = D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET;
    let rt = D3D12_RESOURCE_STATE_RENDER_TARGET;
    let colour = texture(d, render, DXGI_FORMAT_R16G16B16A16_FLOAT, target, rt);
    let depth = texture(d, render, DXGI_FORMAT_R32_FLOAT, target, rt);
    let motion = texture(d, render, DXGI_FORMAT_R16G16_FLOAT, target, rt);
    let output = texture(
        d,
        display,
        DXGI_FORMAT_R16G16B16A16_FLOAT,
        D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
    );
    // SAFETY: a plain heap of three render target views.
    let heap: ID3D12DescriptorHeap = unsafe {
        d.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
            Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
            NumDescriptors: 3,
            ..Default::default()
        })
    }
    .unwrap();
    // SAFETY: plain queries.
    let (start, step) = unsafe {
        (
            heap.GetCPUDescriptorHandleForHeapStart(),
            d.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV),
        )
    };
    let views: Vec<_> = [(&colour, COLOUR), (&depth, [0.5; 4]), (&motion, [0.0; 4])]
        .into_iter()
        .enumerate()
        .map(|(i, (t, clear))| {
            let at = D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: start.ptr + i * step as usize,
            };
            // SAFETY: a live texture and a slot of our heap.
            unsafe { d.CreateRenderTargetView(t, None, at) };
            (t, at, clear)
        })
        .collect();
    let read =
        D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE | D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
    let mut readback = None;
    for frame in 0..8 {
        let list = host.new_list().unwrap();
        let l: &ID3D12GraphicsCommandList = &list.list;
        // SAFETY: an open list, our live textures and views.
        unsafe {
            for (_, at, clear) in &views {
                l.ClearRenderTargetView(*at, clear, None);
            }
            let to_read: Vec<_> = views
                .iter()
                .map(|(t, ..)| crate::host::transition(t, rt, read))
                .collect();
            l.ResourceBarrier(&to_read);
        }
        let (x, y) = jitter_offset(frame, jitter_phase_count(render.0, display.0));
        let frame_values = Frame {
            jitter: [x, y],
            motion_scale: [1.0, 1.0],
            render,
            reset: frame == 0,
            pre_exposure: 1.0,
            frame_ms: 16.7,
        };
        let textures = Textures {
            color: &colour,
            depth: &depth,
            motion: &motion,
            exposure: None,
            reactive: None,
            output: &output,
        };
        feature
            .record(l, &textures, &frame_values)
            .expect("DLSS evaluates");
        // SAFETY: as above.
        unsafe {
            let back: Vec<_> = views
                .iter()
                .map(|(t, ..)| crate::host::transition(t, read, rt))
                .collect();
            l.ResourceBarrier(&back);
            if frame == 7 {
                l.ResourceBarrier(&[crate::host::transition(
                    &output,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
                readback = Some(copy_back(d, l, &output));
            }
        }
        list.close();
        host.execute(&[&list]);
    }
    // SAFETY: plain query.
    unsafe { d.GetDeviceRemovedReason() }.expect("the device is still there");
    let (buffer, footprint) = readback.unwrap();
    let mut mapped = std::ptr::null_mut();
    // SAFETY: a read-back buffer the card has finished writing.
    unsafe { buffer.Map(0, None, Some(&mut mapped)) }.unwrap();
    let pitch = footprint.Footprint.RowPitch as usize;
    let pixel = |x: usize, y: usize| {
        // SAFETY: inside the mapped footprint (8 bytes a texel).
        let p = unsafe {
            mapped
                .cast::<u8>()
                .add(footprint.Offset as usize + y * pitch + x * 8)
                .cast::<[u16; 4]>()
                .read_unaligned()
        };
        p.map(half)
    };
    let samples = [pixel(960, 540), pixel(10, 10), pixel(1900, 1070)];
    // SAFETY: mapped above.
    unsafe { buffer.Unmap(0, None) };
    eprintln!("DLSS output pixels: {samples:?}");
    for got in samples {
        for (c, (g, want)) in got.iter().zip(COLOUR).enumerate().take(3) {
            assert!(
                (g - want).abs() < 0.05,
                "channel {c}: {g} for {want} ({samples:?})"
            );
        }
    }
}

/// A title texture (our console object), as the title makes one.
fn title_texture(
    (w, h): (u32, u32),
    format: DXGI_FORMAT,
    flags: D3D12_RESOURCE_FLAGS,
    state: D3D12_RESOURCE_STATES,
) -> *const c_void {
    use crate::object::{S_OK, guid};
    use windows::core::Interface;
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: u64::from(w),
        Height: h,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Flags: flags,
        ..Default::default()
    };
    let id = ID3D12Resource::IID;
    let iid = guid(id.data1, id.data2, id.data3, id.data4);
    let null = std::ptr::null();
    let mut texture = null;
    // The title's descriptions use the PC layout.
    let made = crate::resource::create_committed(
        null,
        (&raw const heap).cast(),
        0,
        (&raw const desc).cast(),
        state.0 as u32,
        null.cast(),
        &iid,
        &mut texture,
    );
    assert_eq!(made, S_OK);
    texture.cast()
}

/// The path a title dispatch takes, on title textures: their host copies
/// brought to DLSS's states on a host list ([`evaluate_in`]), then run.
#[test]
#[ignore = "needs an NVIDIA RTX card, NGX from its driver and VICTORY_DLSS_DLL"]
fn dlss_evaluates_title_textures_on_a_host_list() {
    let model = std::env::var_os("VICTORY_DLSS_DLL").map(PathBuf::from);
    let setup = match check(Some("1"), Some("quality"), model.as_deref(), ngx::core_path) {
        Readiness::Ready(setup) => setup,
        other => {
            eprintln!("DLSS device test skipped: {other:?}");
            return;
        }
    };
    let host = crate::host::host().expect("a host GPU");
    let ngx = ngx_with(&setup).expect("NGX starts on the host device and runs DLSS");
    let (display, render) = ((2560, 1440), (1707, 960));
    let flags = Flags {
        hdr: true,
        depth_inverted: true,
        auto_exposure: true,
        ..Flags::default()
    };
    let feature = Feature::made(
        ngx,
        Create {
            render,
            display,
            flags,
            mode: Mode::Quality,
        },
    )
    .expect("the DLSS feature is made");
    let target = D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET;
    let rt = D3D12_RESOURCE_STATE_RENDER_TARGET;
    let inputs = Inputs {
        color: title_texture(render, DXGI_FORMAT_R16G16B16A16_FLOAT, target, rt),
        depth: title_texture(render, DXGI_FORMAT_R32_FLOAT, target, rt),
        motion: title_texture(render, DXGI_FORMAT_R16G16_FLOAT, target, rt),
        exposure: std::ptr::null(),
        reactive: std::ptr::null(),
        output: title_texture(
            display,
            DXGI_FORMAT_R16G16B16A16_FLOAT,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        ),
    };
    assert!(crate::external::host_texture(inputs.output).is_some_and(|t| t.unordered));
    for frame in 0..3 {
        let list = host.new_list().unwrap();
        let mut pass = crate::external::Pass::new(&list);
        let values = Frame {
            jitter: [0.25, -0.25],
            motion_scale: [1.0, 1.0],
            render,
            reset: frame == 0,
            pre_exposure: 1.0,
            frame_ms: 16.7,
        };
        evaluate_in(&feature, &mut pass, &inputs, &values)
            .expect("DLSS evaluates on the title's textures");
        list.close();
        host.execute(&[&list]);
    }
    // SAFETY: plain query.
    unsafe { host.device.GetDeviceRemovedReason() }.expect("the device is still there");
    // A null texture is said, not sent to NGX.
    let list = host.new_list().unwrap();
    let mut pass = crate::external::Pass::new(&list);
    let values = Frame {
        jitter: [0.0; 2],
        motion_scale: [1.0; 2],
        render,
        reset: false,
        pre_exposure: 1.0,
        frame_ms: 16.7,
    };
    let why = evaluate_in(
        &feature,
        &mut pass,
        &Inputs {
            depth: std::ptr::null(),
            ..inputs
        },
        &values,
    )
    .unwrap_err();
    assert!(why.contains("depth"), "{why}");
    list.close();
}

/// Records a copy of `texture` into a new read-back buffer.
///
/// # Safety
/// An open list; `texture` in the copy-source state.
unsafe fn copy_back(
    d: &ID3D12Device,
    l: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
) -> (ID3D12Resource, D3D12_PLACED_SUBRESOURCE_FOOTPRINT) {
    let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
    let mut total = 0;
    // SAFETY: the caller's; plain creation and queries.
    unsafe {
        d.GetCopyableFootprints(
            &texture.GetDesc(),
            0,
            1,
            0,
            Some(&mut footprint),
            None,
            None,
            Some(&mut total),
        );
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: total,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        d.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &desc,
            D3D12_RESOURCE_STATE_COPY_DEST,
            None,
            &mut buffer,
        )
        .unwrap();
        let buffer = buffer.unwrap();
        let to = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&buffer),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: footprint,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(texture),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        l.CopyTextureRegion(&to, 0, 0, 0, &from, None);
        (buffer, footprint)
    }
}

#[test]
fn halves_convert() {
    assert_eq!(half(0x3c00), 1.0);
    assert_eq!(half(0x3800), 0.5);
    assert_eq!(half(0x3400), 0.25);
    assert_eq!(half(0x0000), 0.0);
}
