use std::time::Duration;

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde::Deserialize;
use tokio::task::JoinSet;

use crate::lyrics::lrc;
use crate::{Lyrics, LyricsHit, LyricsLine, LyricsProvider, LyricsQuery, LyricsWord, Voice};

const SOURCE: &str = crate::lyrics::NETEASE;
/// The provider's slug. A query names the track's origin by slug, which is not the display name
/// in [`SOURCE`], and only a track that came from this site can be asked for by id.
const SLUG: &str = "netease";
/// What a sheet fetched by id is worth: the site hands a track's own lyrics over for its own id,
/// where a sheet found by name is only ever the site's answer to a search.
const EXACT: u32 = crate::lyrics::catalog::TRUST;
const SEARCH: &str = "https://music.163.com/api/search/get";
const LYRIC: &str = "https://music.163.com/api/song/lyric/v1";
const CANDIDATES: usize = 3;
/// How many songs a name search may answer with.
const HITS: usize = 5;
/// How far a side sheet's time may sit from the line it belongs to and still be taken for it, in
/// milliseconds. Wide enough for the tens the site's own two timelines differ by, narrow enough
/// that a line never takes its neighbour's words.
const SIDE_NEAR: u128 = 250;
/// How far a side sheet written for exactly this many lines may sit from them on a typical line and
/// still be read by place, in milliseconds. A sheet cut for another version drifts by whole lines,
/// which is seconds, and falls back to reading by time.
const PAIRED_NEAR: u128 = 1_000;
const AGENT: &str = concat!(
    "sonora/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/sonorahq/sonora)"
);

pub struct NetEase {
    http: reqwest::Client,
}

impl NetEase {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }

    /// One name search, answered with the songs the site lists for it.
    async fn search_songs(&self, wanted: &str, limit: usize) -> Result<Vec<Song>> {
        let response = self
            .http
            .get(SEARCH)
            .query(&[("s", wanted), ("type", "1"), ("limit", &limit.to_string())])
            .header("User-Agent", AGENT)
            .header("Referer", "https://music.163.com")
            .send()
            .await
            .context("cannot reach netease")?;
        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("netease answered with status {status}");
        }
        let answer: SearchAnswer = response
            .json()
            .await
            .context("cannot read the netease search response")?;
        Ok(answer.result.map(|result| result.songs).unwrap_or_default())
    }

    async fn lyric(&self, id: u64) -> Result<Sheet> {
        let response = self
            .http
            .get(LYRIC)
            .query(&[("id", id.to_string().as_str()), ("cp", "false")])
            .query(&[
                ("lv", "0"),
                ("tv", "1"),
                ("rv", "1"),
                ("kv", "0"),
                ("yv", "0"),
                ("ytv", "0"),
                ("yrv", "0"),
            ])
            .header("User-Agent", AGENT)
            .header("Referer", "https://music.163.com")
            .send()
            .await
            .context("cannot reach netease")?;
        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("netease answered with status {status}");
        }
        response
            .json()
            .await
            .context("cannot read the netease lyric response")
    }
}

impl Default for NetEase {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct SearchAnswer {
    result: Option<SearchResult>,
}

#[derive(Deserialize)]
struct SearchResult {
    #[serde(default)]
    songs: Vec<Song>,
}

#[derive(Deserialize)]
struct Song {
    id: u64,
    name: String,
    #[serde(default)]
    duration: u64,
    #[serde(default)]
    artists: Vec<Named>,
    album: Option<Named>,
}

#[derive(Deserialize)]
struct Named {
    name: Option<String>,
}

#[derive(Deserialize)]
struct Sheet {
    lrc: Option<Verse>,
    yrc: Option<Verse>,
    /// The translation, which the site hands over as a sheet of its own, timed like the lyric.
    tlyric: Option<Verse>,
    #[serde(default, rename = "pureMusic")]
    pure_music: bool,
}

#[derive(Deserialize)]
struct Verse {
    lyric: Option<String>,
}

#[async_trait]
impl LyricsProvider for NetEase {
    fn name(&self) -> &'static str {
        SOURCE
    }

