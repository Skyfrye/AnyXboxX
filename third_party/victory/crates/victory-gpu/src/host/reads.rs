//! A submission whose steps read on the CPU title memory that flights in the
//! air may write, where the card cannot do the reads for it
//! ([`card_steps`]), waits per fence (`VICTORY_READ_WAIT`, see [`on`]).
//!
//! What it waited for before: until no flight in the air declared a write
//! over what it reads ([`relay_wait`] over [`blocked_by`]), that is until
//! the thread that lands flights ([`relay`]) had landed every flight up to
//! the last such one: their copies in the card's memory compared, fetched
//! and merged, those in the PC's memory compared and merged, all of what
//! they wrote, in order. A fill of upload memory once a frame stood some 60
//! ms there (run nbc-measure: "title memory read 1.0 (... a fill of upload
//! memory 1.0)", 62-63 ms a frame, the card's steps refusing it).
//!
//! Now it waits for the card: for the host's fence behind the last flight
//! that declared a write over what the steps read, and for the flights
//! ahead of that one to land, as they do meanwhile. Then what they read, and
//! only that, is brought back into title memory at once, from the host
//! copies that flight wrote, merged as its landing merges them
//! ([`crate::land`], or `Shadow::relay_merge` and `Shadow::card_merge_relay`
//! with `VICTORY_LAND=0`): a copy in the PC's memory from its snapshot on, a
//! copy in the card's memory fetched over a copy queue of its own. The
//! landing that comes later finds those bytes merged already (the copy's
//! reference holds them) and leaves them, and leaves what the title wrote
//! over them since.
//!
//! Why the title memory read is then what it was after the old wait. A byte
//! of a range read is the title's, or the last of the flights that wrote it
//! wrote it. Every flight that may write it declared that, and all of them
//! are ahead of the last one waited for, on the one queue: the card has run
//! them, and no later flight declares it, so the host copy holds their last
//! write and nothing writes it any more. The merge takes from the copy only
//! the bytes that differ from its reference (what it held when they started,
//! or since the last merge), as the landing does, and leaves the title's
//! other bytes; of a copy in the card's memory it leaves out the title's
//! bytes that went up behind the last flight waited for, or not at all yet,
//! as the landing does (the card's side is the older there). Of a copy in
//! the card's memory only what that landing lands is brought back: its
//! plan's ranges, not what `crate::picked` found and does not land
//! (`VICTORY_PICKED_WRITES=1`).
//!
//! And what the title can see stays in the relay's order (a later flight's
//! results never reach title memory before an earlier flight's): the
//! flights ahead of the one read have landed, whole, before anything of it
//! is brought back. No landing runs meanwhile: one in its course is waited
//! for, and the thread that lands takes no new flights until the merge is
//! done (`Line::reading`), so no fetch made before the card ran the flight
//! read is merged after this. While the read waits, that thread lands what
//! is ahead of the flight read, and not it ([`landing_limit`]).
//!
//! Where that cannot be said the old wait is taken ([`needs`], [`plan`],
//! [`Plan::fits`]), and the reason is told before anything is waited for
//! where it can be: a step of ours that may read or write anything; a title
//! buffer copy into memory flights may write (it must land over what they
//! wrote, after them, and the submission's own refreshes would leave it
//! out); a copy at its place that waits; what a copy done on the card wrote
//! read through another host copy; a texture's or a copy's bytes read back
//! there, or a value written there behind a flight (a landing writes those);
//! two host copies written over the same title memory; more than [`MOST`]
//! bytes to bring back; a copy in the card's memory without the relay's copy
//! queue (`VICTORY_RELAY_COPY=1`), where the landing's fetches go to the
//! drawing queue and a fetch beside it would be new, or with the check of
//! torn fetches (`VICTORY_FETCH_CHECK=1`, Linux and AMD), which this fetch
//! does not make; a copy in the PC's memory without its snapshot.

use std::sync::Arc;

use super::*;
use crate::shadow::Shadow;

/// Whether reads wait per fence and bring back what they read
/// (`VICTORY_READ_WAIT=0`: they wait for the landings of the flights, as
/// before; on by default). Only with the relay's second stage
/// ([`relay_exact`]), where the wait it replaces is.
pub(super) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_READ_WAIT").is_none_or(|v| v != "0"))
}

/// The most bytes a submission brings back this way.
const MOST: usize = 16 << 20;

/// Since the last frame-rate line: reads waited for per fence; flights in
/// the air up to the last one waited for; microseconds waiting for the card;
/// microseconds bringing back; bytes brought back; of them bytes put into
/// title memory; parts of copies in the PC's memory and in the card's;
/// reads that took the old wait; microseconds of the fetches; microseconds
/// waiting for landings (of the flights ahead, or one in its course); of
/// the reads that took the old wait, microseconds they had waited before.
static STATS: [std::sync::atomic::AtomicU64; 12] = [const { std::sync::atomic::AtomicU64::new(0) }; 12];

/// Why reads took the old wait, since the last frame-rate line.
static REFUSED: Mutex<Vec<(&'static str, u64)>> = Mutex::new(Vec::new());

/// A read takes the old wait for `why`.
fn refused(why: &'static str) {
    STATS[8].fetch_add(1, Ordering::Relaxed);
    let mut all = REFUSED.lock().unwrap_or_else(|e| e.into_inner());
    match all.iter_mut().find(|(w, _)| *w == why) {
        Some((_, n)) => *n += 1,
        None => all.push((why, 1)),
    }
}

/// For the frame-rate line.
pub(super) fn log(frames: f64) {
    if !on() || !relay_exact() {
        return;
    }
    let s: Vec<f64> = STATS.iter().map(|s| s.swap(0, Ordering::Relaxed) as f64 / frames).collect();
    let mut refused = std::mem::take(&mut *REFUSED.lock().unwrap_or_else(|e| e.into_inner()));
    refused.sort_by_key(|&(why, n)| (std::cmp::Reverse(n), why));
    let refused: Vec<String> = refused.iter().take(3).map(|(why, n)| format!("{:.2} {why}", *n as f64 / frames)).collect();
    tracing::info!(
        "title memory read under flights, per frame (VICTORY_READ_WAIT=0: until the flights have landed): {:.2} reads waited \
         for the card's fence and the landings ahead of it, behind {:.1} flights each, {:.1} ms for the card, {:.2} ms for \
         the landings, then {:.2} ms bringing back {:.1} KB ({:.1} KB of it new to title memory; {:.1} parts of copies in the \
         PC's memory, {:.1} fetched from the card's in {:.2} ms); {:.2} waited for landings ({:.1} ms of it before they \
         were refused){}{}",
        s[0],
        if s[0] > 0.0 { s[1] / s[0] } else { 0.0 },
        s[2] / 1000.0,
        s[10] / 1000.0,
        s[3] / 1000.0,
        s[4] / 1024.0,
        s[5] / 1024.0,
        s[6],
        s[7],
        s[9] / 1000.0,
        s[8],
        s[11] / 1000.0,
        if refused.is_empty() { "" } else { ": " },
        refused.join("; "),
    );
}

/// What the CPU steps `steps` read of `written` (title memory flights may
/// write, in order and joined) goes into `out`; `placed` is set where a
/// copy at its place waits for its readers. As [`blocked_by`] sees them.
fn needs_of<'a>(
    written: &[(usize, usize)],
    steps: impl IntoIterator<Item = &'a Step>,
    out: &mut Vec<(usize, usize)>,
    placed: &mut bool,
) -> Result<(), &'static str> {
    for step in steps {
        match step {
            &Step::Copy(dst, src, len) => {
                if touches(written, dst, dst + len) {
                    return Err("a buffer copy into memory flights may write");
                }
                if read_blocked(written, src, len) {
                    out.push((src, src + len));
                }
            }
            &Step::Fill(_, src, len, _) => {
                if read_blocked(written, src, len) {
                    out.push((src, src + len));
                }
            }
            Step::Texture(staging) => {
                let (at, len) = staging.memory();
                if touches(written, at, at + len) {
                    out.push((at, at + len));
                }
            }
            &Step::Plane((at, len), _) => {
                if touches(written, at, at.saturating_add(len)) {
                    out.push((at, at.saturating_add(len)));
                }
            }
            Step::Run(_) => return Err("a step of ours runs"),
            Step::Placed(p) => match p.waits(written) {
                place::Waits::No => {}
                place::Waits::ForReaders => *placed = true,
                place::Waits::Yes => return Err("a buffer copy at its place"),
            },
            Step::Refresh(..) | Step::GpuWrite(..) | Step::Host(..) => {}
        }
    }
    Ok(())
}

