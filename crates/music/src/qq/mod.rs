//! QQ Music's lyrics: the site's own search, and the words it hands over for a song with the
//! translation it writes line for line beside them.

use std::time::Duration;

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;

use crate::lyrics::{lrc, sheet};
use crate::{Lyrics, LyricsHit, LyricsLine, LyricsProvider, LyricsQuery};

const SOURCE: &str = crate::lyrics::QQ;
/// The site's search. The long-standing `client_search_cp` endpoint beside it now answers every
/// search with a server error, and the mobile gateway answers most of them with an error code of its
/// own, so this is the one that still finds anything.
const SEARCH: &str = "https://c.y.qq.com/soso/fcgi-bin/search_for_qq_cp";
const LYRIC: &str = "https://u.y.qq.com/cgi-bin/musicu.fcg";
/// How many songs a name search may answer with, and how many of them are fetched a sheet for.
const HITS: usize = 8;
const CANDIDATES: usize = 3;
/// How far a translated line may start from the line it belongs to and still be taken for it, for a
/// sheet whose two languages the site does not count line for line.
const NEAR: Duration = Duration::from_millis(1_000);
/// How long a label may be for a line that opens with one and a colon to be read as a credit rather
/// than a line of the words.
const CREDIT_REACH: usize = 24;
/// The whole labels the site writes in English, which have to match the label exactly so that a line
/// of words that happens to open the same way is not taken for one.
const CREDITS: &[&str] = &[
    "arranged by",
    "bass",
    "chorus",
    "composed by",
    "drums",
    "executive produce",
    "guitar",
    "lyrics by",
    "mastered by",
    "mastering",
    "mix",
    "mixed by",
    "music by",
    "piano",
    "produce",
    "produced by",
    "program",
    "recorded by",
    "recording",
    "recording & mix",
    "sound produce",
    "strings",
    "vocal",
    "vocals",
    "written by",
];
/// The words the site opens its Chinese labels with, which are short enough to be read as labels on
/// their own.
const CREDIT_STEMS: &[&str] = &[
    "作词", "作曲", "编曲", "制作", "混音", "录音", "母带", "和声", "吉他", "贝斯", "鼓", "钢琴",
    "弦乐", "词", "曲",
];
const AGENT: &str = concat!(
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36 sonora/",
    env!("CARGO_PKG_VERSION")
);
const REFERER: &str = "https://y.qq.com/";

pub struct QQ {
    http: reqwest::Client,
}

impl QQ {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }

    /// The songs the site lists for a name, in the order it ranks them.
    async fn songs(&self, wanted: &str) -> Result<Vec<Song>> {
        let response = self
            .http
            .get(SEARCH)
            .query(&[
                ("format", "json"),
                ("p", "1"),
                ("n", &HITS.to_string()),
                ("w", wanted),
            ])
            .header("User-Agent", AGENT)
            .header("Referer", REFERER)
            .send()
            .await
            .context("cannot reach qq music")?;
        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("qq music answered with status {status}");
        }
        let answer: SearchReply = response
            .json()
            .await
            .context("cannot read the qq music search response")?;
        Ok(answer.data.song.list)
    }

    /// The words the site holds for a song, and the translation it wrote beside them.
    async fn lyric(&self, mid: &str) -> Result<Option<Verses>> {
        let ask = LyricAsk {
            comm: Comm {
                ct: 24,
                cv: 0,
                uin: "0",
                format: "json",
            },
            req: Want {
                module: "music.musichallSong.PlayLyricInfo",
                method: "GetPlayLyricInfo",
                param: Param {
                    mid: mid.to_owned(),
                    id: 0,
                    trans: 1,
                    roma: 0,
                    crypt: 0,
                    lrc_type: 0,
                    qrc: 0,
                },
            },
        };
        let response = self
            .http
            .post(LYRIC)
            .header("User-Agent", AGENT)
            .header("Referer", REFERER)
            .json(&ask)
            .send()
            .await
            .context("cannot reach qq music")?;
        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("qq music answered with status {status}");
        }
        let answer: LyricReply = response
            .json()
            .await
            .context("cannot read the qq music lyric response")?;
        Ok(answer
            .req
            .data
            .filter(|verses| !text(&verses.lyric).trim().is_empty()))
    }
}

