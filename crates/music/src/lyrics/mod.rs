pub(crate) mod catalog;
mod japanese;
pub mod lrc;
pub(crate) mod romanize;
mod shape;
pub(crate) mod sheet;
pub(crate) mod ttml;

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::{Lyrics, LyricsHit, LyricsLine, LyricsQuery, LyricsWord};

/// The provider that reads a local file's own tags.
pub const LOCAL: &str = "Local";
/// The two Chinese services, which answer for much the same catalogue. A sheet names itself with
/// one of these, so a caller can tell them apart.
pub const NETEASE: &str = "NetEase";
pub const KUGOU: &str = "Kugou";
/// The third of them, which hands over a translation of its own beside most of the foreign songs it
/// holds.
pub const QQ: &str = "QQ Music";

const CLOSE_ENOUGH: u64 = 3;
/// How far a sheet's own length may sit from the track's and still answer for it, in seconds, for
/// one that names the song or its singer. The same song comes out at different lengths on different
/// releases — 打上花火 runs 289 seconds on the single and 259 on the album it was later put on — and
/// what a listener is after is the words, not that exact take, so a release half a minute either way
/// is still taken for it. Which sheet is shown is the length's to say: the take timed closest to the
/// track wins.
const WAY_OFF: u64 = 30;
/// The same, for a sheet with nothing but what its names read as to show for itself, which is the
/// only thing keeping another recording of the same singer out. Less says it is the one, so it also
/// has to run closer to the track.
const READ_OFF: u64 = 20;
/// What a sheet that runs to the track's own length is worth, and what every second it misses that
/// length by, past the window that already counts as the same length, costs it. The step is small
/// enough to grade the whole span a release can differ by, so the take the track was timed against
/// is the one shown, while two services timing one recording off different masters — a second or two
/// either way — cost it nothing at all.
const LENGTH: u32 = 100;
const LENGTH_STEP: u32 = 2;
const TITLE: u32 = 40;
const ARTIST: u32 = 30;
const ALBUM: u32 = 15;
const SYNCED: u32 = 200;
const WORDED: u32 = 400;
const TRUNCATED: u32 = 500;
const TRUSTED: u32 = 25;
/// What a sheet is worth over an otherwise identical one for having come with a track
/// beside the words.
const CARRIED: u32 = 10;
/// Rank the sheets that answer for a track, best first, whatever source each came from.
pub fn rank(query: &LyricsQuery, hits: Vec<LyricsHit>) -> Vec<LyricsHit> {
    let mut scored: Vec<(bool, i64, LyricsHit)> = hits
        .into_iter()
        .filter(|hit| eligible(query, hit))
        .map(|hit| {
            (
                names_the_track(&hit.title, &query.title),
                score(query, &hit),
                hit,
            )
        })
        .collect();
    scored.sort_by(
        |(left_names, left_score, left), (right_names, right_score, right)| {
            // a sheet that names the track itself comes before one that names a version of it,
            // however well the version scores: a remix or a live take is another recording
            right_names
                .cmp(left_names)
                .then_with(|| right_score.cmp(left_score))
                // sheets that name the song and score alike are told apart by their length: the one
                // the track's own take was timed against is the one its words were laid on
                .then_with(|| drift(query, left).cmp(&drift(query, right)))
                .then_with(|| left.source.cmp(right.source))
                .then_with(|| left.title.cmp(&right.title))
        },
    );

    let mut seen = HashSet::new();
    scored
        .into_iter()
        .map(|(_, _, hit)| hit)
        .filter(|hit| seen.insert(fingerprint(&hit.lyrics)))
        .collect()
}

/// Whether a sheet names the track the way the track names itself, rather than as a version of
/// it. A remix, a live take or an instrumental is another recording whose words may differ, so
/// its sheet answers only for the tracks nothing better names.
fn names_the_track(claimed: &str, wanted: &str) -> bool {
    title_letters(claimed, wanted) == 1.
}

pub fn reshape(hits: &mut [LyricsHit]) {
    let Some(guide) = hits
        .iter()
        .find(|hit| hit.lyrics.synced() && !hit.lyrics.worded())
        .map(|hit| hit.lyrics.clone())
    else {
        return;
    };
    for hit in hits
        .iter_mut()
        .filter(|hit| hit.lyrics.worded() && hit.trust < TRUSTED && !layered(&hit.lyrics))
    {
        if let Some(conformed) = shape::conform(&hit.lyrics, &guide) {
            hit.lyrics = conformed;
        }
    }
}

