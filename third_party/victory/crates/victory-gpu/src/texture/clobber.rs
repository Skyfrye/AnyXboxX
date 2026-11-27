//! DIAG (diagnostics builds): uploads from title memory that run after a host
//! write of the same texture. A list samples a texture and records its upload
//! from title memory; another list, recorded later on another thread
//! (`VICTORY_THIN`) but submitted ahead of it, copies into the texture on the
//! host (the title's decoration bake copies its result into a 256x256 BC3
//! texture). [`super::upload_from`] turns down uploads only of textures the
//! host wrote *before* it runs, so the upload recorded first runs after the
//! copy and puts title memory (which the copy never reached) over it.
//!
//! Counted at the submission of the list that uploads: the write of the
//! texture was recorded after the upload was ([`written`]), and its list was
//! submitted already ([`submitted`]).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// The order of records (uploads and host writes).
static SEQ: AtomicU64 = AtomicU64::new(1);

/// Per texture: the order of its last host write recorded, and of the last
/// one whose list was submitted, with that list (a list submitted again runs
/// its own upload and write in their order again).
static WRITES: Mutex<Option<HashMap<usize, (u64, u64, usize)>>> = Mutex::new(None);

/// Since the start: uploads submitted after a later-recorded host write of
/// their texture had been submitted (they run after it), uploads of a texture
/// a host write recorded after them that is not submitted yet (they run
/// first), late uploads of a texture the host wrote, and host writes of a
/// texture whose first upload is on its way on the copy queue
/// (`super::ahead`: the two queues race).
pub(crate) static COUNTS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// The next number in the order of records.
pub(crate) fn next() -> u64 {
    SEQ.fetch_add(1, Relaxed)
}

/// A host write of `resource` is recorded now: its number.
pub(crate) fn written(resource: usize) -> u64 {
    let seq = next();
    let mut map = WRITES.lock().unwrap_or_else(|e| e.into_inner());
    map.get_or_insert_with(Default::default).entry(resource).or_default().0 = seq;
    seq
}

/// `list`, of the host write numbered `seq` of `resource`, is submitted.
pub(crate) fn submitted(resource: usize, seq: u64, list: usize) {
    let mut map = WRITES.lock().unwrap_or_else(|e| e.into_inner());
    let entry = map.get_or_insert_with(Default::default).entry(resource).or_default();
    if seq >= entry.1 {
        entry.1 = seq;
        entry.2 = list;
    }
}

/// Notes the host write of `resource` in `list`, at its submission.
pub(crate) fn note_write(list: &crate::host::HostList, resource: crate::object::This) {
    if let Some(copy) = crate::resource::made_copy(resource)
        && copy.ahead.load(Relaxed) == 1
    {
        let n = COUNTS[3].fetch_add(1, Relaxed) + 1;
        if n <= 64 || n.is_power_of_two() {
            tracing::warn!(
                resource = format_args!("{:#x}", resource as usize),
                kind = ?crate::resource::texture_kind(resource),
                frame = crate::frame::current(),
                so_far = n,
                "DIAG a host write of a texture whose first upload is on its way on the copy queue: the two queues race"
            );
        }
    }
    let resource = resource as usize;
    let seq = written(resource);
    let at = std::ptr::from_ref(list) as usize;
    list.defer_host(move || submitted(resource, seq, at));
}

/// Forgets a released texture.
pub(crate) fn forget(resource: usize) {
    let mut map = WRITES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = map.as_mut() {
        map.remove(&resource);
    }
}

/// `list`, whose upload of `resource` was recorded as number `seq`, is
/// submitted: says whether a host write recorded after it, in another list,
/// was submitted before it.
pub(crate) fn upload_submitted(resource: usize, seq: u64, list: usize) {
    let (recorded, sent, by) = {
        let map = WRITES.lock().unwrap_or_else(|e| e.into_inner());
        match map.as_ref().and_then(|m| m.get(&resource)) {
            Some(&w) => w,
            None => return,
        }
    };
    if sent > seq && by != list {
        let n = COUNTS[0].fetch_add(1, Relaxed) + 1;
        if n <= 64 || n.is_power_of_two() {
            tracing::warn!(
                resource = format_args!("{resource:#x}"),
                kind = ?crate::resource::texture_kind(resource as crate::object::This),
                upload = seq,
                write_recorded = recorded,
                write_submitted = sent,
                frame = crate::frame::current(),
                so_far = n,
                "DIAG an upload from title memory runs after a host write of its texture recorded after it: it puts title memory over what the host copied"
            );
        }
    } else if recorded > seq {
        COUNTS[1].fetch_add(1, Relaxed);
    }
}

/// A late upload of the texture whose title memory is at `memory`
/// (`super::late_uploads`): says if the host wrote it.
pub(crate) fn late(memory: u64) {
    let hit = {
        let map = super::HOST_WRITTEN_AT.lock().unwrap_or_else(|e| e.into_inner());
        map.get(&memory).map(|d| d.5)
    };
    let Some(resource) = hit else { return };
    if !super::host_wrote(resource) {
        return;
    }
    let n = COUNTS[2].fetch_add(1, Relaxed) + 1;
    if n <= 64 || n.is_power_of_two() {
        tracing::warn!(
            resource = format_args!("{resource:#x}"),
            kind = ?crate::resource::texture_kind(resource as crate::object::This),
            frame = crate::frame::current(),
            so_far = n,
            "DIAG a late upload from title memory of a texture the host wrote: it puts title memory over what the host copied"
        );
    }
}

/// A texture at `at` (`bytes`, description words 4, 6, 7 and 8) is written
/// by the host for the first time: says which placed textures were made in
/// its memory before (they keep what title memory held, which the write
/// never reaches; `super::placed_in_host_written` says those made after).
pub(crate) fn placed_before(at: u64, bytes: u64, desc: [u32; 4]) {
    let placed = crate::resource::placed_between(at, at + bytes);
    if placed.is_empty() {
        return;
    }
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Relaxed) + 1;
    if n <= 200 {
        let same = placed.iter().filter(|(a, d)| *a == at && *d == desc).count();
        tracing::warn!(
            address = format_args!("{at:#x}"),
            bytes,
            desc = ?desc,
            placed = placed.len(),
            same_shape_at_its_start = same,
            first = ?placed.iter().take(4).map(|(a, d)| format!("{a:#x}:{d:?}")).collect::<Vec<_>>(),
            frame = crate::frame::current(),
            "DIAG a texture the host writes for the first time has placed textures made before in its memory: they keep title memory"
        );
    }
}

/// An upload in front of a list met a texture lists sent before wrote on the
/// card (`super::late_uploads`): the texture at `memory`, those of its
/// `parts` parts (a bit each), whether all of it was left out, the list's
/// place in the order of sent lists.
pub(crate) fn left_out(memory: u64, written: u64, parts: usize, all: bool, order: u64) {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Relaxed) + 1;
    if n <= 64 || n.is_power_of_two() {
        tracing::warn!(
            memory = format_args!("{memory:#x}"),
            written = format_args!("{written:#x}"),
            parts,
            all,
            order,
            frame = crate::frame::current(),
            so_far = n,
            "DIAG an upload in front of a list leaves out what lists sent before wrote of its texture on the card"
        );
    }
}