impl Default for QQ {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LyricsProvider for QQ {
    fn name(&self) -> &'static str {
        SOURCE
    }

    async fn search(&self, query: &LyricsQuery) -> Result<Vec<LyricsHit>> {
        // The site is asked by name, then by the title alone. A service that romanizes its titles
        // calls a Japanese song by its reading, which names nothing the site holds it under, and
        // the title alone still finds it.
        let wanted = format!("{} {}", query.title, query.artist);
        let mut songs = shortlist(self.songs(&wanted).await?, query);
        if songs.is_empty() {
            songs = shortlist(self.songs(&query.title).await?, query);
        }

        let mut tasks = JoinSet::new();
        for song in songs {
            let qq = Self {
                http: self.http.clone(),
            };
            let title = query.title.clone();
            // a search that named the title and came back with a sheet whose own title cannot be
            // compared with it matched something only the site can see
            let trust = crate::lyrics::answered_by_title(&song.title, &title);
            tasks.spawn(async move {
                let verses = qq
                    .lyric(&song.mid)
                    .await
                    .inspect_err(|error| {
                        log::warn!("lyrics: qq music did not hand over {}: {error:#}", song.mid)
                    })
                    .ok()??;
                hit(&song, &verses, &title, trust)
            });
        }

        let mut hits = Vec::new();
        while let Some(found) = tasks.join_next().await {
            hits.extend(found.ok().flatten());
        }
        Ok(hits)
    }
}

/// One song as the site's search lists it: what it is called, who sings it, and how long it runs.
/// The search writes its fields the short way, which the aliases map onto the names the rest of the
/// module reads: the fuller json api beside it is not the one that answers.
#[derive(Deserialize)]
struct Song {
    #[serde(alias = "songmid")]
    mid: String,
    #[serde(default, alias = "songname")]
    title: String,
    #[serde(default)]
    interval: u64,
    #[serde(default)]
    singer: Vec<Named>,
    #[serde(default)]
    albumname: Option<String>,
}