fn layered(lyrics: &Lyrics) -> bool {
    let Lyrics::Synced { lines } = lyrics else {
        return false;
    };
    lines
        .iter()
        .any(|line| !line.secondary.is_empty() || !line.voice.lead())
}

pub fn eligible(query: &LyricsQuery, hit: &LyricsHit) -> bool {
    matched(query, hit) && !hit.lyrics.is_empty() && !low_quality(&hit.lyrics)
}

pub fn instrumental(query: &LyricsQuery, hits: &[LyricsHit]) -> bool {
    let matching = || hits.iter().filter(|hit| matched(query, hit));
    matching().any(|hit| hit.instrumental) && !matching().any(|hit| !hit.lyrics.is_empty())
}

/// Whether a sheet answers for the track at all: it has to name the song or its singer by what they
/// are written as, or read alike when the two are written in different scripts, and then run to
/// something like the track's own length. Whether it names the track itself or a version of it is
/// not asked here; that only decides the order the sheets come in.
fn matched(query: &LyricsQuery, hit: &LyricsHit) -> bool {
    if !could_be(query, &hit.title, &hit.artist) {
        return false;
    }
    // a sheet that brings no length of its own can only answer by its names
    let Some(duration) = hit.duration else {
        return alike(&hit.title, &query.title) && artists_alike(&hit.artist, &query.artist);
    };
    query.duration.is_zero()
        || duration.as_secs().abs_diff(query.duration.as_secs()) <= allowance(query, hit)
}

/// How far a sheet's own length may sit from the track's and still answer for it, by what the sheet
/// has to show for itself: one that names the song or its singer is the recording whatever release
/// it was taken from, while one only read alike has to be closer, since less says it is the one.
fn allowance(query: &LyricsQuery, hit: &LyricsHit) -> u64 {
    match alike(&hit.title, &query.title) || artists_alike(&hit.artist, &query.artist) {
        true => WAY_OFF,
        false => READ_OFF,
    }
}

/// How far a sheet's own length sits from the track's, which is nothing at all for a sheet that
/// carries no length or a track that never had one, so those are taken as they come.
fn drift(query: &LyricsQuery, hit: &LyricsHit) -> u64 {
    match hit.duration {
        Some(duration) if !query.duration.is_zero() => {
            duration.as_secs().abs_diff(query.duration.as_secs())
        }
        _ => 0,
    }
}

/// How much of a Latin name a non-Latin one has to read as for the two to be weighed at all when
/// nothing else about them can be compared. A reading taken by machine is often wrong — a kanji
/// title can come out nothing like the reading the service filed it under — but the words it does
/// get right are still there, and a name that shares none of them is another song.
const READING_LEAST: f32 = 0.2;
/// How much of a title another script has to read as for the two to be taken for one song written
/// two ways, the letters having said nothing. Unrelated titles still come out sharing about a fifth
/// of their bigrams — 感電 and M八七 read 0.22 and 0.26 of Uchiagehanabi, both another song by the
/// same singer one or two seconds off the length asked for — where the song itself reads 0.57, so
/// the floor sits above the noise that a shared singer and a shared length would let through.
const READING_LIKELY: f32 = 0.35;
/// How much of a reading two names have to share to be taken for one name written two ways. The
/// same name in another character set reads the same way twice, so little is left to chance.
const READING_SAME: f32 = 0.8;

/// Whether a song a service lists could be the one being played, by its names alone. What is
/// comparable has to agree: the title, or the credits. A field the two services write in different
/// scripts — one in Latin letters and the other not, so that neither can name the other — is not
/// compared by its letters but by what it reads as. How long the sheet runs is not asked here: a
/// song comes out at different lengths on different releases, and it is the length that sorts the
/// sheets that get this far, not one that keeps them out.
pub(crate) fn could_be(query: &LyricsQuery, title: &str, artist: &str) -> bool {
    match (
        artists_alike(artist, &query.artist),
        alike(title, &query.title),
    ) {
        (true, true) => true,
        // the title is not written the way the one asked for is, so its letters say nothing and its
        // reading is what has to: a song by the same singer would otherwise answer for the one
        // playing, its length being the only thing left to tell them apart
        (true, false) => {
            scripts_differ(title, &query.title)
                && reading_similarity(title, &query.title) >= READING_LIKELY
        }
        // the title names the track but the credits cannot be compared, so the credits have to at
        // least read like the ones asked for: a cover by someone whose name happens to be written
        // in Latin letters would otherwise answer for a song its singer never recorded
        (false, true) => {
            scripts_differ(artist, &query.artist)
                && reading_similarity(artist, &query.artist) >= READING_LEAST
        }
        // neither name is written the way the other is, so both are read instead: what the title
        // reads as is what says whether this is the one
        (false, false) => {
            scripts_differ(title, &query.title)
                && scripts_differ(artist, &query.artist)
                && reading_similarity(title, &query.title) >= READING_LIKELY
        }
    }
}

