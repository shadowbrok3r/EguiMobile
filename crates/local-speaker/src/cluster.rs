//! Deciding how many voices there are, and which windows are whose.
//!
//! The hard part is the count. Two prints of one voice are about as alike in a quiet room as
//! prints of two different voices are across a table from the phone, so no fixed "this alike
//! means the same person" survives a change of room. What does survive is a ratio: how alike
//! two groups of windows are to each other, measured against how alike each group is to
//! itself. One voice split in two scores near 1 whatever the microphone; two voices rarely
//! pass 0.5. So the windows are first cut into many small cells, and cells are then merged
//! for as long as that ratio says they are one voice.
//!
//! Two things keep that honest. The network's least confident windows — a breath, a laugh,
//! two people at once — are left out of the count altogether: they are the ones that would
//! found a "speaker" of their own. And a small group has to be far more different than a
//! large one before it stands as a person, because a short stretch of someone sounding unlike
//! themselves is commoner than a guest who says three sentences.
//!
//! The thresholds were set on AMI meeting recordings, close and far microphone, and
//! LibriSpeech; `scripts/speaker-export/reference.py` is the same procedure in numpy, and
//! holds the measurements. When unsure it settles on fewer speakers: a monologue cut into
//! three people is worse than two alike voices given one name.

use crate::prints::Prints;
use crate::DIM;

/// The most voices a recording is ever split into: the number of cells it starts from.
pub const MAX_SPEAKERS: usize = 24;

/// Seconds of real audio below which a window's print is too noisy to count.
const STRONG_SECONDS: f32 = 1.0;
/// Share of the remaining windows left out of the count for being the network's least sure.
/// In double precision, as numpy's `quantile` takes it: 0.2 as an `f32` is a hair over a
/// fifth, which is enough to push the cut past the window it should land on.
const WEAK_SHARE: f64 = 0.2;
/// Windows that count, below which everything is one voice.
const MIN_STRONG: usize = 6;
/// Windows per starting cell, and the fewest a cell may hold.
const PER_CELL: usize = 8;
const MIN_CELL: usize = 3;
const KMEANS_ROUNDS: usize = 25;
/// Two groups scoring this against each other are one voice, whatever their size.
const SAME_VOICE: f32 = 0.65;
/// What a group may score against a larger one and still stand as a person: up to
/// `STANDS_SMALL` for one of `SMALL_SECONDS` or less, `STANDS_BIG` from `BIG_SECONDS` on.
const STANDS_SMALL: f32 = 0.5;
const STANDS_BIG: f32 = 0.6;
const SMALL_SECONDS: f32 = 20.0;
const BIG_SECONDS: f32 = 60.0;
/// A group this much less alike within itself than the rest is a heap of oddments, and is
/// held to the small group's standard however long it is.
const COHERENT: f32 = 0.75;
/// Pairs' worth of the overall likeness mixed into each group's own, so that a group of
/// three windows is not measured against three windows' worth of noise.
const SHRINK: f32 = 20.0;
/// A voice whose windows come to less than this many seconds is not kept apart.
const MIN_SECONDS: f32 = 6.0;
const REFINE_ROUNDS: usize = 2;

/// The speakers of a recording's spans.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Labels {
    /// Speaker of each span, numbered from zero in the order they are first heard; `None`
    /// for a span too short to have had a window.
    pub spans: Vec<Option<u8>>,
    /// How many voices there are.
    pub speakers: usize,
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    // Eight running sums: the compiler may not reorder a float sum itself, and this loop is
    // where the whole grouping spends its time.
    let mut lanes = [0f32; 8];
    let (a_chunks, b_chunks) = (a.chunks_exact(8), b.chunks_exact(8));
    let tail: f32 = a_chunks.remainder().iter().zip(b_chunks.remainder()).map(|(x, y)| x * y).sum();
    for (x, y) in a_chunks.zip(b_chunks) {
        for lane in 0..8 {
            lanes[lane] += x[lane] * y[lane];
        }
    }
    lanes.iter().sum::<f32>() + tail
}