/// What the CPU steps of `runs` read of `written`, in order and joined; or
/// why the reads cannot wait per fence.
fn needs(written: &[(usize, usize)], runs: &[&HostList]) -> Result<Vec<(usize, usize)>, &'static str> {
    let mut out = Vec::new();
    let mut placed = false;
    for list in runs {
        needs_of(written, list.deferred().copies.iter(), &mut out, &mut placed)?;
    }
    if placed && place::read_by(runs, written) {
        return Err("what a copy at its place writes is read");
    }
    join_ranges(&mut out, []);
    Ok(out)
}

/// A part of a host copy to bring back: the copy, and title memory (from,
/// to).
type Part = (Arc<Shadow>, usize, usize);

/// What a read waits for and brings back.
struct Plan {
    /// The host fence's value behind the last flight in the air that
    /// declared a write over what is read.
    fence: u64,
    /// The flights in the air up to that one, it too: those ahead of it
    /// land before anything is brought back.
    flights: usize,
    /// What is brought back, each copy's parts in order and joined.
    back: Vec<Part>,
    /// The send to copies in the card's memory those flights ran behind
    /// (`CardPlan::sent_as`; 0: none).
    sent: u64,
}

impl Plan {
    /// Whether what it brings back can be brought back; asked as soon as
    /// it is made, before anything is waited for, and again just before.
    fn fits(&self) -> Result<(), &'static str> {
        let carded = self.back.iter().any(|p| p.0.carded());
        // A fetch on a queue of its own, beside the flights that follow, is
        // what the relay does only with its switch: without it, as before.
        if carded && !relay_copy() {
            return Err("copies in the card's memory are fetched on the drawing queue (VICTORY_RELAY_COPY=0)");
        }
        // The relay hashes what its fetches bring there (`torn`); this
        // fetch does not, and was not tried where that was needed.
        if carded && torn::on() {
            return Err("copies in the card's memory, with torn fetches checked (VICTORY_FETCH_CHECK=1)");
        }
        if self.back.iter().any(|(shadow, ..)| !shadow.carded() && !shadow.snapshot_held()) {
            return Err("a copy in the PC's memory holds no snapshot");
        }
        Ok(())
    }
}

/// What lies of `[a, b)`, a declared write of `shadow`, in what is read
/// (`needs`, in order and joined) and in the copy, goes into `back`.
fn parts_of(shadow: &Arc<Shadow>, (a, b): (usize, usize), needs: &[(usize, usize)], back: &mut Vec<Part>) {
    let (start, len) = shadow.span();
    let (start, end) = (start as usize, (start + len) as usize);
    let first = needs.partition_point(|n| n.1 <= a);
    for &(na, nb) in needs[first..].iter().take_while(|n| n.0 < b) {
        let (from, to) = (a.max(na).max(start), b.min(nb).min(end));
        if from < to {
            back.push((shadow.clone(), from, to));
        }
    }
}

/// For reads of `needs` (in order and joined), with `items` in the air:
/// what to wait for and bring back; `None` if no flight in the air declared
/// a write over them.
fn plan(items: &[Late], needs: &[(usize, usize)]) -> Result<Option<Plan>, &'static str> {
    let mut last = None;
    let mut back: Vec<Part> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let flight = match item {
            Late::Run(flight) => flight,
            // (Not among what flights declare; a landing up to the flight
            // waited for would have written it.)
            &Late::Write(at, _, bytes) if touches(needs, at, at + bytes) => {
                return Err("a value is written there behind a flight");
            }
            _ => continue,
        };
        if flight.texture_back_ranges.iter().any(|&(a, b)| touches(needs, a, b)) {
            return Err("a texture a flight wrote is read back there");
        }
        let read_back = flight.order.iter().any(|&l| {
            flight.lists[l].host().deferred().backs.iter().any(|(_, at, rows)| {
                let from = at + rows.title_start;
                touches(needs, from, from + rows.title_span())
            })
        });
        if read_back {
            return Err("a flight reads back there");
        }
        // What it declared it may write is waited for, all of it.
        for (shadow, ranges) in &flight.back {
            for &(a, b) in ranges {
                if !touches(needs, a, b) {
                    continue;
                }
                last = Some(i);
                // In place the card wrote title memory itself; of a copy in
                // the card's memory, what its landing lands (below).
                if !shadow.in_place() && !shadow.carded() {
                    parts_of(shadow, (a, b), needs, &mut back);
                }
            }
        }
        // Of copies in the card's memory, what its landing lands: its
        // plan's ranges (`Host::relay_card_back`), not those `crate::picked`
        // only looks at, nor, at its first stage, the buffers it found,
        // which are declared and not landed (`Host::card_before`).
        for r in flight.card.iter().flat_map(|c| &c.ranges).filter(|r| !r.picked && !r.shadow.in_place()) {
            if touches(needs, r.declared.0, r.declared.1) {
                last = Some(i);
                parts_of(&r.shadow, r.declared, needs, &mut back);
            }
        }
    }
    let Some(last) = last else {
        return Ok(None);
    };
    // Each copy's parts joined; then no two copies over the same bytes.
    back.sort_by_key(|(shadow, from, _)| (Arc::as_ptr(shadow) as usize, *from));
    let mut joined: Vec<Part> = Vec::with_capacity(back.len());
    for (shadow, from, to) in back {
        match joined.last_mut() {
            Some((s, _, end)) if Arc::ptr_eq(s, &shadow) && from <= *end => *end = (*end).max(to),
            _ => joined.push((shadow, from, to)),
        }
    }
    let mut by_address: Vec<&Part> = joined.iter().collect();
    by_address.sort_by_key(|p| p.1);
    if by_address.windows(2).any(|w| w[1].1 < w[0].2) {
        return Err("flights write it through two host copies");
    }
    if joined.iter().map(|p| p.2 - p.1).sum::<usize>() > MOST {
        return Err("too much to bring back");
    }
    let flights: Vec<&Flight> = items[..=last]
        .iter()
        .filter_map(|l| match l {
            Late::Run(flight) => Some(flight),
            _ => None,
        })
        .collect();
    Ok(Some(Plan {
        fence: flights.last().map_or(0, |f| f.fence),
        flights: flights.len(),
        back: joined,
        sent: flights.iter().filter_map(|f| f.card.as_ref().map(|c| c.sent_as)).max().unwrap_or(0),
    }))
}

/// `parts` cut where the 64 KiB blocks of title memory end, as a landing
/// cuts its pieces ([`crate::land::BLOCK`]), in the order of title memory
/// (no two parts hold the same bytes): the part's number, from, to.
fn blocks(parts: &[&Part]) -> Vec<(usize, usize, usize)> {
    let block = crate::land::BLOCK;
    let mut out = Vec::new();
    for (i, &&(_, from, to)) in parts.iter().enumerate() {
        let mut at = from;
        while at < to {
            let end = ((at / block + 1) * block).min(to);
            out.push((i, at, end));
            at = end;
        }
    }
    out.sort_by_key(|p| p.1);
    out
}