    async fn search(&self, query: &LyricsQuery) -> Result<Vec<LyricsHit>> {
        // A track that came from this site carries its own id, and the site hands that
        // recording's own sheet over for it: nothing is searched for, and nothing is guessed.
        if let Some(id) = query.id_for(SLUG).and_then(|id| id.parse::<u64>().ok()) {
            let sheet = self.lyric(id).await?;
            return Ok(sheet_hit(query, &sheet, EXACT).into_iter().collect());
        }

        // The site is asked by name, then by the title alone. A service that romanizes its titles
        // calls a Japanese song by its reading, which names nothing the site holds it under, and
        // the title alone still finds it.
        let wanted = format!("{} {}", query.title, query.artist);
        let mut songs = shortlist(self.search_songs(&wanted, HITS).await?, query);
        if songs.is_empty() {
            songs = shortlist(self.search_songs(&query.title, HITS).await?, query);
        }

        let mut tasks = JoinSet::new();
        for song in songs {
            let netease = Self {
                http: self.http.clone(),
            };
            let named = song_query(&song);
            // a search that named the title and came back with a sheet whose own title cannot be
            // compared with it matched something only the site can see
            let trust = crate::lyrics::answered_by_title(&named.title, &query.title);
            tasks.spawn(async move {
                let sheet = netease
                    .lyric(song.id)
                    .await
                    .inspect_err(|error| {
                        log::warn!("lyrics: netease did not hand over {}: {error:#}", song.id)
                    })
                    .ok()?;
                sheet_hit(&named, &sheet, trust)
            });
        }

        let mut hits = Vec::new();
        while let Some(found) = tasks.join_next().await {
            hits.extend(found.ok().flatten());
        }
        Ok(hits)
    }
}

/// The songs worth fetching sheets for, out of what the site answered with.
fn shortlist(songs: Vec<Song>, query: &LyricsQuery) -> Vec<Song> {
    crate::lyrics::shortlist(
        songs,
        query,
        CANDIDATES,
        |song| (song.name.clone(), credited(song)),
        |song| {
            Duration::from_millis(song.duration)
                .as_secs()
                .abs_diff(query.duration.as_secs())
        },
    )
}