/// Index of the largest value, the first of equals.
fn argmax(values: impl Iterator<Item = f32>) -> usize {
    let mut best = (0, f32::NEG_INFINITY);
    for (index, value) in values.enumerate() {
        if value > best.1 {
            best = (index, value);
        }
    }
    best.0
}

/// splitmix64: the seeding below has to come out the same on every run and in the Python
/// reference, which rules out anything seeded from the clock or tied to a library version.
struct Rng(u64);

impl Rng {
    fn unit(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// The windows as unit vectors, with what was divided out and how long each is.
struct Points {
    unit: Vec<f32>,
    norm: Vec<f32>,
    seconds: Vec<f32>,
}

impl Points {
    fn of(prints: &Prints) -> Self {
        let mut unit = Vec::with_capacity(prints.len() * DIM);
        let mut norm = Vec::with_capacity(prints.len());
        for index in 0..prints.len() {
            let print = prints.print(index);
            let length = dot(print, print).sqrt();
            // A print that is not a number, which only a faulty run of the network gives,
            // becomes one the network was least sure of, pointing nowhere: it shapes nothing
            // and cannot poison a sum.
            if !length.is_finite() {
                norm.push(0.0);
                unit.extend(std::iter::repeat_n(0.0, DIM));
                continue;
            }
            norm.push(length);
            unit.extend(print.iter().map(|v| v / length.max(1e-9)));
        }
        Self { unit, norm, seconds: prints.windows().iter().map(|w| w.seconds()).collect() }
    }

    fn len(&self) -> usize {
        self.norm.len()
    }

    fn at(&self, index: usize) -> &[f32] {
        &self.unit[index * DIM..(index + 1) * DIM]
    }

    /// Unit vector along the mean of `members`.
    fn centre(&self, members: impl Iterator<Item = usize>) -> Vec<f32> {
        let mut sum = vec![0f32; DIM];
        for index in members {
            for (total, value) in sum.iter_mut().zip(self.at(index)) {
                *total += value;
            }
        }
        let length = dot(&sum, &sum).sqrt().max(1e-9);
        sum.iter_mut().for_each(|v| *v /= length);
        sum
    }
}

/// The value below which `share` of `values` fall, interpolated as numpy's `quantile` does.
fn quantile(values: &mut [f32], share: f64) -> f32 {
    values.sort_by(|a, b| a.total_cmp(b));
    let position = share * (values.len() - 1) as f64;
    let (low, high) = (position.floor() as usize, position.ceil() as usize);
    (values[low] as f64 + (values[high] as f64 - values[low] as f64) * (position - low as f64)) as f32
}

/// Spherical k-means over the points `chosen`, seeded k-means++ fashion from a fixed seed.
/// Returns a cell per chosen point.
fn cells(points: &Points, chosen: &[usize], k: usize) -> Vec<usize> {
    let middle = points.centre(chosen.iter().copied());
    let first = argmax(chosen.iter().map(|&i| dot(points.at(i), &middle)));
    let mut centres = vec![points.at(chosen[first]).to_vec()];
    let mut apart: Vec<f32> = chosen.iter().map(|&i| 1.0 - dot(points.at(i), &centres[0])).collect();
    let mut rng = Rng(0x5eed);
    while centres.len() < k {
        // The next seed is drawn with odds that grow with distance from every seed so far.
        let odds: Vec<f64> = apart.iter().map(|d| (d.max(0.0) as f64).powi(2)).collect();
        let total: f64 = odds.iter().sum();
        if total <= 0.0 {
            break;
        }
        let target = rng.unit() * total;
        let mut running = 0.0;
        let pick = odds.iter().position(|odd| {
            running += odd;
            running > target
        });
        let pick = pick.unwrap_or(chosen.len() - 1);
        centres.push(points.at(chosen[pick]).to_vec());
        for (distance, &i) in apart.iter_mut().zip(chosen) {
            *distance = distance.min(1.0 - dot(points.at(i), centres.last().expect("just pushed")));
        }
    }

    let mut cell = vec![0usize; chosen.len()];
    for round in 0..KMEANS_ROUNDS {
        let nearest: Vec<usize> = chosen.iter().map(|&i| argmax(centres.iter().map(|centre| dot(points.at(i), centre)))).collect();
        if round > 0 && nearest == cell {
            break;
        }
        cell = nearest;
        for (index, centre) in centres.iter_mut().enumerate() {
            // A cell that lost every point keeps its seed, and may win some back.
            if cell.iter().any(|&c| c == index) {
                *centre = points.centre(chosen.iter().zip(&cell).filter(|(_, c)| **c == index).map(|(i, _)| *i));
            }
        }
    }
    cell
}

/// A set of windows taken to be one voice.
struct Group {
    /// Sum of its unit vectors.
    sum: Vec<f32>,
    count: usize,
    seconds: f32,
}

impl Group {
    /// Mean likeness between two different members, from the sum alone.
    fn within(&self) -> Option<f32> {
        let n = self.count as f32;
        (self.count > 1).then(|| (dot(&self.sum, &self.sum) - n) / (n * (n - 1.0)))
    }

    fn pairs(&self) -> f32 {
        (self.count * self.count.saturating_sub(1)) as f32 / 2.0
    }

    fn absorb(&mut self, other: Group) {
        for (total, value) in self.sum.iter_mut().zip(&other.sum) {
            *total += value;
        }
        self.count += other.count;
        self.seconds += other.seconds;
    }
}

/// The groups being merged, by the label their windows carry.
struct Groups {
    of: Vec<(usize, Group)>,
}

impl Groups {
    /// Mean likeness within a group, over all of them.
    fn overall(&self) -> f32 {
        let pairs: f32 = self.of.iter().map(|(_, g)| g.pairs()).sum();
        let likeness: f32 = self.of.iter().filter_map(|(_, g)| g.within().map(|w| w * g.pairs())).sum();
        likeness / pairs.max(1.0)
    }

    /// A group's own likeness, steadied by the overall one.
    fn own(&self, index: usize, overall: f32) -> f32 {
        let group = &self.of[index].1;
        ((group.within().unwrap_or(0.0) * group.pairs() + overall * SHRINK) / (group.pairs() + SHRINK)).max(1e-6)
    }

    /// How alike two groups are, against how alike each is to itself: about 1 for one voice
    /// in two halves, seldom over 0.5 for two voices.
    fn ratio(&self, a: usize, b: usize, overall: f32) -> f32 {
        let (x, y) = (&self.of[a].1, &self.of[b].1);
        dot(&x.sum, &y.sum) / (x.count * y.count) as f32 / (self.own(a, overall) * self.own(b, overall)).sqrt()
    }

    /// The most alike pair and its ratio.
    fn closest(&self) -> Option<(f32, usize, usize)> {
        let overall = self.overall();
        let mut best: Option<(f32, usize, usize)> = None;
        for a in 0..self.of.len() {
            for b in a + 1..self.of.len() {
                let ratio = self.ratio(a, b, overall);
                if best.is_none_or(|(r, ..)| ratio > r) {
                    best = Some((ratio, a, b));
                }
            }
        }
        best
    }

    /// Fold group `from` into group `into`, relabelling its windows.
    fn merge(&mut self, into: usize, from: usize, labels: &mut [usize]) {
        let (gone, group) = self.of.remove(from);
        let into = if from < into { into - 1 } else { into };
        let kept = self.of[into].0;
        self.of[into].1.absorb(group);
        labels.iter_mut().filter(|label| **label == gone).for_each(|label| *label = kept);
    }

    /// The smallest group that does not stand apart from a larger one, and the one it
    /// belongs with.
    fn not_standing(&self) -> Option<(usize, usize)> {
        let overall = self.overall();
        let mut order: Vec<usize> = (0..self.of.len()).collect();
        order.sort_by(|a, b| self.of[*a].1.seconds.total_cmp(&self.of[*b].1.seconds));
        for (rank, &index) in order.iter().enumerate().take(order.len().saturating_sub(1)) {
            let group = &self.of[index].1;
            let grown = ((group.seconds - SMALL_SECONDS) / (BIG_SECONDS - SMALL_SECONDS)).clamp(0.0, 1.0);
            let mut needed = STANDS_SMALL + (STANDS_BIG - STANDS_SMALL) * grown;
            if group.within().is_some_and(|within| within < COHERENT * overall) {
                needed = STANDS_SMALL;
            }
            let mut best: Option<(f32, usize)> = None;
            for &other in &order[rank + 1..] {
                let ratio = self.ratio(index, other, overall);
                if best.is_none_or(|(r, _)| ratio > r) {
                    best = Some((ratio, other));
                }
            }
            if let Some((ratio, other)) = best {
                if ratio >= needed {
                    return Some((index, other));
                }
            }
        }
        None
    }
}

/// Reassign every window to the nearest of the current voices' centres, `rounds` times over.
fn refine(points: &Points, labels: &mut [usize], rounds: usize) {
    for _ in 0..rounds {
        let mut ids: Vec<usize> = labels.to_vec();
        ids.sort_unstable();
        ids.dedup();
        let centres: Vec<Vec<f32>> = ids.iter().map(|&id| points.centre((0..labels.len()).filter(|&i| labels[i] == id))).collect();
        let nearest: Vec<usize> = (0..labels.len()).map(|i| ids[argmax(centres.iter().map(|centre| dot(points.at(i), centre)))]).collect();
        if nearest == labels {
            break;
        }
        labels.copy_from_slice(&nearest);
    }
}

/// A voice for every window. `fixed` is the number of voices to find, when it is known.
fn group(points: &Points, fixed: Option<usize>) -> Vec<usize> {
    let n = points.len();
    if fixed == Some(1) {
        return vec![0; n];
    }
    // A print with no length is one that was not a number: it never counts.
    let mut strong: Vec<bool> = (0..n).map(|i| points.seconds[i] >= STRONG_SECONDS && points.norm[i] > 0.0).collect();
    if strong.iter().filter(|s| **s).count() >= MIN_STRONG {
        let mut norms: Vec<f32> = (0..n).filter(|&i| strong[i]).map(|i| points.norm[i]).collect();
        let floor = quantile(&mut norms, WEAK_SHARE);
        strong.iter_mut().zip(&points.norm).for_each(|(s, norm)| *s &= *norm >= floor);
    }
    let wanted = fixed.unwrap_or(0);
    if fixed.is_some() && strong.iter().filter(|s| **s).count() < MIN_STRONG.max(wanted) {
        // Told how many voices there are, a recording too short to count them in is split
        // on everything it has: the count is the listener's word, not this procedure's.
        strong = points.norm.iter().map(|norm| *norm > 0.0).collect();
    }
    let chosen: Vec<usize> = (0..n).filter(|&i| strong[i]).collect();
    if chosen.len() < if fixed.is_some() { 2 } else { MIN_STRONG } {
        return vec![0; n];
    }

    // As many cells as the windows support. Asked for a number of voices, it starts from
    // twice that where there are windows enough — exactly as many cells as voices leaves
    // the seeding no room to put two in one voice — and never from fewer.
    let room = (2 * wanted).min(chosen.len() / 2).max(wanted);
    let k = (chosen.len() / PER_CELL).clamp(2, MAX_SPEAKERS).max(room).min(chosen.len());
    let mut cell = cells(points, &chosen, k);
    // Cells too small to measure are folded into their nearest neighbour first — unless
    // that would leave fewer than were asked for, when every cell is kept.
    let size = |cell: &[usize], id: usize| cell.iter().filter(|c| **c == id).count();
    let mut sound: Vec<usize> = (0..k).filter(|&id| size(&cell, id) >= MIN_CELL).collect();
    if sound.len() < wanted {
        sound = (0..k).filter(|&id| size(&cell, id) > 0).collect();
    }
    if sound.is_empty() {
        sound = vec![(0..k).max_by_key(|&id| size(&cell, id)).unwrap_or(0)];
    }
    let centres: Vec<Vec<f32>> = sound.iter().map(|&id| points.centre(chosen.iter().zip(&cell).filter(|(_, c)| **c == id).map(|(i, _)| *i))).collect();
    for (slot, &i) in cell.iter_mut().zip(&chosen) {
        if !sound.contains(slot) {
            *slot = sound[argmax(centres.iter().map(|centre| dot(points.at(i), centre)))];
        }
    }

    let mut groups = Groups { of: Vec::new() };
    for &id in &sound {
        let mut group = Group { sum: vec![0f32; DIM], count: 0, seconds: 0.0 };
        for (&i, _) in chosen.iter().zip(&cell).filter(|(_, c)| **c == id) {
            group.sum.iter_mut().zip(points.at(i)).for_each(|(total, value)| *total += value);
            group.count += 1;
            group.seconds += points.seconds[i];
        }
        groups.of.push((id, group));
    }

    match fixed {
        Some(count) => {
            while groups.of.len() > count {
                let Some((_, a, b)) = groups.closest() else { break };
                groups.merge(a, b, &mut cell);
            }
        }
        None => {
            // What is plainly one voice first, then every smaller group has to stand apart
            // from the bigger ones.
            while let Some((ratio, a, b)) = groups.closest() {
                if ratio < SAME_VOICE {
                    break;
                }
                groups.merge(a, b, &mut cell);
            }
            while let Some((small, large)) = groups.not_standing() {
                groups.merge(large, small, &mut cell);
            }
        }
    }

    // Everything that was left out of the count goes to the voice it is most like.
    let ids: Vec<usize> = groups.of.iter().map(|(id, _)| *id).collect();
    let centres: Vec<Vec<f32>> = groups.of.iter().map(|(_, g)| { let length = dot(&g.sum, &g.sum).sqrt().max(1e-9); g.sum.iter().map(|v| v / length).collect() }).collect();
    let mut labels = vec![0usize; n];
    for (&i, &id) in chosen.iter().zip(&cell) {
        labels[i] = id;
    }
    for i in (0..n).filter(|&i| !strong[i]) {
        labels[i] = ids[argmax(centres.iter().map(|centre| dot(points.at(i), centre)))];
    }

    if fixed.is_none() {
        // A voice heard for a few seconds in all is not worth a name.
        let seconds = |labels: &[usize], id: usize| (0..n).filter(|&i| labels[i] == id).map(|i| points.seconds[i]).sum::<f32>();
        let mut heard: Vec<usize> = ids.iter().copied().filter(|&id| seconds(&labels, id) >= MIN_SECONDS).collect();
        if heard.is_empty() {
            heard = vec![ids.iter().copied().max_by(|a, b| seconds(&labels, *a).total_cmp(&seconds(&labels, *b))).unwrap_or(0)];
        }
        if heard.len() < ids.len() {
            let centres: Vec<Vec<f32>> = heard.iter().map(|&id| points.centre((0..n).filter(|&i| labels[i] == id))).collect();
            for i in 0..n {
                if !heard.contains(&labels[i]) {
                    labels[i] = heard[argmax(centres.iter().map(|centre| dot(points.at(i), centre)))];
                }
            }
        }
    }
    let voices = |labels: &[usize]| {
        let mut ids = labels.to_vec();
        ids.sort_unstable();
        ids.dedup();
        ids.len()
    };
    let before = (fixed.is_some()).then(|| labels.clone());
    refine(points, &mut labels, REFINE_ROUNDS);
    // Tidying the edges may empty a voice. Found, that is one voice fewer; asked for by
    // number, it is the number that stands.
    if let Some(before) = before.filter(|before| voices(before) > voices(&labels)) {
        labels = before;
    }
    labels
}

/// Group a recording's voice prints and give each of its `spans` a speaker. With `fixed`
/// the number of voices is taken as given rather than found. Asking for more than there are
/// splits real voices; the answer then holds as many as are most of some span, which can be
/// fewer than were asked for, and never more than the recording has windows.
pub fn cluster(prints: &Prints, spans: usize, fixed: Option<usize>) -> Labels {
    let mut out = Labels { spans: vec![None; spans], speakers: 0 };
    if prints.is_empty() {
        return out;
    }
    let points = Points::of(prints);
    let voices = group(&points, fixed.map(|count| count.clamp(1, MAX_SPEAKERS)));

    // A span is whoever most of it sounds like.
    let mut tally: Vec<Vec<(usize, f32)>> = vec![Vec::new(); spans];
    for (window, voice) in prints.windows().iter().zip(&voices) {
        let Some(votes) = tally.get_mut(window.span as usize) else { continue };
        match votes.iter_mut().find(|(id, _)| id == voice) {
            Some((_, seconds)) => *seconds += window.seconds(),
            None => votes.push((*voice, window.seconds())),
        }
    }
    // Numbered in the order they are first heard, so the first to speak is speaker one.
    let mut order: Vec<usize> = Vec::new();
    for (slot, votes) in out.spans.iter_mut().zip(&tally) {
        let Some(voice) = votes.iter().map(|(_, seconds)| *seconds).reduce(f32::max).and_then(|most| votes.iter().find(|(_, seconds)| *seconds == most)).map(|(id, _)| *id) else { continue };
        let number = order.iter().position(|id| *id == voice).unwrap_or_else(|| {
            order.push(voice);
            order.len() - 1
        });
        *slot = Some(number as u8);
    }
    out.speakers = order.len();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window::Window;

    /// Prints scattered around a few made-up voices. `spread` is how far from its voice a
    /// print may stray; `loud` is the length real prints of clear speech have.
    struct Fake {
        rng: Rng,
        prints: Prints,
        spans: u32,
    }

    impl Fake {
        fn new() -> Self {
            Self { rng: Rng(42), prints: Prints::default(), spans: 0 }
        }

        fn normal(&mut self) -> f32 {
            // Sum of twelve uniforms: near enough to a bell curve for scattering points.
            (0..12).map(|_| self.rng.unit()).sum::<f64>() as f32 - 6.0
        }

        fn voice(&mut self) -> Vec<f32> {
            (0..DIM).map(|_| self.normal()).collect()
        }

        /// A span of `windows` windows of `voice`, each `seconds` long.
        fn say(&mut self, voice: &[f32], windows: usize, seconds: f32, spread: f32, loud: f32) {
            for _ in 0..windows {
                let length = dot(voice, voice).sqrt();
                let print: Vec<f32> = voice.iter().map(|v| v / length).collect();
                let noise: Vec<f32> = (0..DIM).map(|_| self.normal() * spread / (DIM as f32).sqrt()).collect();
                let print: Vec<f32> = print.iter().zip(&noise).map(|(v, n)| (v + n) * loud).collect();
                self.prints.push(Window { span: self.spans, start: 0, len: (seconds * 16_000.0) as u32 }, &print);
            }
            self.spans += 1;
        }

        fn labels(&self, fixed: Option<usize>) -> Labels {
            cluster(&self.prints, self.spans as usize, fixed)
        }
    }

    #[test]
    fn one_voice_is_one_speaker_however_noisy() {
        for spread in [0.6, 1.0, 1.6] {
            let mut fake = Fake::new();
            let voice = fake.voice();
            for _ in 0..40 {
                fake.say(&voice, 2, 2.0, spread, 3.0);
            }
            let labels = fake.labels(None);
            assert_eq!(labels.speakers, 1, "spread {spread}");
            assert!(labels.spans.iter().all(|s| *s == Some(0)));
        }
    }

    #[test]
    fn voices_are_told_apart_and_numbered_as_first_heard() {
        let mut fake = Fake::new();
        let voices = [fake.voice(), fake.voice(), fake.voice()];
        // B opens, then A, then C: speaker numbers follow that order, not the voices' own.
        let turns = [1, 0, 2, 0, 1, 0, 2, 1, 0, 1, 2, 0, 1, 0, 2, 1, 0, 2, 0, 1, 1, 0, 2, 0];
        for &turn in &turns {
            fake.say(&voices[turn], 3, 2.0, 0.9, 3.0);
        }
        let labels = fake.labels(None);
        assert_eq!(labels.speakers, 3);
        let expected: Vec<Option<u8>> = turns.iter().map(|turn| Some([1u8, 0, 2][*turn])).collect();
        assert_eq!(labels.spans, expected);
    }

    #[test]
    fn unsure_windows_vote_but_found_nothing() {
        let mut fake = Fake::new();
        let (a, b) = (fake.voice(), fake.voice());
        for _ in 0..12 {
            fake.say(&a, 3, 2.0, 0.8, 3.0);
            fake.say(&b, 3, 2.0, 0.8, 3.0);
        }
        // A fifth of the windows again, each unlike anything and each quiet in the network's
        // own terms: breaths and crosstalk. On their own they would be a third speaker.
        let stray = fake.voice();
        for _ in 0..18 {
            let near_a: Vec<f32> = a.iter().zip(&stray).map(|(v, s)| v * 0.5 + s).collect();
            fake.say(&near_a, 1, 2.0, 1.5, 0.9);
        }
        let labels = fake.labels(None);
        assert_eq!(labels.speakers, 2);
        // Clips under a second do not count either, and take the voice they are nearest.
        let mut short = Fake::new();
        let (a, b) = (short.voice(), short.voice());
        for _ in 0..10 {
            short.say(&a, 3, 2.0, 0.8, 3.0);
            short.say(&b, 1, 0.4, 0.8, 3.0);
        }
        let labels = short.labels(None);
        assert_eq!(labels.speakers, 1, "ten half-second clips are not a second speaker");
    }

    #[test]
    fn a_count_given_by_hand_is_kept_to() {
        let mut fake = Fake::new();
        let voices = [fake.voice(), fake.voice(), fake.voice(), fake.voice()];
        for round in 0..10 {
            for (index, voice) in voices.iter().enumerate() {
                fake.say(voice, 2 + (round + index) % 2, 2.0, 0.9, 3.0);
            }
        }
        assert_eq!(fake.labels(None).speakers, 4);
        assert_eq!(fake.labels(Some(4)).spans, fake.labels(None).spans);
        // Fewer than there are: the most alike are put together, and nothing else moves.
        let two = fake.labels(Some(2));
        assert_eq!(two.speakers, 2);
        let one = fake.labels(Some(1));
        assert!(one.speakers == 1 && one.spans.iter().all(|s| *s == Some(0)));
        // More than there are: voices are split to make up the number, though half a voice
        // that is never most of a stretch does not show.
        let six = fake.labels(Some(6));
        assert!((5..=6).contains(&six.speakers), "{} speakers", six.speakers);
        assert!(fake.labels(Some(1000)).speakers <= MAX_SPEAKERS);

        // A short recording can still be told how many speak in it. Three voices over a
        // minute are too few windows to find a third among, and are three once it is said.
        let mut short = Fake::new();
        let voices = [short.voice(), short.voice(), short.voice()];
        for turn in 0..9 {
            short.say(&voices[turn % 3], 3, 2.0, 0.7, 3.0);
        }
        let told = short.labels(Some(3));
        assert_eq!(told.speakers, 3);
        assert_eq!(told.spans, (0..9).map(|turn| Some((turn % 3) as u8)).collect::<Vec<_>>());
        // Down to two people and four windows.
        let mut tiny = Fake::new();
        let (a, b) = (tiny.voice(), tiny.voice());
        tiny.say(&a, 2, 2.0, 0.5, 3.0);
        tiny.say(&b, 2, 2.0, 0.5, 3.0);
        assert_eq!(tiny.labels(None).speakers, 1);
        assert_eq!(tiny.labels(Some(2)), Labels { spans: vec![Some(0), Some(1)], speakers: 2 });
        // And never more voices than windows.
        assert_eq!(tiny.labels(Some(9)).speakers, 2);
    }

    #[test]
    fn too_little_to_go_on_is_one_speaker_and_nothing_is_nobody() {
        let mut fake = Fake::new();
        let (a, b) = (fake.voice(), fake.voice());
        fake.say(&a, 2, 2.0, 0.5, 3.0);
        fake.say(&b, 2, 2.0, 0.5, 3.0);
        // Four windows: not enough to say there are two people.
        assert_eq!(fake.labels(None), Labels { spans: vec![Some(0), Some(0)], speakers: 1 });
        // A span with no window is nobody's, and a window for a span that is not there is
        // ignored rather than trusted.
        fake.spans += 1;
        fake.prints.push(Window { span: 99, start: 0, len: 32_000 }, &a);
        assert_eq!(fake.labels(None).spans, [Some(0), Some(0), None]);
        assert_eq!(cluster(&Prints::default(), 2, None), Labels { spans: vec![None, None], speakers: 0 });
    }

    #[test]
    fn a_print_that_is_not_a_number_poisons_nothing() {
        let mut fake = Fake::new();
        let (a, b) = (fake.voice(), fake.voice());
        for _ in 0..10 {
            fake.say(&a, 3, 2.0, 0.8, 3.0);
            fake.say(&b, 3, 2.0, 0.8, 3.0);
        }
        let clean = fake.labels(None);
        assert_eq!(clean.speakers, 2);
        // One window of each kind of nonsense, in spans of their own.
        for bad in [f32::NAN, f32::INFINITY] {
            let mut print = a.clone();
            print[7] = bad;
            fake.prints.push(Window { span: fake.spans, start: 0, len: 32_000 }, &print);
            fake.spans += 1;
        }
        let labels = fake.labels(None);
        assert_eq!((labels.speakers, &labels.spans[..20]), (2, &clean.spans[..]));
        assert!(labels.spans[20..].iter().all(|s| s.is_some()));
        // And a recording of nothing but nonsense is one speaker, not a panic.
        let mut broken = Prints::default();
        for span in 0..12 {
            broken.push(Window { span, start: 0, len: 32_000 }, &[f32::NAN; DIM]);
        }
        assert_eq!(cluster(&broken, 12, None).speakers, 1);
        assert_eq!(cluster(&broken, 12, Some(3)).speakers, 1);
    }

    #[test]
    fn quantiles_interpolate_as_numpy_does() {
        assert_eq!(quantile(&mut [4.0, 1.0, 3.0, 2.0], 0.2), 1.6);
        assert_eq!(quantile(&mut [5.0], 0.2), 5.0);
        assert_eq!(quantile(&mut [1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 0.2), 2.0);
        // Where the cut lands exactly on a window it is that window's value, not a hair
        // over it: the window is kept, as numpy keeps it.
        assert_eq!(quantile(&mut [1.0, 1.5, 8.0, 8.0, 8.0, 8.0], WEAK_SHARE), 1.5);
        let mut rng = Rng(0x5eed);
        // The stream the Python reference is checked against.
        assert_eq!((rng.unit() * 1e9) as u64, 38_848_734);
    }
}