/// Parts of copies in the PC's memory go into title memory as their
/// landing puts them (`RelayBack::merge`): the bytes put.
fn merge_back(parts: &[&Part]) -> u64 {
    use crate::land;
    if parts.is_empty() {
        return 0;
    }
    if !land::on() {
        let (mut now, mut title) = (Vec::new(), Vec::new());
        return parts
            .iter()
            .map(|(shadow, from, to)| {
                let start = shadow.span().0 as usize;
                shadow.relay_merge(from - start, to - from, &mut now, &mut title) as u64
            })
            .sum();
    }
    let pieces = blocks(parts);
    let titles: Vec<(usize, usize)> = pieces.iter().map(|&(_, from, to)| (from, to - from)).collect();
    let jobs = land::jobs(&titles);
    let put = std::sync::atomic::AtomicU64::new(0);
    land::each(jobs.len(), &|j| {
        let mut notes = land::Notes::default();
        for &(i, from, to) in &pieces[jobs[j].clone()] {
            let shadow = &parts[i].0;
            put.fetch_add(shadow.land_back(from - shadow.span().0 as usize, to - from, &mut notes), Ordering::Relaxed);
        }
        notes.flush();
    });
    land::count(land::stat::JOBS, jobs.len() as u64);
    put.into_inner()
}

/// Whether the thread that lands flights may take the first of the line:
/// something is there, and no read brings bytes back meanwhile.
pub(super) fn may_land(line: &Line) -> bool {
    !line.line.is_empty() && !line.reading && !held()
}

/// The fence value up to which the thread that lands flights may land them
/// ([`landable`]) when the card has reached `completed` and a read waits for
/// the flight of `hold` ([`Line::hold`]; 0: none): not that flight.
///
/// Without it, when the card ran the flight a read waits for, the read and
/// the thread that lands woke together, and if that thread took the line
/// first the read waited for the whole landing, as before.
pub(super) fn landing_limit(completed: u64, hold: u64) -> u64 {
    match hold {
        0 => completed,
        hold => completed.min(hold - 1),
    }
}

/// Tests: the thread that lands flights takes none while this is set (a
/// landing that is late, or never comes). Never set outside them.
pub(super) static HOLD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn held() -> bool {
    cfg!(test) && HOLD.load(Ordering::Acquire)
}

/// Tests: milliseconds a read sleeps once the card has run what it waited
/// for, before it looks at the line again (the thread that lands flights,
/// woken by the same fence, has long taken what it may by then).
#[cfg(test)]
static LOOK_LATE_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Tests: called once a read has brought its bytes back, before the thread
/// that lands flights may take the flight read.
#[cfg(test)]
type Brought = Mutex<Option<Box<dyn FnMut() + Send>>>;
#[cfg(test)]
static BROUGHT: Brought = Mutex::new(None);

/// What reads fetch parts of copies in the card's memory with: a copy queue
/// of their own (whatever was sent to the drawing queue meanwhile, the fetch
/// does not wait for it), a list, a fence, an event and the memory the parts
/// come down to (buffer, mapping, bytes).
struct Fetch {
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    queue: ID3D12CommandQueue,
    fence: ID3D12Fence,
    event: HANDLE,
    sent: u64,
    buffer: Option<(ID3D12Resource, usize, u64)>,
}

// SAFETY: D3D12 objects (free-threaded) and an event, used under the lock
// by the thread that holds the submission lock.
unsafe impl Send for Fetch {}

static FETCH: Mutex<Option<Fetch>> = Mutex::new(None);