#[derive(Deserialize)]
struct Named {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct SearchReply {
    data: SearchData,
}

#[derive(Deserialize)]
struct SearchData {
    song: SearchSongs,
}

#[derive(Deserialize)]
struct SearchSongs {
    #[serde(default)]
    list: Vec<Song>,
}

#[derive(Serialize)]
struct LyricAsk {
    comm: Comm,
    req: Want,
}

#[derive(Serialize)]
struct Comm {
    ct: u32,
    cv: u32,
    uin: &'static str,
    format: &'static str,
}

#[derive(Serialize)]
struct Want {
    module: &'static str,
    method: &'static str,
    param: Param,
}

#[derive(Serialize)]
struct Param {
    #[serde(rename = "songMID")]
    mid: String,
    #[serde(rename = "songID")]
    id: u32,
    trans: u32,
    roma: u32,
    crypt: u32,
    #[serde(rename = "lrcType")]
    lrc_type: u32,
    qrc: u32,
}

#[derive(Deserialize)]
struct LyricReply {
    req: LyricPayload,
}

#[derive(Deserialize)]
struct LyricPayload {
    #[serde(default)]
    data: Option<Verses>,
}

/// The two sheets the site answers with: the words, and the translation it wrote beside them.
#[derive(Deserialize)]
struct Verses {
    #[serde(default)]
    lyric: String,
    #[serde(default)]
    trans: String,
}

/// The songs worth fetching sheets for, out of what the site answered with.
fn shortlist(songs: Vec<Song>, query: &LyricsQuery) -> Vec<Song> {
    crate::lyrics::shortlist(
        songs,
        query,
        CANDIDATES,
        |song| (song.title.clone(), credited(song)),
        |song| song.interval.abs_diff(query.duration.as_secs()),
    )
}

/// One sheet as a hit, named from the record the search matched, with the translation the site wrote
/// beside its words as the track the panel can switch to.
fn hit(song: &Song, verses: &Verses, title: &str, trust: u32) -> Option<LyricsHit> {
    let lines = lrc::parse(&text(&verses.lyric));
    if lines.is_empty() {
        return None;
    }
    let mut lines = dressed(
        lines,
        &lrc::parse(&text(&verses.trans)),
        &credited_parts(song),
    );
    if !sheet::headed(&mut lines, title, &credited_parts(song)) {
        return None;
    }
    if lines.is_empty() {
        return None;
    }

    Some(LyricsHit {
        source: SOURCE,
        trust,
        lyrics: Lyrics::Synced {
            lines: lines.into(),
        },
        instrumental: false,
        title: song.title.clone(),
        artist: credited(song),
        album: song.albumname.clone(),
        duration: (song.interval > 0).then(|| Duration::from_secs(song.interval)),
        writers: Vec::new(),
    })
}

/// The words with the site's translation of each line beside them, and the credits it writes above
/// them taken off. A sheet the site counts line for line is paired by place; one it does not is
/// paired by the time each line starts.
fn dressed(
    lines: Vec<LyricsLine>,
    translation: &[LyricsLine],
    credits: &[String],
) -> Vec<LyricsLine> {
    let by_place = translation.len() == lines.len();
    let mut sides: Vec<Option<String>> = lines
        .iter()
        .enumerate()
        .map(|(place, line)| beside(translation, line, place, by_place))
        .collect();
    let mut words = false;
    let mut dressed: Vec<LyricsLine> = Vec::with_capacity(lines.len());
    for (line, side) in lines.into_iter().zip(sides.iter_mut()) {
        let side = side.take();
        // The site writes the credits above the words rather than in them, and says which lines
        // they are with a bare slash where a translation would be, or leaves them to be read as a
        // label and a colon. Nothing past the first line of words is dropped for either.
        let marked = side.as_deref().is_some_and(slashes);
        // The sheet also names itself and its singer above the words, which the shared header check
        // takes off afterwards, so that line does not count as the words having started.
        let own = {
            let text = line.text.to_lowercase();
            credits
                .iter()
                .any(|name| text.contains(&name.to_lowercase()))
        };
        if !words && !own && (marked || credit(&line.text)) {
            continue;
        }
        words |= !own;
        match side.filter(|text| !slashes(text)) {
            Some(text) => dressed.push(LyricsLine {
                tracks: vec![text],
                ..line
            }),
            None => dressed.push(line),
        }
    }
    dressed
}

/// The site's line for the line of words it belongs to: the one in the same place, when the two
/// count the same lines, and otherwise the one that starts nearest, within a window.
fn beside(side: &[LyricsLine], line: &LyricsLine, place: usize, by_place: bool) -> Option<String> {
    let found = match by_place {
        true => side.get(place),
        false => side
            .iter()
            .find(|side| side.start.abs_diff(line.start) <= NEAR),
    }?;
    let text = found.text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// Whether a line is one of the marks the site writes where a translation would be: a bare slash.
fn slashes(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|letter| letter == '/')
}

/// Whether a line reads as one of the credits the site writes beside the words rather than a line of
/// them: a label and a colon, which is how it writes who wrote and played what.
fn credit(text: &str) -> bool {
    let Some((label, _)) = text.split_once(['：', ':']) else {
        return false;
    };
    let label = label.trim().to_lowercase();
    if label.is_empty() || label.chars().count() > CREDIT_REACH {
        return false;
    }
    CREDITS.contains(&label.as_str()) || CREDIT_STEMS.iter().any(|stem| label.starts_with(stem))
}

/// The text a field of the answer carries: the site answers with its sheet in base64, and in the
/// clear when the field already holds a sheet with its own tags.
fn text(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() || value.starts_with('[') {
        return value.to_owned();
    }
    match STANDARD.decode(value) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => value.to_owned(),
    }
}

/// The credits the site lists for a song, as the one line the rest of the matching reads.
fn credited(song: &Song) -> String {
    credited_parts(song).join("、")
}

/// The site's credit line, read as the names it is made of, which is how a sheet's own header is
/// compared against it.
fn credited_parts(song: &Song) -> Vec<String> {
    song.singer
        .iter()
        .filter_map(|singer| singer.name.clone())
        .filter(|name| !name.is_empty())
        .collect()
}