/// How much of a track's own title a song's title answers for, from nothing to all of it, for a
/// provider choosing which of its search's answers are worth fetching a sheet for. It is the measure
/// the ranking weighs a title by, so the sheets fetched are the ones the ranking will want.
pub(crate) fn title_match(claimed: &str, wanted: &str) -> f32 {
    name_similarity(claimed, wanted, title_letters)
}

/// How much of a track's own artist line a song's credit answers for, from nothing to all of it.
pub(crate) fn artist_match(claimed: &str, wanted: &str) -> f32 {
    name_similarity(claimed, wanted, artist_letters)
}

/// Whether a title is written in Latin letters and no others.
fn written_in_latin(text: &str) -> bool {
    let mut letters = text
        .chars()
        .filter(|letter| letter.is_alphanumeric())
        .peekable();
    letters.peek().is_some_and(|first| first.is_ascii()) && letters.all(|letter| letter.is_ascii())
}

/// Whether two titles are written in different scripts, one in Latin letters and the other in
/// something else. Only then can neither name the other, whatever they say.
fn scripts_differ(left: &str, right: &str) -> bool {
    written_in_latin(left) != written_in_latin(right)
}

/// How much of a name reads as the other one, for two names written in different scripts. A
/// reading is not a translation, and reading a name out by machine is not the same as knowing it —
/// 打上花火 comes back as "dauehanabi" where the service filed it under "Uchiagehanabi" — but the
/// letters the two do share are the only thing such a pair has in common, and they are enough to
/// tell one song from another of the same length.
fn reading_similarity(left: &str, right: &str) -> f32 {
    let mut best: f32 = 0.;
    for left in readings(left) {
        for right in readings(right) {
            best = best.max(dice(&left, &right));
        }
    }
    best
}

/// How much two readings have in common, over the pairs of letters they are made of. Sound written
/// out twice never comes out the same twice, so this counts what matches rather than asking for
/// equality.
fn dice(left: &[char], right: &[char]) -> f32 {
    if left.len() < 2 || right.len() < 2 {
        return 0.;
    }
    let mut right: Vec<(char, char)> = right.windows(2).map(|pair| (pair[0], pair[1])).collect();
    let total = left.len() - 1 + right.len();
    let mut shared = 0usize;
    for pair in left.windows(2) {
        let pair = (pair[0], pair[1]);
        if let Some(place) = right.iter().position(|other| *other == pair) {
            right.swap_remove(place);
            shared += 1;
        }
    }
    2. * shared as f32 / total as f32
}

/// The ways a name written in another script might be read out in Latin letters. More than one is
/// offered because a name of kanji alone reads as the language its script suggests and as Japanese
/// alike, and only one of those is the reading the other service filed it under.
fn readings(text: &str) -> Vec<Vec<char>> {
    let mut found: Vec<Vec<char>> = Vec::new();
    let mut push = |letters: Vec<char>| {
        if !letters.is_empty() && !found.contains(&letters) {
            found.push(letters);
        }
    };
    if let Some(romanized) = romanize::plain(text) {
        push(spelled(&romanized.text));
    }
    push(spelled(&japanese::romanize(text)));
    if written_in_latin(text) {
        push(spelled(text));
    }
    found
}

pub fn score(query: &LyricsQuery, hit: &LyricsHit) -> i64 {
    let mut score: i64 = i64::from(hit.trust);
    if let Some(duration) = hit.duration
        && !query.duration.is_zero()
    {
        // a sheet loses nothing for the second or two the two services time one recording
        // differently by, so inside that window the names are what tell one sheet from another
        let drift = duration.as_secs().abs_diff(query.duration.as_secs());
        let past = drift
            .saturating_sub(CLOSE_ENOUGH)
            .min(WAY_OFF - CLOSE_ENOUGH) as u32;
        score += i64::from(LENGTH.saturating_sub(past * LENGTH_STEP));
    }
    if comparable(&hit.title, &query.title) {
        score += (f64::from(TITLE)
            * f64::from(name_similarity(&hit.title, &query.title, title_letters)))
        .round() as i64;
    }
    if comparable(&hit.artist, &query.artist) {
        score += (f64::from(ARTIST)
            * f64::from(name_similarity(&hit.artist, &query.artist, artist_letters)))
        .round() as i64;
    }
    if let Some(album) = &query.album
        && let Some(named) = &hit.album
        && comparable(named, album)
    {
        score += (f64::from(ALBUM) * f64::from(name_similarity(named, album, title_letters)))
            .round() as i64;
    }
    if hit.lyrics.synced() {
        score += i64::from(SYNCED);
    }
    if hit.lyrics.worded() {
        score += i64::from(WORDED);
    }
    if hit.lyrics.carries_extras() {
        score += i64::from(CARRIED);
    }
    if truncated(&hit.lyrics, query.duration) {
        score -= i64::from(TRUNCATED);
    }
    score
}