/// Waits without the line's lock (`held`) until flights have landed once
/// more, standing as [`relay_wait`] stands; the microseconds go to `us`.
fn landings(held: std::sync::MutexGuard<'static, Line>, us: &mut u64) -> std::sync::MutexGuard<'static, Line> {
    let was_working = working(false);
    let waiting = std::time::Instant::now();
    let held = LINE_LANDED.wait(held).unwrap_or_else(|e| e.into_inner());
    stood(waiting);
    working(was_working);
    *us += waiting.elapsed().as_micros() as u64;
    held
}

impl Host {
    /// [`late_blocked`] said a step of `runs` reads title memory flights in
    /// the air may write, and the card does not do it ([`card_steps`]):
    /// waits for the card to have run the last flight that declared a write
    /// over what is read, and for the flights ahead of it to land, and
    /// brings that back into title memory (see the file). If that was not
    /// done, the caller waits for the landings as before, counted from the
    /// moment given (`Err(Some(..))`: this waited already) or from its own
    /// wait (`Err(None)`). Under the submission lock (`submitted`), whose
    /// event it waits with.
    pub(super) fn read_wait(&self, submitted: &mut u64, runs: &[&HostList]) -> Result<(), Option<std::time::Instant>> {
        let _ = submitted;
        let (written, copies) = {
            let line = line();
            (line.written.clone(), card_steps::copies(&line))
        };
        if card_steps::copied_read(&copies, runs) {
            refused("what a copy done on the card wrote is read through another host copy");
            return Err(None);
        }
        let needs = match needs(&written, runs) {
            Ok(needs) if !needs.is_empty() => needs,
            // (What blocked it is not among the reads: as before.)
            Ok(_) => {
                refused("no read of what flights write was found");
                return Err(None);
            }
            Err(why) => {
                refused(why);
                return Err(None);
            }
        };
        let began = std::time::Instant::now();
        let _waiting = crate::timeline::span("waiting for the card: title memory read");
        let (mut card_us, mut landing_us, mut flights) = (0u64, 0u64, 0usize);
        let mut waited = false;
        // Lets the thread that lands flights take them all again.
        let release = |mut line: std::sync::MutexGuard<'_, Line>| {
            (line.reading, line.hold) = (false, 0);
            drop(line);
            RELAY_MOVED.notify_one();
        };
        // Refused once something was waited for: that time is the old
        // wait's ([`relay_wait_since`]).
        let late = |why: &'static str, waited: bool| {
            refused(why);
            waited.then(|| {
                STATS[11].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
                began
            })
        };
        let mut held = line();
        loop {
            // A landing in its course is waited for: what it fetched may
            // be older than what is brought back here.
            if held.settling {
                held = landings(held, &mut landing_us);
                waited = true;
                continue;
            }
            let plan = match plan(&held.line, &needs) {
                Ok(Some(plan)) => plan,
                // Nothing in the air writes it: as if nothing blocked it.
                Ok(None) if !waited => {
                    release(held);
                    return Ok(());
                }
                // Landed meanwhile: title memory holds it.
                Ok(None) => {
                    release(held);
                    break;
                }
                Err(why) => {
                    release(held);
                    return Err(late(why, waited));
                }
            };
            if let Err(why) = plan.fits() {
                release(held);
                return Err(late(why, waited));
            }
            flights = flights.max(plan.flights);
            // From here on what is ahead of it lands meanwhile, and not it.
            if held.hold != plan.fence {
                held.hold = plan.fence;
                RELAY_MOVED.notify_one();
            }
            // SAFETY: a plain query.
            if unsafe { self.fence.GetCompletedValue() } < plan.fence {
                drop(held);
                card_us += self.card_wait(plan.fence);
                waited = true;
                #[cfg(test)]
                std::thread::sleep(std::time::Duration::from_millis(LOOK_LATE_MS.load(Ordering::Relaxed)));
                held = line();
                continue;
            }
            // The flights ahead of it land first, whole: a later flight's
            // bytes never reach title memory before an earlier flight's.
            if plan.flights > 1 {
                held = landings(held, &mut landing_us);
                waited = true;
                continue;
            }
            // The card has run it, and it is the first of the line: no
            // landing starts until the bytes are in.
            held.reading = true;
            drop(held);
            let bringing = std::time::Instant::now();
            let brought = self.bring_back(&plan);
            STATS[3].fetch_add(bringing.elapsed().as_micros() as u64, Ordering::Relaxed);
            #[cfg(test)]
            if let Some(brought) = BROUGHT.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                brought();
            }
            release(line());
            match brought {
                Ok((bytes, put)) => {
                    STATS[4].fetch_add(bytes, Ordering::Relaxed);
                    STATS[5].fetch_add(put, Ordering::Relaxed);
                    COPY_BACK[1].fetch_add(put, Ordering::Relaxed);
                }
                Err(why) => return Err(late(why, true)),
            }
            break;
        }
        let us = began.elapsed().as_micros() as u64;
        STATS[0].fetch_add(1, Ordering::Relaxed);
        STATS[1].fetch_add(flights as u64, Ordering::Relaxed);
        STATS[2].fetch_add(card_us, Ordering::Relaxed);
        STATS[10].fetch_add(landing_us, Ordering::Relaxed);
        // On the settles' line as the wait it replaces was.
        LATE_STATS[1].fetch_add(1, Ordering::Relaxed);
        LATE_STATS[WHY_READ].fetch_add(1, Ordering::Relaxed);
        LATE_STATS[11].fetch_add(flights as u64, Ordering::Relaxed);
        LATE_STATS[12].fetch_add(us, Ordering::Relaxed);
        LATE_WAITED[WHY_READ].fetch_add(us, Ordering::Relaxed);
        SUBMISSION_US[1].fetch_add(us, Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        blamed(WHY_READ, us);
        Ok(())
    }

    /// Waits for the host's fence to reach `fence`, standing as
    /// [`relay_wait`] stands: the microseconds. Under the submission lock,
    /// whose event it waits with.
    fn card_wait(&self, fence: u64) -> u64 {
        let was_working = working(false);
        let waiting = std::time::Instant::now();
        // SAFETY: the host's fence; the event is the submission lock's,
        // which the caller holds.
        unsafe {
            if self.fence.SetEventOnCompletion(fence, self.event).is_ok() {
                #[cfg(feature = "diagnostics")]
                let _card = crate::timeline::card_wait(Some(fence));
                WaitForSingleObject(self.event, INFINITE);
            } else {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        stood(waiting);
        working(was_working);
        waiting.elapsed().as_micros() as u64
    }

    /// The parts of `plan` go into title memory: the bytes read, and the
    /// bytes of them put into title memory.
    fn bring_back(&self, plan: &Plan) -> Result<(u64, u64), &'static str> {
        // (Asked again first: nothing is merged where something is refused.)
        plan.fits()?;
        let mut put = 0u64;
        let carded: Vec<&Part> = plan.back.iter().filter(|p| p.0.carded()).collect();
        if !carded.is_empty() {
            put += self.fetch_back(&carded, plan.sent)?;
            STATS[7].fetch_add(carded.len() as u64, Ordering::Relaxed);
        }
        let others: Vec<&Part> = plan.back.iter().filter(|p| !p.0.carded()).collect();
        put += merge_back(&others);
        STATS[6].fetch_add(others.len() as u64, Ordering::Relaxed);
        let bytes = plan.back.iter().map(|p| (p.2 - p.1) as u64).sum();
        Ok((bytes, put))
    }

    /// Parts of copies in the card's memory: fetched over the copy queue
    /// of [`FETCH`], and merged as their landing merges them
    /// (`Host::relay_card_back`), as having run behind send `sent` (title
    /// bytes that went up later are left out). The bytes put into title
    /// memory.
    fn fetch_back(&self, parts: &[&Part], sent: u64) -> Result<u64, &'static str> {
        use crate::land;
        let total: u64 = parts.iter().map(|p| (p.2 - p.1) as u64).sum();
        let mut fetch = FETCH.lock().unwrap_or_else(|e| e.into_inner());
        if fetch.is_none() {
            *fetch = self.make_read_fetch();
        }
        let Some(f) = fetch.as_mut() else {
            return Err("no copy queue to fetch with");
        };
        if f.buffer.as_ref().is_none_or(|b| b.2 < total) {
            let size = total.next_multiple_of(1 << 20);
            f.buffer = self.shared_buffer(size).map(|(buffer, data)| (buffer, data as usize, size));
        }
        let Some((buffer, data, _)) = f.buffer.clone() else {
            return Err("no memory to fetch into");
        };
        f.sent += 1;
        // Of the sends up to which a merge was made since, every fetch is
        // behind (`crate::shadow::relay_fetched`).
        let after = sent.max(crate::shadow::fetched_so_far());
        let fetching = std::time::Instant::now();
        // Where each part comes down.
        let mut at = Vec::with_capacity(parts.len());
        let mut end = 0usize;
        for p in parts {
            at.push(end);
            end += p.2 - p.1;
        }
        // SAFETY: the list's last run was waited for (under the lock); live
        // buffers (the plan holds the copies); the parts lie in the copies.
        let ran = unsafe {
            (|| -> windows::core::Result<()> {
                f.allocator.Reset()?;
                f.list.Reset(&f.allocator, None)?;
                for ((shadow, from, to), &at) in parts.iter().map(|p| (&p.0, p.1, p.2)).zip(&at) {
                    f.list.CopyBufferRegion(&buffer, at as u64, &shadow.buffer, shadow.offset(from as u64), (to - from) as u64);
                }
                f.list.Close()?;
                f.queue.ExecuteCommandLists(&[Some(f.list.cast::<ID3D12CommandList>()?)]);
                f.queue.Signal(&f.fence, f.sent)?;
                if f.fence.GetCompletedValue() < f.sent {
                    f.fence.SetEventOnCompletion(f.sent, f.event)?;
                    WaitForSingleObject(f.event, INFINITE);
                }
                Ok(())
            })()
        };
        STATS[9].fetch_add(fetching.elapsed().as_micros() as u64, Ordering::Relaxed);
        if ran.is_err() {
            return Err("the fetch failed");
        }
        let mut put = 0u64;
        if land::on() {
            let pieces = blocks(parts);
            let titles: Vec<(usize, usize)> = pieces.iter().map(|&(_, from, to)| (from, to - from)).collect();
            let jobs = land::jobs(&titles);
            let landed = std::sync::atomic::AtomicU64::new(0);
            land::each(jobs.len(), &|j| {
                let mut notes = land::Notes::default();
                for run in pieces[jobs[j].clone()].chunk_by(|a, b| Arc::ptr_eq(&parts[a.0].0, &parts[b.0].0)) {
                    let shadow = &parts[run[0].0].0;
                    let start = shadow.span().0 as usize;
                    let addressed: Vec<(usize, usize, usize)> =
                        run.iter().map(|&(i, from, to)| (from - start, to - start, data + at[i] + (from - parts[i].1))).collect();
                    // SAFETY: the mapping of the fetched bytes, `total` of
                    // them; the copy into it has run, and the next waits for
                    // this call (the lock is held).
                    landed.fetch_add(unsafe { shadow.land_card(&addressed, after, &mut notes) }, Ordering::Relaxed);
                }
                notes.flush();
            });
            land::count(land::stat::JOBS, jobs.len() as u64);
            put = landed.into_inner();
        } else {
            let mut title = Vec::new();
            for ((shadow, from, to), &at) in parts.iter().map(|p| (&p.0, p.1, p.2)).zip(&at) {
                // SAFETY: the mapping of the fetched bytes, `total` of them;
                // the copy into it has run, and the next waits for this.
                let gpu = unsafe { std::slice::from_raw_parts((data + at) as *const u8, to - from) };
                let merged = shadow.card_merge_relay(from - shadow.span().0 as usize, gpu, after, &mut title);
                shadow.walls_merged(gpu.len(), merged);
                put += merged as u64;
            }
        }
        Ok(put)
    }

    fn make_read_fetch(&self) -> Option<Fetch> {
        // SAFETY: plain object creation.
        unsafe {
            let allocator: ID3D12CommandAllocator = self.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY).ok()?;
            let list: ID3D12GraphicsCommandList = self.device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None).ok()?;
            list.Close().ok()?;
            let queue: ID3D12CommandQueue = self
                .device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_COPY,
                    Priority: D3D12_COMMAND_QUEUE_PRIORITY_HIGH.0,
                    ..Default::default()
                })
                .ok()?;
            name(&queue, || "title memory read: fetches".into());
            let fence: ID3D12Fence = self.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
            let event = CreateEventW(None, false, false, None).ok()?;
            Some(Fetch {
                allocator,
                list,
                queue,
                fence,
                event,
                sent: 0,
                buffer: None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flight(fence: u64, back: Vec<Declared>, texture_back_ranges: Vec<(usize, usize)>) -> Late {
        flight_with(fence, back, None, texture_back_ranges)
    }

    fn flight_with(fence: u64, back: Vec<Declared>, card: Option<CardPlan>, texture_back_ranges: Vec<(usize, usize)>) -> Late {
        Late::Run(Flight {
            number: fence,
            fence,
            lists: Vec::new(),
            order: Vec::new(),
            card,
            back,
            own: None,
            texture_back_ranges,
        })
    }

    fn copy(start: usize, len: usize) -> Arc<Shadow> {
        // (Never read: the plan looks at spans alone.)
        Shadow::unbacked(start as u64, std::ptr::NonNull::<u8>::dangling().as_ptr(), len as u64, false)
    }

    /// A copy in the card's memory, as [`copy`].
    fn carded(start: usize, len: usize) -> Arc<Shadow> {
        Shadow::unbacked(start as u64, std::ptr::NonNull::<u8>::dangling().as_ptr(), len as u64, true)
    }

    /// A flight's plan for copies in the card's memory with `ranges`
    /// (copy, declared range, whether `crate::picked` only looks at it).
    fn card(ranges: Vec<(Arc<Shadow>, (usize, usize), bool)>) -> CardPlan {
        CardPlan {
            ranges: ranges
                .into_iter()
                .map(|(shadow, declared, picked)| CardRange {
                    span: shadow.card_span(declared.0, declared.1).unwrap(),
                    shadow,
                    declared,
                    at: 0,
                    compared: false,
                    picked,
                })
                .collect(),
            results: (0, 0),
            check: None,
            _synced: Vec::new(),
            lists: Vec::new(),
            sent_as: 0,
            looks: None,
            sends: None,
            up: None,
        }
    }

    /// A read waits for the last flight that declared a write over what it
    /// reads, not for the last flight in the air, and brings back only what
    /// it reads of what they declared.
    #[test]
    fn a_read_waits_for_the_flights_that_write_what_it_reads() {
        let a = copy(0x10_0000, 0x10_0000);
        let line = [
            flight(5, vec![(a.clone(), vec![(0x10_1000, 0x10_3000)])], Vec::new()),
            Late::Signal(0x99, 1),
            flight(6, vec![(a.clone(), vec![(0x18_0000, 0x19_0000)])], Vec::new()),
            flight(7, Vec::new(), Vec::new()),
        ];
        let p = plan(&line, &[(0x10_2800, 0x10_2900)]).unwrap().unwrap();
        assert_eq!((p.fence, p.flights), (5, 1));
        let back: Vec<(usize, usize)> = p.back.iter().map(|p| (p.1, p.2)).collect();
        assert_eq!(back, [(0x10_2800, 0x10_2900)]);
        // Over both: the second; of what each declared, what is read.
        let p = plan(&line, &[(0x10_2f00, 0x10_3100), (0x18_8000, 0x18_8010)]).unwrap().unwrap();
        assert_eq!((p.fence, p.flights), (6, 2));
        let back: Vec<(usize, usize)> = p.back.iter().map(|p| (p.1, p.2)).collect();
        assert_eq!(back, [(0x10_2f00, 0x10_3000), (0x18_8000, 0x18_8010)]);
        // Over none of them (or only touching an end): nothing to wait for.
        assert!(plan(&line, &[(0x10_3000, 0x10_4000), (0x30_0000, 0x30_0100)]).unwrap().is_none());
        assert!(plan(&[], &[(0, 1)]).unwrap().is_none());
    }

    /// Two flights over the same bytes of one copy: the later is waited for,
    /// and the bytes are brought back once.
    #[test]
    fn bytes_two_flights_declared_are_brought_back_once() {
        let a = copy(0x10_0000, 0x10_0000);
        let line = [
            flight(5, vec![(a.clone(), vec![(0x10_0000, 0x10_8000)])], Vec::new()),
            flight(6, vec![(a.clone(), vec![(0x10_4000, 0x10_c000), (0x10_0000, 0x10_1000)])], Vec::new()),
        ];
        let p = plan(&line, &[(0x10_0800, 0x10_5000)]).unwrap().unwrap();
        assert_eq!((p.fence, p.flights), (6, 2));
        let back: Vec<(usize, usize)> = p.back.iter().map(|p| (p.1, p.2)).collect();
        assert_eq!(back, [(0x10_0800, 0x10_5000)]);
    }

    /// Of a copy in the card's memory only what its landing lands comes
    /// back: its plan's ranges. Not a buffer `crate::picked` found, which is
    /// declared and not landed at its first stage (`VICTORY_PICKED_WRITES=1`:
    /// the title may have other things there by now), nor what it only
    /// looks at; a flight that declared what is read is waited for all the
    /// same.
    #[test]
    fn of_a_copy_in_the_cards_memory_only_what_its_landing_lands_comes_back() {
        let a = carded(0x10_0000, 0x10_0000);
        // Its shaders' declared writes, and a buffer `picked` found.
        let back = vec![(a.clone(), vec![(0x10_0000, 0x10_4000), (0x10_8000, 0x10_9000)])];
        let plan_of = card(vec![(a.clone(), (0x10_0000, 0x10_4000), false), (a.clone(), (0x10_c000, 0x10_d000), true)]);
        let line = [flight_with(5, back, Some(plan_of), Vec::new())];
        let p = plan(&line, &[(0x10_3000, 0x10_3100), (0x10_8800, 0x10_8900), (0x10_c800, 0x10_c900)]).unwrap().unwrap();
        assert_eq!((p.fence, p.flights), (5, 1));
        let back: Vec<(usize, usize)> = p.back.iter().map(|p| (p.1, p.2)).collect();
        assert_eq!(back, [(0x10_3000, 0x10_3100)]);
        // What `picked` found, alone: waited for, nothing brought back.
        let p = plan(&line, &[(0x10_8800, 0x10_8900)]).unwrap().unwrap();
        assert_eq!(p.fence, 5);
        assert!(p.back.is_empty());
        // What it only looks at is no flight's write.
        assert!(plan(&line, &[(0x10_c800, 0x10_c900)]).unwrap().is_none());
    }

    /// What cannot be brought back from a copy takes the old wait.
    #[test]
    fn what_cannot_be_brought_back_waits_for_the_landings() {
        let (a, b) = (copy(0x10_0000, 0x10_0000), copy(0x10_0000, 0x1000));
        // A texture read back over it; a value written there behind a flight.
        let line = [flight(5, Vec::new(), vec![(0x10_0000, 0x10_1000)])];
        assert!(plan(&line, &[(0x10_0800, 0x10_0900)]).is_err());
        let line = [flight(5, vec![(a.clone(), vec![(0x10_0000, 0x10_2000)])], Vec::new()), Late::Write(0x10_0880, crate::queue::Written::Value(1), 8)];
        assert!(plan(&line, &[(0x10_0800, 0x10_0900)]).is_err());
        assert!(plan(&line, &[(0x10_1800, 0x10_1900)]).unwrap().is_some());
        // Two copies of the same title memory.
        let line = [
            flight(5, vec![(a.clone(), vec![(0x10_0000, 0x10_2000)])], Vec::new()),
            flight(6, vec![(b.clone(), vec![(0x10_0000, 0x10_1000)])], Vec::new()),
        ];
        assert!(plan(&line, &[(0x10_0800, 0x10_0900)]).is_err());
        // ... but a read of what one of them alone wrote is brought back.
        let p = plan(&line, &[(0x10_1800, 0x10_1900)]).unwrap().unwrap();
        assert!(Arc::ptr_eq(&p.back[0].0, &a));
        // More than the most.
        let line = [flight(5, vec![(copy(0x1000_0000, 64 << 20), vec![(0x1000_0000, 0x1400_0000)])], Vec::new())];
        assert!(plan(&line, &[(0x1000_0000, 0x1000_0000 + MOST + 1)]).is_err());
        // A copy in the PC's memory without its snapshot: refused before
        // anything is waited for.
        let line = [flight(5, vec![(a.clone(), vec![(0x10_0000, 0x10_2000)])], Vec::new())];
        let p = plan(&line, &[(0x10_0800, 0x10_0900)]).unwrap().unwrap();
        assert!(!a.snapshot_held() && p.fits().is_err());
    }

    /// Parts are cut where 64 KiB blocks of title memory end, in the order
    /// of title memory, whatever copy they are of.
    #[test]
    fn parts_are_cut_at_blocks_in_the_order_of_title_memory() {
        let (a, b) = (copy(0x10_0000, 0x10_0000), copy(0x30_0000, 0x1000));
        let parts: Vec<Part> = vec![(b.clone(), 0x30_0100, 0x30_0200), (a.clone(), 0x10_ff00, 0x12_0100)];
        let refs: Vec<&Part> = parts.iter().collect();
        assert_eq!(
            blocks(&refs),
            [(1, 0x10_ff00, 0x11_0000), (1, 0x11_0000, 0x12_0000), (1, 0x12_0000, 0x12_0100), (0, 0x30_0100, 0x30_0200)]
        );
    }

    /// What steps read of what flights write, and the steps that take the
    /// old wait.
    #[test]
    fn the_reads_of_steps() {
        let written = [(0x1000, 0x2000), (0x8000, 0x9000)];
        let fill = |src: usize, len: usize| Step::Fill(0x50_0000, src, len, None);
        let steps = [
            fill(0x1800, 0x100),
            fill(0x3000, 0x100),
            Step::Copy(0x40_0000, 0x8f00, 0x200),
            Step::Copy(0x40_0000, 0x4000, 0x200),
            Step::Host(Box::new(|| {}), (0x1000, 0x2000)),
        ];
        let (mut out, mut placed) = (Vec::new(), false);
        needs_of(&written, &steps, &mut out, &mut placed).unwrap();
        assert_eq!(out, [(0x1800, 0x1900), (0x8f00, 0x9100)]);
        assert!(!placed);
        // A copy into what flights write, a step of ours.
        let into = [Step::Copy(0x1f00, 0x40_0000, 0x200)];
        assert!(needs_of(&written, &into, &mut Vec::new(), &mut placed).is_err());
        let run = [fill(0x3000, 1), Step::Run(Box::new(|| {}))];
        assert!(needs_of(&written, &run, &mut Vec::new(), &mut placed).is_err());
    }

    /// The whole way, in a process of its own with the relay on:
    /// `VICTORY_RELAY=2 VICTORY_RELAY_COPY=1 VICTORY_CARD_STEPS=2
    /// VICTORY_CARD_BUFFERS_MIN_KB=1024 cargo test -p victory-gpu --lib host::reads -- --ignored
    /// --test-threads=1` (on the card, or on WARP with `VICTORY_D3D12_WARP=1`
    /// in a diagnostics build). A flight's "shaders" write two stretches of
    /// two title buffers' copies, one in the card's memory and one in the
    /// PC's; the flight is not landed ([`HOLD`]). A submission whose fills
    /// read one of the stretches of each (fills the card cannot do) goes on
    /// once the card has run the flight: what they read is brought back into
    /// title memory and filled, the other stretch is not, and the landing
    /// after it leaves title memory as it would have alone.
    ///
    /// Without the wait per fence (`VICTORY_READ_WAIT=0`) the submission
    /// waits for the landing, which does not come until the test lets it:
    /// it fails.
    #[test]
    #[ignore = "wants VICTORY_RELAY=2 VICTORY_RELAY_COPY=1 VICTORY_CARD_BUFFERS_MIN_KB=1024 and a process of its own"]
    fn a_read_goes_on_when_the_card_has_run_what_it_reads_not_when_it_lands() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
        const BIG: usize = 2 << 20;
        const SMALL: usize = 512 << 10;
        let Some(host) = host() else {
            return;
        };
        assert!(relay_exact() && relay_copy(), "set VICTORY_RELAY=2 VICTORY_RELAY_COPY=1");
        settle_now(WHY_SYNC);
        let (title, start) = {
            // SAFETY: memory of our own, never freed.
            let base = unsafe { VirtualAlloc(std::ptr::null(), BIG + SMALL, MEM_RESERVE | MEM_COMMIT | 0x20_0000, PAGE_READWRITE) }
                .cast::<u8>();
            assert!(!base.is_null());
            // SAFETY: the bytes just committed.
            (unsafe { std::slice::from_raw_parts_mut(base, BIG + SMALL) }, base as usize)
        };
        let seed = |i: usize| (i * 7 + i / 251) as u8;
        for (i, b) in title.iter_mut().enumerate() {
            *b = seed(i);
        }
        crate::shadow::register(0x5151, start as u64, BIG as u64);
        crate::shadow::register(0x5252, (start + BIG) as u64, SMALL as u64);
        let carded = crate::shadow::find(start as u64, 64).unwrap();
        let pc = crate::shadow::find((start + BIG) as u64, 64).unwrap();
        assert!(carded.carded(), "set VICTORY_CARD_BUFFERS_MIN_KB=1024");
        assert!(!pc.carded());
        // What the flight's "shaders" write: 300 bytes read next, 100 not.
        let (upload, data) = host.upload_buffer(4096).unwrap();
        let wrote = |i: usize| 0x80 | (i * 31 + 5) as u8;
        for i in 0..400 {
            // SAFETY: inside the mapping.
            unsafe { data.add(i).write(wrote(i)) };
        }
        HOLD.store(true, Ordering::Release);
        let a = host.new_list().unwrap();
        for (copy, at, len) in [(&carded, start, BIG), (&pc, start + BIG, SMALL)] {
            copy.refresh(&a, at as u64, len as u64);
            copy.gpu_writes(&a, at as u64, len as u64);
            // SAFETY: an open list; live buffers.
            unsafe {
                a.list.CopyBufferRegion(&copy.buffer, copy.offset(at as u64 + 4500), &upload, 0, 300);
                a.list.CopyBufferRegion(&copy.buffer, copy.offset(at as u64 + 9000), &upload, 300, 100);
                a.list.ResourceBarrier(&[transition(&copy.buffer, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON)]);
            }
        }
        a.keep(&upload);
        a.close();
        let flown = host.execute_own(&a);
        // A submission that fills upload memory from both stretches read.
        let (filled, into) = host.upload_buffer(4096).unwrap();
        let c = host.new_list().unwrap();
        // SAFETY: inside the mapping.
        unsafe {
            c.defer_fill(into, (start + 4400) as *const u8, 600);
            c.defer_fill(into.add(1024), (start + BIG + 4400) as *const u8, 600);
        }
        c.keep(&filled);
        c.close();
        // If it waits for the landing, the landing is let come after a
        // while, and the test fails.
        let (went, told) = std::sync::mpsc::channel::<()>();
        let late = std::thread::spawn(move || {
            let late = told.recv_timeout(std::time::Duration::from_secs(30)).is_err();
            HOLD.store(false, Ordering::Release);
            RELAY_MOVED.notify_all();
            late
        });
        let sent = host.execute_own(&c);
        let landed = FINISHED.load(Ordering::Acquire);
        let _ = went.send(());
        assert!(!late.join().unwrap(), "the read waited for the landing");
        assert!(sent > flown && landed < flown, "the flight landed before the read went on");
        // What was filled: the title's bytes with what the flight wrote.
        let wanted: Vec<u8> = (4400..5000).map(|i| if (4500..4800).contains(&i) { wrote(i - 4500) } else { seed(i) }).collect();
        // SAFETY: inside the mapping, filled when the submission was prepared.
        let (one, two) = unsafe { (std::slice::from_raw_parts(into, 600), std::slice::from_raw_parts(into.add(1024), 600)) };
        assert!(one == &wanted[..], "the fill from the copy in the card's memory");
        let wanted_too: Vec<u8> = (4400..5000)
            .map(|i| if (4500..4800).contains(&i) { wrote(i - 4500) } else { seed(BIG + i) })
            .collect();
        assert!(two == &wanted_too[..], "the fill from the copy in the PC's memory");
        // Only what was read is in title memory before the landing.
        for base in [0, BIG] {
            assert!(title[base + 9000..base + 9100].iter().enumerate().all(|(i, &b)| b == seed(base + 9000 + i)), "brought back too soon");
        }
        // The landing: what it brings, and nothing over what was brought.
        settle_now(WHY_SYNC);
        assert!(FINISHED.load(Ordering::Acquire) >= sent);
        for (i, &b) in title.iter().enumerate() {
            let at = if i >= BIG { i - BIG } else { i };
            let want = match at {
                4500..4800 => wrote(at - 4500),
                9000..9100 => wrote(300 + at - 9000),
                _ => seed(i),
            };
            assert_eq!(b, want, "title memory at {i:#x} after the landing");
        }
        assert_eq!(host.log_messages(), None);
        drop((a, c));
    }

    /// Of [`two_flights`]: a buffer whose copy is in the card's memory, two
    /// whose copies are in the PC's.
    const BIG: usize = 2 << 20;
    const SMALL: usize = 512 << 10;
    /// Where the three buffers lie in title memory, from the first.
    const BASES: [usize; 3] = [0, BIG, BIG + SMALL];
    /// What a title thread writes over what was read, once it was brought
    /// back (of each of the first two buffers).
    const TITLE_WRITES: std::ops::Range<usize> = 4600..4616;

    /// Title memory's bytes before anything is written.
    fn seed(i: usize) -> u8 {
        (i * 7 + i / 251) as u8
    }

    /// What a flight's "shaders" write: 300 bytes at 4500 of a buffer, then
    /// 100 at 9000.
    fn wrote(i: usize) -> u8 {
        0x80 | (i * 31 + 5) as u8
    }

    /// Title memory once everything has landed (offset `i` from the first
    /// buffer): what flights wrote, and over it what the title wrote.
    fn landed(i: usize) -> u8 {
        let base = BASES.iter().rposition(|&b| b <= i).unwrap();
        let at = i - BASES[base];
        match at {
            _ if base < 2 && TITLE_WRITES.contains(&at) => 0x5a,
            4500..4800 => wrote(at - 4500),
            9000..9100 => wrote(300 + at - 9000),
            _ => seed(i),
        }
    }

    /// A list whose "shaders" write, in each copy (with the title memory of
    /// its whole range, declared), 300 bytes of `upload` at 4500 and the
    /// next 100 at 9000.
    fn writes(host: &Host, upload: &ID3D12Resource, copies: &[(&Arc<Shadow>, usize, usize)]) -> HostList {
        let list = host.new_list().unwrap();
        for &(copy, at, len) in copies {
            copy.refresh(&list, at as u64, len as u64);
            copy.gpu_writes(&list, at as u64, len as u64);
            // SAFETY: an open list; live buffers.
            unsafe {
                list.list.CopyBufferRegion(&copy.buffer, copy.offset(at as u64 + 4500), upload, 0, 300);
                list.list.CopyBufferRegion(&copy.buffer, copy.offset(at as u64 + 9000), upload, 300, 100);
                list.list.ResourceBarrier(&[transition(&copy.buffer, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON)]);
            }
        }
        list.keep(upload);
        list.close();
        list
    }

    /// A fence the card's queue waits for, opened from another thread.
    struct Gate(ID3D12Fence);
    // SAFETY: a fence (free-threaded).
    unsafe impl Send for Gate {}

    /// What a read saw once it had brought its bytes back: how far flights
    /// had landed, whether the first flight's bytes were in title memory,
    /// and whether the second's that were not read were not. Then a title
    /// thread writes over what was read ([`TITLE_WRITES`]).
    type Seen = Option<(u64, bool, bool)>;

    /// Two flights in the air, and a submission whose fills read 600 bytes
    /// of each of the first two buffers ([`BASES`], from `ids`, from 4400 on;
    /// fills the card's steps cannot do) of what the second flight alone
    /// declared: the first writes the third buffer, the second the first two
    /// ([`writes`]). The read goes on once it has brought back what it read
    /// ([`Seen`]); the test checks what was filled, and title memory once
    /// everything has landed ([`landed`]).
    ///
    /// `held`: the thread that lands flights takes none ([`HOLD`]) until the
    /// read has waited 2 s, else none for good. Else it lands beside the read
    /// as it does in a run: the card stands at a gate until the read waits
    /// for it, and the read, woken by the card, looks at the line only once
    /// that thread has taken what it may.
    ///
    /// The numbers of the two flights, what the read saw, and whether the
    /// landing was let come late.
    fn two_flights(host: &Host, ids: [usize; 3], held: bool) -> (u64, u64, Seen, bool) {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
        assert!(relay_exact() && relay_copy(), "set VICTORY_RELAY=2 VICTORY_RELAY_COPY=1");
        settle_now(WHY_SYNC);
        let all = BIG + 2 * SMALL;
        // SAFETY: memory of our own, never freed, read and written by
        // address from here on (as the title's threads do).
        let start = unsafe {
            let base = VirtualAlloc(std::ptr::null(), all, MEM_RESERVE | MEM_COMMIT | 0x20_0000, PAGE_READWRITE).cast::<u8>();
            assert!(!base.is_null());
            for i in 0..all {
                base.add(i).write(seed(i));
            }
            base as usize
        };
        // SAFETY: title memory of the test's own.
        let at = move |i: usize| unsafe { ((start + i) as *const u8).read_volatile() };
        let lens = [BIG, SMALL, SMALL];
        for k in 0..3 {
            crate::shadow::register(ids[k], (start + BASES[k]) as u64, lens[k] as u64);
        }
        let copies: [Arc<Shadow>; 3] = std::array::from_fn(|k| crate::shadow::find((start + BASES[k]) as u64, 64).unwrap());
        assert!(copies[0].carded(), "set VICTORY_CARD_BUFFERS_MIN_KB=1024");
        assert!(!copies[1].carded() && !copies[2].carded());
        let range = |k: usize| (&copies[k], start + BASES[k], lens[k]);
        let (upload, data) = host.upload_buffer(4096).unwrap();
        for i in 0..400 {
            // SAFETY: inside the mapping.
            unsafe { data.add(i).write(wrote(i)) };
        }
        let gate = (!held).then(|| {
            // SAFETY: plain object creation; the queue's wait for it.
            unsafe {
                let gate: ID3D12Fence = host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
                host.queue.Wait(&gate, 1).unwrap();
                Gate(gate)
            }
        });
        HOLD.store(held, Ordering::Release);
        let a = writes(host, &upload, &[range(2)]);
        let first = host.execute_own(&a);
        let b = writes(host, &upload, &[range(0), range(1)]);
        let second = host.execute_own(&b);
        assert!(first > 0 && second > first);
        let (filled, into) = host.upload_buffer(4096).unwrap();
        let c = host.new_list().unwrap();
        // SAFETY: inside the mapping.
        unsafe {
            c.defer_fill(into, (start + 4400) as *const u8, 600);
            c.defer_fill(into.add(1024), (start + BIG + 4400) as *const u8, 600);
        }
        c.keep(&filled);
        c.close();
        let seen = Arc::new(Mutex::new(None));
        let saw = seen.clone();
        *BROUGHT.lock().unwrap() = Some(Box::new(move || {
            let finished = FINISHED.load(Ordering::Acquire);
            let ahead = (4500..4800).all(|i| at(BASES[2] + i) == wrote(i - 4500));
            let rest = [0, BIG].iter().all(|&b| (9000..9100).all(|i| at(b + i) == seed(b + i)));
            for b in [0, BIG] {
                for i in TITLE_WRITES {
                    // SAFETY: title memory of the test's own.
                    unsafe { ((start + b + i) as *mut u8).write_volatile(0x5a) };
                }
            }
            *saw.lock().unwrap() = Some((finished, ahead, rest));
        }));
        let (went, told) = std::sync::mpsc::channel::<()>();
        let late = std::thread::spawn(move || {
            match gate {
                Some(gate) => {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    // SAFETY: a plain signal.
                    unsafe { gate.0.Signal(1) }.unwrap();
                    false
                }
                None => {
                    let late = told.recv_timeout(std::time::Duration::from_secs(2)).is_err();
                    HOLD.store(false, Ordering::Release);
                    RELAY_MOVED.notify_all();
                    late
                }
            }
        });
        LOOK_LATE_MS.store(if held { 0 } else { 250 }, Ordering::Relaxed);
        let sent = host.execute_own(&c);
        LOOK_LATE_MS.store(0, Ordering::Relaxed);
        let _ = went.send(());
        let late = late.join().unwrap();
        *BROUGHT.lock().unwrap() = None;
        let seen = seen.lock().unwrap().take();
        // What was filled: title memory once it was brought back.
        // SAFETY: inside the mapping, filled when the submission was prepared.
        let (one, two) = unsafe { (std::slice::from_raw_parts(into, 600), std::slice::from_raw_parts(into.add(1024), 600)) };
        if seen.is_some() {
            assert!(one.iter().zip(4400..).all(|(&b, i)| b == landed(i)), "the fill from the copy in the card's memory");
            assert!(two.iter().zip(BIG + 4400..).all(|(&b, i)| b == landed(i)), "the fill from the copy in the PC's memory");
        }
        settle_now(WHY_SYNC);
        assert!(FINISHED.load(Ordering::Acquire) >= sent);
        if seen.is_some() {
            for i in 0..all {
                assert_eq!(at(i), landed(i), "title memory at {i:#x} once everything landed");
            }
        }
        assert_eq!(host.log_messages(), None);
        drop((a, b, c, upload, filled));
        (first, second, seen, late)
    }

    /// [`two_flights`] with the thread that lands flights at work beside
    /// the read, as in a run (`VICTORY_RELAY=2 VICTORY_RELAY_COPY=1
    /// VICTORY_CARD_STEPS=2 VICTORY_CARD_BUFFERS_MIN_KB=1024`, a process of
    /// its own). The card runs both flights at once, and the read and that
    /// thread wake together: it lands the first, and not the second until
    /// the read has brought its bytes back. A title thread's write over them
    /// then outlives the second's landing.
    ///
    /// Without [`landing_limit`] that thread lands both while the read
    /// looks, and the read brings nothing back: it waited for the landing.
    #[test]
    #[ignore = "wants VICTORY_RELAY=2 VICTORY_RELAY_COPY=1 VICTORY_CARD_BUFFERS_MIN_KB=1024 and a process of its own"]
    fn a_read_goes_on_before_the_flight_it_reads_lands_while_the_ones_ahead_land() {
        let Some(host) = host() else {
            return;
        };
        let (first, second, seen, _) = two_flights(host, [0x5353, 0x5454, 0x5555], false);
        let (finished, ahead, rest) = seen.expect("the read waited for the landing of the flight it reads: it brought nothing back");
        assert!(finished < second, "the flight read had landed when the read brought its bytes back");
        assert!(finished >= first && ahead, "the flight ahead had not landed when the read brought bytes back");
        assert!(rest, "brought back more than was read");
    }

    /// [`two_flights`] with the thread that lands flights held until the
    /// read has waited a while (`HOLD`; switches as above). The card has run
    /// both flights, and the read waits for the first to land before it
    /// brings anything of the second back: the second's bytes never reach
    /// title memory before the first's.
    ///
    /// Without the wait for the flights ahead the read brings its bytes back
    /// at once, with the first flight still in the line.
    #[test]
    #[ignore = "wants VICTORY_RELAY=2 VICTORY_RELAY_COPY=1 VICTORY_CARD_BUFFERS_MIN_KB=1024 and a process of its own"]
    fn a_read_brings_bytes_back_only_once_the_flights_ahead_have_landed() {
        let Some(host) = host() else {
            return;
        };
        let (first, second, seen, late) = two_flights(host, [0x5656, 0x5757, 0x5858], true);
        let (finished, ahead, rest) = seen.expect("the read brought nothing back");
        assert!(finished >= first && ahead, "bytes of a flight were brought back before the flight ahead of it had landed");
        assert!(late, "the read went on before the flight ahead had landed");
        assert!(finished < second && rest);
    }

    /// The thread that lands flights takes none while a read brings bytes
    /// back.
    #[test]
    fn no_landing_while_a_read_brings_bytes_back() {
        let mut line = Line {
            line: vec![flight(5, Vec::new(), Vec::new())],
            written: Vec::new(),
            lists: 0,
            target: 0,
            settling: false,
            poke: false,
            landed: Vec::new(),
            in_hand: Vec::new(),
            landing: Vec::new(),
            reading: false,
            hold: 0,
        };
        assert!(may_land(&line) || held());
        line.reading = true;
        assert!(!may_land(&line));
        line.reading = false;
        line.line.clear();
        assert!(!may_land(&line));
    }

    /// While a read waits for the flight of fence 6, the flight of 5 and
    /// what follows it land as soon as the card has run them; the flight of
    /// 6 does not, even run, and lands once the read lets it.
    #[test]
    fn what_is_ahead_of_the_flight_a_read_waits_for_lands_and_not_it() {
        // Flight 5, its write, flight 6, its write, flight 7.
        let line = [Some(5), None, Some(6), None, Some(7)];
        assert_eq!(landable(&line, landing_limit(4, 6)), 0);
        assert_eq!(landable(&line, landing_limit(5, 6)), 2);
        assert_eq!(landable(&line, landing_limit(7, 6)), 2);
        assert_eq!(landable(&line[2..], landing_limit(7, 6)), 0);
        // Let go.
        assert_eq!(landable(&line[2..], landing_limit(7, 0)), 3);
        assert_eq!(landable(&line, landing_limit(6, 0)), 4);
    }
}
