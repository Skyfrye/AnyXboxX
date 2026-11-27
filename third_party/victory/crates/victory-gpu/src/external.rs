//! Host objects for PC libraries the title's calls are redirected to (NVIDIA
//! DLSS in place of the title's FidelityFX upscaler, `VICTORY_DLSS`). The
//! title holds our console objects; a PC library needs the host device, the
//! host command list the title's list records onto, and the host textures
//! behind the title's.
//!
//! Source: Neptune (VIctory Edit.) 2026-11-25c, `crates/victory-gpu/src/
//! external.rs` (the FidelityFX redirect patch received for DexNom
//! Runtime). Adapted to this base: a title list may only write its calls
//! down (`VICTORY_THIN`) and be translated later by threads of ours, so the
//! library's work is recorded at its place in the list
//! (`crate::list::record_external`) and runs when the translation gets
//! there, on the host list the list's earlier commands went to.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;

use windows::Win32::Graphics::Direct3D12::{ID3D12GraphicsCommandList, ID3D12Resource};
use windows::core::Interface;

use crate::host::HostList;
use crate::object::This;

/// D3D12 states a library's reads and writes need: any shader resource,
/// unordered access.
pub const READ: u32 = 0x40 | 0x80;
pub const WRITE: u32 = 0x8;

/// The host `ID3D12Device` (no reference taken), if there is a host GPU.
pub fn host_device() -> Option<*mut c_void> {
    crate::host::host().map(|h| h.device.as_raw())
}

/// Whether `object` is the device the title created.
pub fn is_title_device(object: *const c_void) -> bool {
    std::ptr::eq(object, crate::device::device().cast())
}

/// Whether `object` is one of our command lists.
pub fn is_title_list(object: *const c_void) -> bool {
    crate::list::list_kind(object.cast()).is_some()
}

/// Whether `object` is a live title texture (a resource of ours that is no
/// buffer).
pub fn is_title_texture(object: *const c_void) -> bool {
    let this: This = object.cast();
    !this.is_null()
        && crate::resource::is_resource(this)
        && crate::resource::buffer_bytes(this).is_none()
}

/// What a library needs to know of the host texture behind a title texture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostTexture {
    pub width: u64,
    pub height: u32,
    /// Its host DXGI format (a stand-in's, `crate::resource::target_format`).
    pub format: u32,
    /// Made with unordered access allowed.
    pub unordered: bool,
}

/// The host texture behind title texture `object` (made now if this is its
/// first use), if it is one.
pub fn host_texture(object: *const c_void) -> Option<HostTexture> {
    const ALLOW_UNORDERED_ACCESS: i32 = 0x4;
    if !is_title_texture(object) {
        return None;
    }
    let host = crate::resource::host_resource(object.cast())?;
    // SAFETY: a live resource.
    let d = unsafe { host.GetDesc() };
    Some(HostTexture {
        width: d.Width,
        height: d.Height,
        format: d.Format.0 as u32,
        unordered: d.Flags.0 & ALLOW_UNORDERED_ACCESS != 0,
    })
}

/// One title list's host list, open for a PC library to record onto.
pub struct Pass<'a> {
    list: &'a HostList,
}

impl<'a> Pass<'a> {
    pub(crate) fn new(list: &'a HostList) -> Self {
        Pass { list }
    }

    /// The host list, for what is kept until it has run.
    pub(crate) fn host_list(&self) -> &'a HostList {
        self.list
    }

    /// The host `ID3D12GraphicsCommandList` (no reference taken), with the
    /// state changes asked for so far recorded first.
    pub fn command_list(&self) -> *mut c_void {
        self.list().as_raw()
    }

    pub(crate) fn list(&self) -> &'a ID3D12GraphicsCommandList {
        let list: &'a HostList = self.list;
        // (`Deref` records the state changes held so far.)
        &list.list
    }

    /// The host texture behind title texture `texture`, moved to the host
    /// resource state `state` (a D3D12 state, as the library will see it)
    /// on this list; `write` if the library writes it. `None` if the title
    /// has no texture there.
    pub fn texture(
        &mut self,
        texture: *const c_void,
        state: u32,
        write: bool,
    ) -> Option<*mut c_void> {
        self.resource(texture, state, write).map(Interface::as_raw)
    }

    /// [`Pass::texture`], as the host resource.
    pub(crate) fn resource(
        &mut self,
        texture: *const c_void,
        state: u32,
        write: bool,
    ) -> Option<&'a ID3D12Resource> {
        let title: This = texture.cast();
        if !is_title_texture(texture) {
            return None;
        }
        let host = crate::resource::host_resource(title)?;
        // What the title fills itself is uploaded first, as for a copy.
        if !write && crate::texture::title_written(host) {
            crate::texture::upload(self.list, title, host);
        }
        // Depth textures shaders also write: what they wrote, first.
        self.list.twin_use(title, false, write);
        if write {
            // A texture made level by level gets its memory ahead of this
            // list (`crate::tiles`); one copied back to the title is copied
            // when the list closes (`crate::texback`).
            crate::texture::card_writes(self.list, title, host, None);
            self.list.texture_written(title);
            crate::texture::host_written_in(self.list, title, None);
        }
        crate::list::ensure_state(self.list, title, state);
        Some(host)
    }
}

/// Records `work` at this place of title list `list`, on the host list its
/// commands go to; afterwards the title's bindings are set again on it (the
/// library set its own). `work` may run later, on another thread, when the
/// list's calls are only written down (`VICTORY_THIN`). False, and `work`
/// dropped, if `list` is not one of our lists or records nothing now.
pub fn record(list: *const c_void, work: impl FnOnce(&mut Pass) + Send + 'static) -> bool {
    crate::list::record_external(
        list.cast(),
        Box::new(move |host| {
            work(&mut Pass::new(host));
        }),
    )
}

/// Runs `work` on a host list of its own and waits for the card to have run
/// it: for a library's setup work (DLSS records some when its feature is
/// made). What `work` returns, or `None` without a host GPU.
pub(crate) fn run_now<T>(work: impl FnOnce(&Pass) -> T) -> Option<T> {
    let host = crate::host::host()?;
    let list = match host.new_list() {
        Ok(list) => list,
        Err(e) => {
            tracing::warn!(error = %e, "no host list for a library's setup");
            return None;
        }
    };
    let done = work(&Pass::new(&list));
    list.close();
    host.execute_sync_internal(&list);
    Some(done)
}