/// A title as the words it is read by: letters, digits and the bracketed words a different
/// version of the same song is named by, lowercased, with the punctuation and spacing between
/// them dropped. The brackets are kept here, where matching drops them, because what a version
/// calls itself is exactly what tells two recordings of one song apart.
fn spelled(text: &str) -> Vec<char> {
    text.chars()
        .filter(|letter| letter.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether two names could be one name written two ways, so that weighing how much of one the other
/// carries means anything: the same name, or two names written in different scripts, where only the
/// reading can tell. Two names in different non-Latin scripts cannot be, and are weighed by length
/// alone elsewhere.
fn comparable(left: &str, right: &str) -> bool {
    alike(left, right) || scripts_differ(left, right)
}

/// How much of one name another carries, from nothing to all of it: what the two have in common in
/// the letters they are written with, and what they have in common in the way they read — the only
/// thing left to go on when the same name is written in another script or another character set —
/// whichever of the two says more. The names are read as they stand, decorations and all, because
/// what a version calls itself is exactly what tells it from the track it is a version of.
/// `by_letters` is how the letters are weighed, which is not the same for every field: a title is a
/// run of words and a credit is a list of names.
fn name_similarity(claimed: &str, wanted: &str, by_letters: fn(&str, &str) -> f32) -> f32 {
    let (claimed, wanted) = (claimed.trim(), wanted.trim());
    if claimed.is_empty() || wanted.is_empty() {
        return 0.;
    }
    by_letters(claimed, wanted).max(reading_similarity(claimed, wanted))
}

/// How much of the track's own title a sheet's title carries, from none of it to all of it. What
/// it measures is what the sheet adds to the words the two share: a sheet that calls them a
/// remix, a live take or an instrumental carries less of the track than one that names it the way
/// the track names itself, while a track whose own title carries its album's decoration is not
/// held against a sheet that leaves that out.
fn title_letters(claimed: &str, wanted: &str) -> f32 {
    let claimed = spelled(claimed);
    if claimed.is_empty() {
        return 0.;
    }

    let mut spare: HashMap<char, usize> = HashMap::new();
    for letter in spelled(wanted) {
        *spare.entry(letter).or_default() += 1;
    }
    let added = claimed
        .iter()
        .filter(|letter| match spare.get_mut(letter) {
            Some(left) if *left > 0 => {
                *left -= 1;
                false
            }
            _ => true,
        })
        .count();
    1. - added as f32 / claimed.len() as f32
}

/// How much of the track's own artist line a sheet's artist line carries, from none of it to all
/// of it. It reads the line as the names it is made of, and as with the title counts what the
/// sheet adds: one that credits a remixer or a guest alongside the track's own artists carries
/// less of the track than one that credits them alone, while a sheet that leaves one of several
/// artists out is not held against it. A name answers for one of the track's own when it is spelt
/// the same, holds the same words, or reads the same — the services write the same act in another
/// script as often as not, and a credit line naming four people would otherwise be read as four
/// strangers, scoring below one that names a single one of them.
fn artist_letters(claimed: &str, wanted: &str) -> f32 {
    let claimed: Vec<String> = artist_names(claimed)
        .map(undecorated)
        .filter(|name| !name.is_empty())
        .collect();
    if claimed.is_empty() {
        return 0.;
    }
    let wanted: Vec<String> = artist_names(wanted)
        .map(undecorated)
        .filter(|name| !name.is_empty())
        .collect();
    let added = claimed
        .iter()
        .filter(|name| {
            !wanted.iter().any(|wanted| {
                name.as_str() == wanted.as_str()
                    || held(name, wanted)
                    || reading_similarity(name, wanted) >= READING_LEAST
            })
        })
        .count();
    1. - added as f32 / claimed.len() as f32
}

/// What a sheet is worth for the service answering a search for the track's title with it, when the
/// title it carries is written in another script than the one asked for. The service matched a title
/// written another way — its translation, or its reading — which is a relationship the letters here
/// cannot see at all, and only the service can vouch for. Worth what a sheet fetched by id is worth,
/// which is the site telling us outright.
///
/// A title in the same script is left to be weighed as it stands: what it shares with the track's own
/// title, or adds to it, is all there under the letters, and a name that is two thirds of the track's
/// says as much by being two thirds of it. Handing that one the trust as well would score a live take
/// above the recording it was taken from.
pub(crate) fn answered_by_title(claimed: &str, wanted: &str) -> u32 {
    match scripts_differ(claimed, wanted) && !names_the_track(claimed, wanted) {
        true => catalog::TRUST,
        false => 0,
    }
}

fn truncated(lyrics: &Lyrics, duration: Duration) -> bool {
    let Some(span) = lyrics.span() else {
        return false;
    };
    !duration.is_zero() && span.as_secs_f64() < duration.as_secs_f64() * 0.6
}

fn low_quality(lyrics: &Lyrics) -> bool {
    let Lyrics::Synced { lines } = lyrics else {
        return false;
    };
    let texts = lines.iter().flat_map(|line| {
        std::iter::once(line.text.as_str()).chain(
            line.secondary
                .iter()
                .map(|secondary| secondary.text.as_str()),
        )
    });
    let (total, noisy) = texts.fold((0usize, 0usize), |(total, noisy), text| {
        (total + 1, noisy + usize::from(stretched_shout(text)))
    });
    noisy >= 3 && noisy.saturating_mul(6) >= total
}

fn stretched_shout(text: &str) -> bool {
    let mut uppercase = false;
    let mut lowercase = false;
    let mut previous = None;
    let mut run = 0usize;
    let mut longest = 0usize;
    for letter in text.chars().filter(|letter| letter.is_alphabetic()) {
        uppercase |= letter.is_uppercase();
        lowercase |= letter.is_lowercase();
        let folded = letter.to_lowercase().next().unwrap_or(letter);
        if previous == Some(folded) {
            run += 1;
        } else {
            previous = Some(folded);
            run = 1;
        }
        longest = longest.max(run);
    }
    uppercase && !lowercase && longest >= 3
}

fn fingerprint(lyrics: &Lyrics) -> String {
    let trim = |text: &str| {
        text.chars()
            .filter(|letter| letter.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    match lyrics {
        Lyrics::Plain { text, .. } => format!("plain:{}", trim(text)),
        Lyrics::Synced { lines } => {
            let worded = lines.iter().any(LyricsLine::worded);
            let text: String = lines
                .iter()
                .flat_map(|line| {
                    std::iter::once(line.text.as_str()).chain(
                        line.secondary
                            .iter()
                            .map(|secondary| secondary.text.as_str()),
                    )
                })
                .map(trim)
                .collect();
            format!("synced:{worded}:{text}")
        }
    }
}

/// Whether two titles name the same thing, decoration aside. Shared with the providers so
/// they can drop the songs that never could be the one being played before fetching sheets.
pub(crate) fn alike(left: &str, right: &str) -> bool {
    let (left, right) = (undecorated(left), undecorated(right));
    if left.is_empty() || right.is_empty() {
        return false;
    }
    held(&left, &right) || reads_alike(&left, &right)
}

/// Whether one name holds the other whole, which is how a decorated title meets a plain one. The
/// shorter side has to be at least half the longer, so two letters do not answer for a line of them.
fn held(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    let (short, long) = match left.len() <= right.len() {
        true => (left, right),
        false => (right, left),
    };
    long.contains(short) && short.len() * 2 >= long.len()
}

/// Whether two names read the same, near enough to be one name written two ways. This is what
/// catches a name the two sides write in different character sets: a Chinese storefront writes
/// 米津玄师 where the services write 米津玄師, and not a character of one is in the other, but both
/// read "mijinxuanshi". Two names written the same way differ by whole readings instead, which is
/// why most of the reading has to agree.
fn reads_alike(left: &str, right: &str) -> bool {
    reading_similarity(left, right) >= READING_SAME
}

/// Whether two credited-artist lines name anyone in common. Shared with the providers, as
/// [`alike`] is.
pub(crate) fn artists_alike(left: &str, right: &str) -> bool {
    alike(left, right)
        || artist_names(left).any(|left| artist_names(right).any(|right| alike(left, right)))
}

fn artist_names(artists: &str) -> impl Iterator<Item = &str> {
    artists
        .split(['、', ',', '，', '&', ';'])
        .map(str::trim)
        .filter(|artist| !artist.is_empty())
}

pub(super) fn undecorated(text: &str) -> String {
    let text = text.split(" - ").next().unwrap_or(text);
    let mut depth = 0usize;
    text.chars()
        .filter(|letter| match letter {
            '(' | '[' => {
                depth += 1;
                false
            }
            ')' | ']' => {
                depth = depth.saturating_sub(1);
                false
            }
            _ => depth == 0,
        })
        .filter(|letter| letter.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

pub fn active(lines: &[LyricsLine], at: Duration) -> Option<usize> {
    lines.iter().rposition(|line| line.start <= at)
}

pub fn active_word(words: &[LyricsWord], at: Duration) -> Option<usize> {
    words
        .iter()
        .rposition(|word| word.start <= at)
        .filter(|index| at < words[*index].end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Lyrics, Voice};

    fn hit(title: &str, artist: &str, seconds: u64, synced: bool) -> LyricsHit {
        LyricsHit {
            source: "test",
            trust: 0,
            lyrics: match synced {
                true => Lyrics::Synced {
                    lines: vec![line(0, seconds.saturating_sub(2), title)].into(),
                },
                false => Lyrics::plain(format!("la {title}")),
            },
            instrumental: false,
            title: title.to_owned(),
            artist: artist.to_owned(),
            album: None,
            duration: Some(Duration::from_secs(seconds)),
            writers: Vec::new(),
        }
    }

    fn line(start: u64, end: u64, text: &str) -> LyricsLine {
        LyricsLine {
            start: Duration::from_secs(start),
            end: Some(Duration::from_secs(end)),
            text: text.to_owned(),
            romanized: None,
            tracks: Vec::new(),
            words: None,
            secondary: Vec::new(),
            voice: Voice::Lead,
        }
    }

    fn query() -> LyricsQuery {
        LyricsQuery {
            title: "Jaded".to_owned(),
            artist: "Spiritbox".to_owned(),
            album: None,
            duration: Duration::from_secs(263),
            track: None,
        }
    }

    #[test]
    fn the_closest_duration_wins() {
        let hits = vec![
            hit("Jaded", "Spiritbox", 200, true),
            hit("Jaded", "Spiritbox", 263, false),
        ];
        let ranked = rank(&query(), hits);
        assert_eq!(ranked[0].duration, Some(Duration::from_secs(263)));
    }

    #[test]
    fn synced_breaks_a_tie() {
        let hits = vec![
            hit("Jaded", "Spiritbox", 263, false),
            hit("Jaded", "Spiritbox", 263, true),
        ];
        let ranked = rank(&query(), hits);
        assert!(ranked[0].lyrics.synced());
    }

    #[test]
    fn a_wrong_track_falls_behind() {
        let hits = vec![
            hit("Something Else", "Nobody", 263, true),
            hit("Jaded", "Spiritbox", 261, false),
        ];
        let ranked = rank(&query(), hits);
        assert_eq!(ranked[0].title, "Jaded");
        assert_eq!(ranked.len(), 1);
    }

    #[test]
    fn karaoke_cannot_rescue_the_wrong_artist() {
        let mut wrong = hit("Jaded", "Aerosmith", 263, true);
        let Lyrics::Synced { lines } = &wrong.lyrics else {
            unreachable!()
        };
        let mut lines = lines.to_vec();
        lines[0].words = Some(vec![LyricsWord {
            start: Duration::ZERO,
            end: Duration::from_secs(1),
            text: "wrong song".to_owned(),
        }]);
        wrong.lyrics = Lyrics::Synced {
            lines: lines.into(),
        };

        let ranked = rank(&query(), vec![wrong, hit("Jaded", "Spiritbox", 263, false)]);

        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].artist, "Spiritbox");
    }

    #[test]
    fn noisy_karaoke_cannot_beat_clean_lyrics() {
        let mut noisy = hit("In Waves", "Trivium", 302, true);
        let mut lines: Vec<LyricsLine> = (0..12)
            .map(|index| line(index, index + 1, "Pulling everyone down with me"))
            .collect();
        for text in ["IN WAVESSSSS!", "PERPETUALLYYY!!!", "AHHHHHHH!!!!!"] {
            let mut shouted = line(lines.len() as u64, lines.len() as u64 + 1, text);
            shouted.words = Some(vec![LyricsWord {
                start: shouted.start,
                end: shouted.end.expect("the test line has an end"),
                text: text.to_owned(),
            }]);
            lines.push(shouted);
        }
        noisy.lyrics = Lyrics::Synced {
            lines: lines.into(),
        };

        let query = LyricsQuery {
            title: "In Waves".to_owned(),
            artist: "Trivium".to_owned(),
            album: Some("In Waves".to_owned()),
            duration: Duration::from_secs(302),
            track: None,
        };
        let ranked = rank(&query, vec![noisy, hit("In Waves", "Trivium", 302, true)]);

        assert_eq!(ranked.len(), 1);
        assert!(!low_quality(&ranked[0].lyrics));
    }

    #[test]
    fn one_stylized_shout_does_not_reject_an_otherwise_clean_sheet() {
        let mut lines: Vec<LyricsLine> = (0..12)
            .map(|index| line(index, index + 1, "ordinary line"))
            .collect();
        lines.push(line(12, 13, "NOOO!!!"));

        assert!(!low_quality(&Lyrics::Synced {
            lines: lines.into()
        }));
    }

    #[test]
    fn a_same_length_song_by_the_same_artist_is_still_rejected() {
        let query = LyricsQuery {
            title: "Versailles".to_owned(),
            artist: "Pinback".to_owned(),
            album: Some("Nautical Antiques".to_owned()),
            duration: Duration::from_secs(213),
            track: None,
        };

        let ranked = rank(
            &query,
            vec![
                hit("Loro", "Pinback", 214, true),
                hit("Versailles", "Pinback", 213, false),
            ],
        );

        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].title, "Versailles");
    }

    #[test]
    fn a_song_filed_under_another_script_is_taken_by_its_length() {
        let query = LyricsQuery {
            title: "Uchiagehanabi".to_owned(),
            artist: "DAOKO".to_owned(),
            album: None,
            duration: Duration::from_secs(289),
            track: None,
        };

        // the same recording, which the site files under its own name: no title of one can name
        // the other, so the credits and the length are all there is to go on
        let ranked = rank(&query, vec![hit("打上花火", "DAOKO", 289, true)]);

        assert_eq!(ranked.len(), 1);
    }

    #[test]
    fn a_song_filed_under_another_script_with_another_name_for_its_artist_is_taken_by_its_reading()
    {
        let query = LyricsQuery {
            title: "Uchiagehanabi".to_owned(),
            artist: "Kenshi Yonezu".to_owned(),
            album: None,
            duration: Duration::from_secs(289),
            track: None,
        };

        // both the song and the credit are written the site's own way, which is what a service
        // that romanizes everything leaves behind: what the two read as is what says it is the one
        let ranked = rank(&query, vec![hit("打上花火", "米津玄師", 289, true)]);
        assert_eq!(ranked.len(), 1);

        // another release of it, timed a few seconds either way, is the same words
        let ranked = rank(&query, vec![hit("打上花火", "米津玄師", 293, true)]);
        assert_eq!(ranked.len(), 1);

        // a length too far off, with nothing but the reading to say it is not the one, is another
        // recording
        let lost = rank(&query, vec![hit("打上花火", "米津玄師", 340, true)]);
        assert!(lost.is_empty());
    }

    #[test]
    fn a_different_recording_is_not_treated_as_the_same_track() {
        let ranked = rank(
            &query(),
            vec![
                hit("Jaded", "Spiritbox", 220, true),
                hit("Jaded", "Spiritbox", 263, false),
            ],
        );

        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].duration, Some(Duration::from_secs(263)));
    }

    #[test]
    fn one_shared_artist_is_enough_for_a_collaboration() {
        let mut query = query();
        query.artist = "Spiritbox, Megan Thee Stallion".to_owned();

        let ranked = rank(&query, vec![hit("Jaded", "Spiritbox", 263, false)]);

        assert_eq!(ranked.len(), 1);
    }

    #[test]
    fn worded_beats_merely_synced() {
        let mut worded = hit("Jaded", "Spiritbox", 263, true);
        let Lyrics::Synced { lines } = &worded.lyrics else {
            unreachable!()
        };
        let mut lines = lines.to_vec();
        lines[0].text = "another take".to_owned();
        lines[0].words = Some(vec![LyricsWord {
            start: Duration::ZERO,
            end: Duration::from_secs(1),
            text: "another take".to_owned(),
        }]);
        worded.lyrics = Lyrics::Synced {
            lines: lines.into(),
        };

        let ranked = rank(&query(), vec![hit("Jaded", "Spiritbox", 263, true), worded]);
        assert!(ranked[0].lyrics.worded());
    }

    #[test]
    fn a_matching_album_pulls_ahead() {
        let mut named = hit("Jaded", "Spiritbox", 263, true);
        named.album = Some("Eternal Blue".to_owned());
        let mut plain = hit("Jaded", "Spiritbox", 263, true);
        plain.lyrics = Lyrics::Synced {
            lines: vec![line(0, 261, "a different upload")].into(),
        };

        let mut query = query();
        query.album = Some("Eternal Blue".to_owned());
        let ranked = rank(&query, vec![plain, named]);
        assert_eq!(ranked[0].album.as_deref(), Some("Eternal Blue"));
    }

    #[test]
    fn a_truncated_sync_falls_behind() {
        let mut cut = hit("Jaded", "Spiritbox", 263, true);
        cut.lyrics = Lyrics::Synced {
            lines: vec![line(0, 40, "stops far too early")].into(),
        };

        let ranked = rank(&query(), vec![cut, hit("Jaded", "Spiritbox", 263, true)]);
        assert!(ranked[0].lyrics.span() > Some(Duration::from_secs(200)));
    }

    #[test]
    fn equal_hits_keep_a_stable_order() {
        let mut left = hit("Jaded", "Spiritbox", 263, true);
        left.source = "Beta";
        let mut right = left.clone();
        right.source = "Alpha";
        right.lyrics = Lyrics::Synced {
            lines: vec![line(0, 261, "a different upload")].into(),
        };

        let ranked = rank(&query(), vec![left.clone(), right.clone()]);
        let reversed = rank(&query(), vec![right, left]);
        assert_eq!(ranked[0].source, "Alpha");
        assert_eq!(reversed[0].source, "Alpha");
    }

    #[test]
    fn twin_uploads_collapse_into_one() {
        let hits = vec![
            hit("Jaded", "Spiritbox", 263, true),
            hit("Jaded", "Spiritbox", 263, true),
        ];
        assert_eq!(rank(&query(), hits).len(), 1);
    }

    #[test]
    fn trust_settles_an_otherwise_even_match() {
        let mut direct = hit("Jaded", "Spiritbox", 263, true);
        direct.trust = 25;
        direct.lyrics = Lyrics::Synced {
            lines: vec![line(0, 261, "a different upload")].into(),
        };
        direct.source = "Direct";

        let ranked = rank(&query(), vec![hit("Jaded", "Spiritbox", 263, true), direct]);
        assert_eq!(ranked[0].source, "Direct");
    }

    #[test]
    fn a_short_title_does_not_match_a_long_one() {
        assert!(alike("Don't Stop", "dont stop"));
        assert!(alike("Jaded", "JADED"));
        assert!(!alike("Jaded", "Rotoscope"));
        assert!(!alike("Love", "Love Story (Taylor's Version)"));
        assert!(alike("Jaded", "Jaded - Single"));
        assert!(alike("Jaded (Remastered 2024)", "Jaded"));
        assert!(alike("Love Story", "Love Story (Taylor's Version)"));
    }

    #[test]
    fn the_active_line_follows_the_clock() {
        let lines = vec![line(0, 5, "one"), line(5, 9, "two")];
        assert_eq!(active(&lines, Duration::from_secs(2)), Some(0));
        assert_eq!(active(&lines, Duration::from_secs(6)), Some(1));
        assert_eq!(active(&lines, Duration::from_secs(30)), Some(1));
    }

    #[test]
    fn a_sung_line_holds_until_the_next_one_starts() {
        let mut padded = line(0, 12, "one");
        padded.words = Some(vec![LyricsWord {
            start: Duration::ZERO,
            end: Duration::from_secs(5),
            text: "one".to_owned(),
        }]);

        assert_eq!(
            active(std::slice::from_ref(&padded), Duration::from_secs(4)),
            Some(0)
        );
        assert_eq!(
            active(std::slice::from_ref(&padded), Duration::from_secs(8)),
            Some(0)
        );

        let pair = vec![padded, line(10, 14, "two")];
        assert_eq!(active(&pair, Duration::from_secs(8)), Some(0));
        assert_eq!(active(&pair, Duration::from_secs(10)), Some(1));
    }

    #[test]
    fn the_active_word_follows_the_clock() {
        let words = vec![
            LyricsWord {
                start: Duration::from_millis(0),
                end: Duration::from_millis(400),
                text: "one ".to_owned(),
            },
            LyricsWord {
                start: Duration::from_millis(400),
                end: Duration::from_millis(900),
                text: "two".to_owned(),
            },
        ];
        assert_eq!(active_word(&words, Duration::from_millis(100)), Some(0));
        assert_eq!(active_word(&words, Duration::from_millis(500)), Some(1));
        assert_eq!(active_word(&words, Duration::from_millis(2000)), None);
    }
}
