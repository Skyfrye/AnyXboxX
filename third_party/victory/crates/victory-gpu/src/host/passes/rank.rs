//! What the stamps of timed frames add up to ([`super`]): each span put
//! down to its work, the works ranked by time, and the line that says so.

use std::fmt::Write as _;

use super::{Key, Rows, Work};

/// What a draw drew into: the first render target's format and size (or
/// the depth target's, when there is no render target), and how many
/// render targets. Zero for dispatches.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Target {
    pub(crate) format: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) count: u8,
    pub(crate) depth: bool,
}

impl Target {
    /// As the line says it ("rgba16f 2560x1440 x4"; "d32 2560x1440" for a
    /// depth target alone); empty for none.
    pub(super) fn text(self) -> String {
        if self.format == 0 && self.width == 0 {
            return String::new();
        }
        let mut text = format!("{} {}x{}", format_name(self.format), self.width, self.height);
        if self.count > 1 {
            let _ = write!(text, " x{}", self.count);
        }
        if self.depth && self.count > 0 {
            text.push_str(" +depth");
        }
        text
    }
}

/// Short names of the formats render and depth targets have.
fn format_name(format: u32) -> String {
    let name = match format {
        2 => "rgba32f",
        10 => "rgba16f",
        11 => "rgba16",
        13 => "rgba16s",
        16 => "rg32f",
        20 => "d32s8",
        24 => "rgb10a2",
        26 => "r11g11b10f",
        28 => "rgba8",
        29 => "rgba8srgb",
        34 => "rg16f",
        35 => "rg16",
        40 => "d32",
        41 => "r32f",
        42 => "r32u",
        45 => "d24s8",
        49 => "rg8",
        54 => "r16f",
        55 => "d16",
        56 => "r16",
        57 => "r16u",
        61 => "r8",
        87 => "bgra8",
        91 => "bgra8srgb",
        _ => return format!("format {format}"),
    };
    name.to_owned()
}

/// The span of each of a list's segments: from its stamp (`starts`) to the
/// next one's, the last to `end`, in ticks; and how many ran backwards or
/// longer than `most` (counted 0). `None` if the list has not run: a stamp
/// still 0.
pub(super) fn spans(starts: &[u32], end: u32, stamp: impl Fn(u32) -> u64, most: u64) -> Option<(Vec<u64>, u64)> {
    let ticks: Vec<u64> = starts.iter().copied().chain([end]).map(&stamp).collect();
    if ticks.contains(&0) {
        return None;
    }
    let mut odd = 0;
    let spans = ticks
        .windows(2)
        .map(|w| match w[1].checked_sub(w[0]).filter(|&t| t < most) {
            Some(t) => t,
            None => {
                odd += 1;
                0
            }
        })
        .collect();
    Some((spans, odd))
}

/// One work's sums: its ticks, draws or dispatches, indirect executions
/// and spans, and its ticks by target (the first [`TARGETS`] targets).
pub(super) struct Row {
    pub(super) work: Work,
    pub(super) ticks: u64,
    pub(super) calls: u64,
    pub(super) indirect: u64,
    pub(super) spans: u64,
    pub(super) targets: Vec<(Target, u64)>,
}

/// Targets a row keeps apart.
const TARGETS: usize = 8;

impl Row {
    /// The target it spent the most time drawing into, and how many others
    /// it drew into.
    fn target(&self) -> (Target, usize) {
        let best = self.targets.iter().max_by_key(|t| t.1).map_or(Target::default(), |t| t.0);
        (best, self.targets.len().saturating_sub(1))
    }
}

/// What the timed frames read since the last line add up to.
#[derive(Default)]
pub(super) struct Totals {
    /// Timed frames with spans read.
    pub(super) frames: u64,
    pub(super) rows: Rows,
    /// Spans read, and their ticks.
    pub(super) spans: u64,
    pub(super) ticks: u64,
    /// Submissions with timed lists, and their ticks.
    pub(super) submissions: u64,
    pub(super) whole: u64,
    /// Lists of timed frames; of those not run (a stamp never written),
    /// and those whose stamps another list took or wrote over.
    pub(super) lists: u64,
    pub(super) not_run: u64,
    pub(super) overwritten: u64,
    /// Spans that ran backwards or too long (counted 0).
    pub(super) odd: u64,
    /// Lists and submissions that came after their frame was read.
    pub(super) late: u64,
}