/// The credits the site lists for a song, as the one line the rest of the matching reads.
fn credited(song: &Song) -> String {
    song.artists
        .iter()
        .filter_map(|artist| artist.name.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

/// One sheet as a hit, named by whoever asked for it: an id-exact fetch names it from the track
/// being played, a search names it from the record it matched, and the two rank differently
/// because of it.
fn sheet_hit(named: &LyricsQuery, sheet: &Sheet, trust: u32) -> Option<LyricsHit> {
    let lyric = |verse: &Option<Verse>| {
        verse
            .as_ref()
            .and_then(|verse| verse.lyric.clone())
            .filter(|text| !text.trim().is_empty())
    };
    // The site holds a word-by-word sheet and a line-timed one, and the translation it wrote goes
    // with whichever of them it timed that translation against. On a track whose two sheets are cut
    // differently — the orchestral take of a song against the single it came from — the word-by-word
    // sheet carries another version's timing, so the sheet the translation sits on is the one that
    // can be shown with it: words and their translation together are worth more than the word-by-word
    // timing. Where the two agree, or where the site wrote no translation, the word-by-word sheet
    // still wins.
    let translation = lyric(&sheet.tlyric);
    let side = translation
        .as_deref()
        .map(stamped_lines)
        .unwrap_or_default();
    let worded = lyric(&sheet.yrc)
        .map(|yrc| parse_yrc(&yrc))
        .filter(|lines| !lines.is_empty());
    let timed = lyric(&sheet.lrc)
        .map(|text| lrc::parse(&text))
        .filter(|lines| !lines.is_empty());
    let lines = match (worded, timed) {
        (Some(worded), Some(timed)) => match (
            pairs_by_place(&side, &worded),
            pairs_by_place(&side, &timed),
        ) {
            (false, true) => Some(timed),
            _ => Some(worded),
        },
        (worded, timed) => worded.or(timed),
    };
    let quiet = sheet.pure_music
        || lines
            .as_deref()
            .is_some_and(crate::lyrics::sheet::instrumental);
    let lyrics = match (lines, quiet) {
        (_, true) => Lyrics::plain(""),
        (Some(mut lines), false) => {
            let artists: Vec<String> = named
                .artist
                .split(", ")
                .map(str::trim)
                .filter(|artist| !artist.is_empty())
                .map(str::to_owned)
                .collect();
            if !crate::lyrics::sheet::headed(&mut lines, &named.title, &artists) {
                return None;
            }
            add_tracks(&mut lines, translation.as_deref());
            Lyrics::Synced {
                lines: lines.into(),
            }
        }
        (None, false) => return None,
    };

    Some(LyricsHit {
        source: SOURCE,
        trust,
        lyrics,
        instrumental: quiet,
        title: named.title.clone(),
        artist: named.artist.clone(),
        album: named.album.clone(),
        duration: (!named.duration.is_zero()).then_some(named.duration),
        writers: [&sheet.yrc, &sheet.lrc]
            .into_iter()
            .filter_map(lyric)
            .flat_map(|text| writers(&text))
            .fold(Vec::new(), |mut writers, name| {
                if !writers.contains(&name) {
                    writers.push(name);
                }
                writers
            }),
    })
}

/// The record a search result names, so a sheet found by name is scored against what the search
/// matched rather than against the track that was playing.
fn song_query(song: &Song) -> LyricsQuery {
    LyricsQuery {
        title: song.name.clone(),
        artist: song
            .artists
            .iter()
            .filter_map(|artist| artist.name.clone())
            .collect::<Vec<_>>()
            .join(", "),
        album: song.album.as_ref().and_then(|album| album.name.clone()),
        duration: Duration::from_millis(song.duration),
        track: None,
    }
}

#[derive(Deserialize)]
struct Credit {
    #[serde(default)]
    c: Vec<Piece>,
}

#[derive(Deserialize)]
struct Piece {
    tx: Option<String>,
}

fn writers(text: &str) -> Vec<String> {
    let mut writers = Vec::new();
    for line in text.lines().filter(|line| line.starts_with('{')) {
        let Ok(credit) = serde_json::from_str::<Credit>(line) else {
            continue;
        };
        let credit: String = credit.c.into_iter().filter_map(|piece| piece.tx).collect();
        let Some((label, names)) = credit.split_once(':').or_else(|| credit.split_once('：'))
        else {
            continue;
        };
        if !label.contains("作词") && !label.contains("作曲") {
            continue;
        }
        for name in names.split('/') {
            let name = name.trim().to_owned();
            if !name.is_empty() && !writers.contains(&name) {
                writers.push(name);
            }
        }
    }
    writers
}

fn parse_yrc(yrc: &str) -> Vec<LyricsLine> {
    let mut lines: Vec<LyricsLine> = yrc.lines().filter_map(read_yrc).collect();
    lrc::normalize(&mut lines);
    lines
}

/// Gives each line whatever the sheet's own side sheets hold at its time, one entry per side
/// sheet in the order the sheet files them. The site times those the way it times the lyric, so
/// they are paired by their stamps rather than by their order, and a line a side sheet does not
/// name keeps an empty entry of its own so the tracks stay lined up.
fn add_tracks(lines: &mut [LyricsLine], translation: Option<&str>) {
    let Some(text) = translation else {
        return;
    };
    let stamped = stamped_lines(text);
    if stamped.is_empty() {
        return;
    }
    let side = Side {
        pairs: pairs_by_place(&stamped, lines),
        stamped,
    };
    for (place, line) in lines.iter_mut().enumerate() {
        line.tracks.push(side.words(line.start, place));
    }
}

/// The translation the site wrote, with the times it was written at, and whether those times can be
/// taken as one per line of the lyric.
struct Side {
    stamped: Vec<(Duration, String)>,
    pairs: bool,
}

impl Side {
    /// The words this sheet holds for the line at `place`, which starts at `time`. The site times
    /// its side sheets against its own sheets and the two can disagree by the better part of a
    /// second on a line without their order differing, so a sheet written for exactly this many
    /// lines is read by place; anything else takes the nearest time within reach, and a line with
    /// nothing to take keeps an empty entry so the tracks stay lined up.
    fn words(&self, at: Duration, place: usize) -> String {
        if self.pairs
            && let Some((_, words)) = self.stamped.get(place)
        {
            return words.clone();
        }
        self.stamped
            .iter()
            .min_by_key(|(stamp, _)| stamp.as_millis().abs_diff(at.as_millis()))
            .filter(|(stamp, _)| stamp.as_millis().abs_diff(at.as_millis()) <= SIDE_NEAR)
            .map(|(_, words)| words.clone())
            .unwrap_or_default()
    }
}

/// Whether a side sheet holds one line for each line of the lyric, closely enough in time to be
/// read by place. A sheet that skipped or added a line would otherwise shift every line after it,
/// which reading by time does not do.
fn pairs_by_place(stamped: &[(Duration, String)], lines: &[LyricsLine]) -> bool {
    if stamped.len() != lines.len() {
        return false;
    }
    let mut drift: Vec<u128> = stamped
        .iter()
        .zip(lines)
        .map(|((stamp, _), line)| stamp.as_millis().abs_diff(line.start.as_millis()))
        .collect();
    drift.sort_unstable();
    drift[drift.len() / 2] <= PAIRED_NEAR
}

/// A side sheet as the time and the words of each of its lines. A stamp that names no words, and
/// the sheet's own offset tag, are left out the way a lyric sheet's are.
fn stamped_lines(text: &str) -> Vec<(Duration, String)> {
    let mut found = Vec::new();
    for line in text.lines() {
        let mut rest = line.trim_start();
        let mut stamps = Vec::new();
        while let Some(tail) = rest.strip_prefix('[') {
            let Some((stamp, after)) = tail.split_once(']') else {
                break;
            };
            let Some(at) = lrc::stamp_of(stamp) else {
                break;
            };
            stamps.push(at);
            rest = after.trim_start();
        }
        // A stamped line with nothing on it is kept: the site leaves the lines of an instrumental
        // break empty, and dropping them would leave a side sheet one entry short of the lyric's
        // lines, so the two could no longer be read by place.
        let words = rest.trim().to_owned();
        for at in stamps {
            found.push((at, words.clone()));
        }
    }
    found
}

fn read_yrc(line: &str) -> Option<LyricsLine> {
    let (header, rest) = line.strip_prefix('[')?.split_once(']')?;
    let (start, span) = pair_of(header)?;

    let mut words = Vec::new();
    let mut text = String::new();
    let mut rest = rest;
    while let Some(open) = rest.find('(') {
        let tail = &rest[open + 1..];
        let Some(shut) = tail.find(')') else { break };
        match stamp_of(&tail[..shut]) {
            Some((at, length)) => {
                grow(&mut words, &mut text, &rest[..open]);
                rest = &tail[shut + 1..];
                let spoken = rest.find('(').map(|next| &rest[..next]).unwrap_or(rest);
                words.push(LyricsWord {
                    start: at,
                    end: at + length,
                    text: spoken.to_owned(),
                });
                text.push_str(spoken);
                rest = &rest[spoken.len()..];
            }
            None => {
                grow(&mut words, &mut text, &rest[..open + shut + 2]);
                rest = &tail[shut + 1..];
            }
        }
    }
    grow(&mut words, &mut text, rest);

    let text = text.trim().to_owned();
    if text.is_empty() {
        return None;
    }
    Some(LyricsLine {
        start,
        end: Some(start + span),
        words: (!words.is_empty()).then_some(words),
        text,
        romanized: None,
        tracks: Vec::new(),
        secondary: Vec::new(),
        voice: Voice::Lead,
    })
}

fn grow(words: &mut [LyricsWord], text: &mut String, tail: &str) {
    if tail.is_empty() {
        return;
    }
    text.push_str(tail);
    if let Some(last) = words.last_mut() {
        last.text.push_str(tail);
    }
}

fn pair_of(header: &str) -> Option<(Duration, Duration)> {
    let (start, span) = header.split_once(',')?;
    Some((
        Duration::from_millis(start.trim().parse().ok()?),
        Duration::from_millis(span.trim().parse().ok()?),
    ))
}

fn stamp_of(stamp: &str) -> Option<(Duration, Duration)> {
    let mut parts = stamp.split(',');
    let start: u64 = parts.next()?.trim().parse().ok()?;
    let span: u64 = parts.next()?.trim().parse().ok()?;
    parts.next()?;
    Some((Duration::from_millis(start), Duration::from_millis(span)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_yrc_line() {
        let lines = parse_yrc(
            "{\"t\":0,\"c\":[{\"tx\":\"credits\"}]}\n[27360,1290](27360,240,0)I've (27600,90,0)been (27690,360,0)tryna (28050,600,0)call\n",
        );

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].start, Duration::from_millis(27_360));
        assert_eq!(lines[0].end, Some(Duration::from_millis(28_650)));
        assert_eq!(lines[0].text, "I've been tryna call");

        let words = lines[0].words.as_ref().expect("the line is worded");
        assert_eq!(words.len(), 4);
        assert_eq!(words[0].text, "I've ");
        assert_eq!(words[1].start, Duration::from_millis(27_600));
        assert_eq!(words[3].end, Duration::from_millis(28_650));
    }

    #[test]
    fn credit_headers_name_the_writers() {
        let text = "{\"t\":0,\"c\":[{\"tx\":\"作词: \"},{\"tx\":\"Abel Tesfaye\"},{\"tx\":\"/\"},{\"tx\":\"Max Martin\"}]}\n{\"t\":1,\"c\":[{\"tx\":\"作曲: \"},{\"tx\":\"Max Martin\"}]}\n{\"t\":2,\"c\":[{\"tx\":\"制作人: \"},{\"tx\":\"Oscar Holter\"}]}\n[1000,2000](1000,500,0)la\n";

        assert_eq!(
            writers(text),
            vec!["Abel Tesfaye".to_owned(), "Max Martin".to_owned()]
        );
    }

    #[test]
    fn untimed_parentheses_become_a_background_lane() {
        let lines = parse_yrc("[1000,2000](1000,500,0)la （la） (1500,500,0)again\n");

        assert_eq!(lines[0].text, "la again");
        let words = lines[0].words.as_ref().expect("the line is worded");
        assert_eq!(words[0].text.trim(), "la");
        assert_eq!(lines[0].secondary[0].text, "(la)");
        assert_eq!(lines[0].secondary[0].start, Duration::from_millis(1000));
    }

    /// The query the two shortlist tests answer for.
    fn named(title: &str, artist: &str, seconds: u64) -> LyricsQuery {
        LyricsQuery {
            title: title.to_owned(),
            artist: artist.to_owned(),
            album: None,
            duration: Duration::from_secs(seconds),
            track: None,
        }
    }

    #[test]
    fn the_closest_durations_make_the_shortlist() {
        let song = |id: u64, duration: u64| Song {
            id,
            name: "Jaded".to_owned(),
            duration,
            artists: vec![Named {
                name: Some("Spiritbox".to_owned()),
            }],
            album: None,
        };
        let songs = vec![
            song(1, 100_000),
            song(2, 263_000),
            song(3, 262_000),
            song(4, 500_000),
            song(5, 264_000),
        ];

        let picked = shortlist(songs, &named("Jaded", "Spiritbox", 263));
        let ids: Vec<u64> = picked.iter().map(|song| song.id).collect();
        assert_eq!(ids, vec![2, 3, 5]);
    }

    #[test]
    fn a_song_by_someone_else_is_no_candidate() {
        let song = |id: u64, name: &str, artist: &str, duration: u64| Song {
            id,
            name: name.to_owned(),
            duration,
            artists: vec![Named {
                name: Some(artist.to_owned()),
            }],
            album: None,
        };
        // another song of the artist's, a cover by someone else, and the recording itself, all
        // within a few seconds of each other
        let songs = vec![
            song(
                1,
                "Ender Ember",
                "MYTH & ROID, TK from 凛として時雨",
                263_000,
            ),
            song(2, "Jaded", "Some Cover Band", 263_000),
            song(3, "Jaded", "Spiritbox", 264_000),
        ];

        let picked = shortlist(songs, &named("Jaded", "Spiritbox", 263));
        let ids: Vec<u64> = picked.iter().map(|song| song.id).collect();
        assert_eq!(ids, vec![3]);
    }
}