impl Totals {
    /// A span of `work` into `target`: `ticks`, with `calls` draws or
    /// dispatches, `indirect` of them indirect.
    pub(super) fn add(&mut self, work: &Work, target: Target, ticks: u64, calls: u32, indirect: u32) {
        self.spans += 1;
        self.ticks += ticks;
        let row = self.rows.entry(work.key()).or_insert_with(|| Row {
            work: work.clone(),
            ticks: 0,
            calls: 0,
            indirect: 0,
            spans: 0,
            targets: Vec::new(),
        });
        row.ticks += ticks;
        row.calls += u64::from(calls);
        row.indirect += u64::from(indirect);
        row.spans += 1;
        match row.targets.iter().position(|t| t.0 == target) {
            Some(at) => row.targets[at].1 += ticks,
            None if row.targets.len() < TARGETS => row.targets.push((target, ticks)),
            None => {}
        }
    }

    /// The rows by time, longest first (rows of equal time by name).
    pub(super) fn ranked(&self) -> Vec<(&Key, &Row)> {
        let mut rows: Vec<(&Key, &Row, String)> = self.rows.iter().map(|(k, r)| (k, r, r.work.name())).collect();
        rows.sort_by(|a, b| b.1.ticks.cmp(&a.1.ticks).then_with(|| a.2.cmp(&b.2)));
        rows.into_iter().map(|(k, r, _)| (k, r)).collect()
    }
}

/// The line: per timed frame, what the submissions with timed lists took
/// and how much of it the spans cover, then the `top` works by time with
/// their share of the frame's time (the submissions', or the spans' when
/// no submission was read), all of their time, their draws or dispatches,
/// what their programs are (`props`) and what they drew into.
pub(super) fn text(totals: &Totals, per_ms: f64, every: u64, top: usize, props: impl Fn(&Work) -> String) -> String {
    let frames = totals.frames.max(1) as f64;
    let ms = |ticks: u64| ticks as f64 / per_ms;
    let (spans, whole) = (ms(totals.ticks) / frames, ms(totals.whole) / frames);
    let of = if whole > 0.0 { whole } else { spans };
    let covered = if whole > 0.0 {
        format!(
            "submissions with timed lists {whole:.2} ms ({:.1} of them), the spans {spans:.2} ms of it ({:.1}% covered)",
            totals.submissions as f64 / frames,
            100.0 * spans / whole
        )
    } else {
        format!("no submission read, the spans {spans:.2} ms")
    };
    let mut text = format!(
        "the title's work on the card by pipeline, per timed frame (VICTORY_PASS_TIMES=1, 1 frame in {every}, {} frames read): \
         {covered} in {:.0} spans of {:.1} lists ({:.1} not run, {:.1} overwritten, {} spans out of order, {} late); by ms a frame:",
        totals.frames,
        totals.spans as f64 / frames,
        totals.lists as f64 / frames,
        totals.not_run as f64 / frames,
        totals.overwritten as f64 / frames,
        totals.odd,
        totals.late,
    );
    let ranked = totals.ranked();
    for (n, (_, row)) in ranked.iter().take(top).enumerate() {
        let each = ms(row.ticks) / frames;
        let _ = write!(
            text,
            " #{} {} {each:.2} ms {:.1}% ({:.1} ms in all) {:.1} calls",
            n + 1,
            row.work.name(),
            if of > 0.0 { 100.0 * each / of } else { 0.0 },
            ms(row.ticks),
            row.calls as f64 / frames,
        );
        if row.indirect > 0 {
            let _ = write!(text, " ({:.1} indirect)", row.indirect as f64 / frames);
        }
        let described = props(&row.work);
        if !described.is_empty() {
            let _ = write!(text, " [{described}]");
        }
        let (target, others) = row.target();
        let target = target.text();
        if !target.is_empty() {
            let _ = write!(text, " {target}");
            if others > 0 {
                let _ = write!(text, " (+{others} targets)");
            }
        }
        text.push(';');
    }
    let rest: u64 = ranked.iter().skip(top).map(|(_, r)| r.ticks).sum();
    let _ = write!(
        text,
        " the other {} {:.2} ms {:.1}%",
        ranked.len().saturating_sub(top),
        ms(rest) / frames,
        if of > 0.0 { 100.0 * ms(rest) / frames / of } else { 0.0 },
    );
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each segment's span runs from its stamp to the next one's, the last
    /// to the list's end; spans of one work add up.
    #[test]
    fn each_span_is_put_down_to_the_work_that_began_it() {
        // Stamps 0..4 at ticks 100, 150, 400, 420, 425: A, B, A, then end.
        let stamps = [100u64, 150, 400, 420];
        let (spans, odd) = spans(&[0, 1, 2], 3, |at| stamps[at as usize], 1 << 20).unwrap();
        assert_eq!((spans.clone(), odd), (vec![50, 250, 20], 0));
        let mut totals = Totals::default();
        let target = Target { format: 10, width: 2560, height: 1440, count: 1, depth: false };
        for (work, ticks) in [(Work::Ours("a"), spans[0]), (Work::Copies, spans[1]), (Work::Ours("a"), spans[2])] {
            totals.add(&work, target, ticks, 2, 1);
        }
        let a = &totals.rows[&Key::Ours("a")];
        assert_eq!((a.ticks, a.calls, a.indirect, a.spans), (70, 4, 2, 2));
        assert_eq!(totals.rows[&Key::Copies].ticks, 250);
        assert_eq!((totals.spans, totals.ticks), (3, 320));
    }

    /// A list whose stamps are not all written has not run: nothing of it
    /// is read. A span that runs backwards, or past the limit, counts 0 and
    /// is counted out of order.
    #[test]
    fn a_list_not_run_is_not_read_and_odd_spans_are_counted() {
        let unwritten = [100u64, 0, 300];
        assert!(spans(&[0, 1], 2, |at| unwritten[at as usize], 1 << 20).is_none());
        let backwards = [100u64, 90, 300, 1 << 30];
        let (spans, odd) = spans(&[0, 1, 2], 3, |at| backwards[at as usize], 1 << 20).unwrap();
        assert_eq!((spans, odd), (vec![0, 210, 0], 2));
    }

    /// Rows come longest first, ties by name; the line ranks the first
    /// `top`, gives each its share of the submissions' time a frame, and
    /// sums the rest.
    #[test]
    fn the_line_ranks_works_by_time_against_the_submissions() {
        let mut totals = Totals::default();
        let none = Target::default();
        totals.add(&Work::Ours("small"), none, 1_000, 1, 0);
        totals.add(&Work::Ours("big"), none, 6_000, 3, 0);
        totals.add(&Work::Copies, none, 1_000, 5, 0);
        totals.add(&Work::Ours("tie"), none, 1_000, 1, 0);
        let names: Vec<String> = totals.ranked().iter().map(|(_, r)| r.work.name()).collect();
        assert_eq!(names, ["ours: big", "ours: small", "ours: tie", "the title's copies, clears and resolves"]);
        // Two frames read; the submissions took 10,000 ticks a frame, at
        // 1,000 ticks a millisecond: 10 ms, of which the spans 4.5 ms.
        (totals.frames, totals.submissions, totals.whole) = (2, 4, 20_000);
        let text = text(&totals, 1_000.0, 30, 2, |w| if matches!(w, Work::Ours("big")) { "w64 paired".into() } else { String::new() });
        assert!(text.contains("submissions with timed lists 10.00 ms (2.0 of them), the spans 4.50 ms of it (45.0% covered) in 2 spans"), "{text}");
        assert!(text.contains(" #1 ours: big 3.00 ms 30.0% (6.0 ms in all) 1.5 calls [w64 paired];"), "{text}");
        assert!(text.contains(" #2 ours: small 0.50 ms 5.0% (1.0 ms in all) 0.5 calls;"), "{text}");
        assert!(!text.contains("#3"), "{text}");
        assert!(text.ends_with(" the other 2 1.00 ms 10.0%"), "{text}");
    }

    /// A row names the target it spent the most time on, and how many
    /// others it drew into.
    #[test]
    fn a_row_names_the_target_it_spent_the_most_on() {
        let mut totals = Totals::default();
        let small = Target { format: 28, width: 640, height: 360, count: 1, depth: false };
        let big = Target { format: 10, width: 2560, height: 1440, count: 4, depth: true };
        totals.add(&Work::Ours("x"), small, 10, 1, 0);
        totals.add(&Work::Ours("x"), big, 30, 1, 0);
        totals.add(&Work::Ours("x"), small, 10, 1, 0);
        let row = &totals.rows[&Key::Ours("x")];
        assert_eq!(row.target(), (big, 1));
        assert_eq!(big.text(), "rgba16f 2560x1440 x4 +depth");
        let depth = Target { format: 40, width: 2048, height: 2048, count: 0, depth: true };
        assert_eq!(depth.text(), "d32 2048x2048");
        assert_eq!(Target::default().text(), "");
        assert_eq!(format_name(999), "format 999");
    }
}
