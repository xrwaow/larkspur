//! Search — the query language, the matcher, and the album grouping both
//! search results and the browse view are rendered from.
//!
//! Framework-agnostic and pure: `parse` turns text into a [`Query`], `matches`
//! decides whether one song satisfies it, and [`search`] applies that over a
//! [`Library`]. The UI never implements matching itself — it parses a query
//! and renders the [`AlbumGroup`]s that come back.
//!
//! # Query language
//!
//! Terms are whitespace-separated; `"..."` groups a phrase. A term whose
//! prefix is a known keyword is a directive, anything else is free text.
//! **Free words are ANDed** — every word has to appear, in any order, which is
//! what makes `nancy sinatra` find "Nancy Sinatra" and "Sinatra, Nancy" alike.
//!
//! ```text
//! type:MODE            exact | contains | fuzzy        (default: contains)
//! from:FIELD[,FIELD]   artist | album | song | lyrics  (default: all)
//! artist:TEXT  album:TEXT  title:TEXT  lyrics:TEXT     (per-term field scope)
//! year:SPEC            YYYY | YYYY-YYYY | >=YYYY | <=YYYY | >YYYY | <YYYY
//! dur:SPEC             M:SS | M:SS-M:SS | >M:SS | <M:SS   (alias: length:)
//! ext:LIST             flac,mp3,…                         (alias: file:)
//! ```
//!
//! Every keyword also has a bracket spelling, `[keyword]{TEXT}` — one scoped
//! directive per bracket group, so several fields can be pinned in one query
//! (`[title]{love} [lyrics]{night}`), and the text may contain spaces without
//! quotes:
//!
//! ```text
//! [title]{bang bang} [year]{2003}
//! [lyrics]{the coldest night} [dur]{<5:00}
//! ```
//!
//! Examples:
//!
//! ```text
//! sinatra                          # any field, substring
//! type:fuzzy gnga                  # subsequence: "gnga" -> "GINGA"
//! from:artist type:exact sinatra   # the word "sinatra" in an artist name
//! title:"bang bang" year:2003
//! dur:>5:00 ext:flac
//! ```
//!
//! Match modes, precisely:
//!
//! - `contains` — the term is a substring of the field.
//! - `exact` — the term equals the whole field, or one whitespace-delimited
//!   word of it.
//! - `fuzzy` — the term's characters appear in the field in order, with gaps.
//!
//! An unrecognised keyword-shaped prefix (`yeer:2003`) is reported as a
//! warning and kept as free text, so a typo degrades instead of erroring.

use std::time::Duration;

use super::identity::SongId;
use super::library::Library;
use super::playlist::PlaylistId;
use super::select::{Order, Scope, Selection};
use super::song::{ReleaseDate, SongMetadata};

/// How a single term is compared against a field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MatchMode {
    /// Whole field, or one word of it, equals the term.
    Exact,
    /// The term is a substring of the field.
    #[default]
    Contains,
    /// The term's characters appear in order, gaps allowed.
    Fuzzy,
}

/// A field of a song that a term can be matched against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Field {
    Artist,
    Album,
    Song,
    Lyrics,
}

impl Field {
    /// Every searchable field — the default scope for a free word.
    pub const ALL: [Field; 4] = [Field::Artist, Field::Album, Field::Song, Field::Lyrics];

    fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "artist" | "artists" => Some(Field::Artist),
            "album" => Some(Field::Album),
            "song" | "title" | "track" => Some(Field::Song),
            "lyrics" => Some(Field::Lyrics),
            _ => None,
        }
    }
}

/// A numeric or path restriction that isn't a text match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    /// Inclusive year bounds; a song with no year never matches.
    Year { min: Option<u16>, max: Option<u16> },
    /// Inclusive duration bounds.
    Duration { min: Option<Duration>, max: Option<Duration> },
    /// Accepted file extensions, lowercase and without the dot.
    Extension(Vec<String>),
}

/// One search term: free text, or text scoped to a single field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term {
    /// Matched against the query's field scope.
    Any(String),
    /// Matched against exactly this field.
    In(Field, String),
}

/// A parsed query: text terms, their scope and mode, plus filters.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Query {
    pub terms: Vec<Term>,
    /// Fields free words search. Empty means all of [`Field::ALL`].
    pub fields: Vec<Field>,
    pub mode: MatchMode,
    pub filters: Vec<Filter>,
}

impl Query {
    /// The fields free words are matched against.
    fn scope(&self) -> &[Field] {
        if self.fields.is_empty() {
            &Field::ALL
        } else {
            &self.fields
        }
    }

    /// Whether the query would match every song (no terms and no filters).
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty() && self.filters.is_empty()
    }

    /// Whether `song` satisfies every term and every filter.
    ///
    /// Convenience for a one-off check; a caller matching many songs should
    /// build a [`Matcher`] once instead, so the field scope and needed fields
    /// aren't re-derived per song.
    pub fn matches(&self, song: &SongMetadata) -> bool {
        self.matcher().matches(song)
    }

    /// A reusable matcher over this query.
    pub fn matcher(&self) -> Matcher<'_> {
        Matcher { query: self, needed: self.needed_fields() }
    }

    /// Only the fields this query actually reads, so lyrics aren't lowercased
    /// (or even joined) for a query that never touches them.
    fn needed_fields(&self) -> Vec<Field> {
        let mut needed: Vec<Field> = Vec::new();
        for term in &self.terms {
            let fields: &[Field] = match term {
                Term::Any(_) => self.scope(),
                Term::In(field, _) => std::slice::from_ref(field),
            };
            for field in fields {
                if !needed.contains(field) {
                    needed.push(*field);
                }
            }
        }
        needed
    }
}

/// A [`Query`] with its field scope and needed fields resolved once, so
/// matching many songs doesn't re-derive them per song.
pub struct Matcher<'a> {
    query: &'a Query,
    needed: Vec<Field>,
}

impl Matcher<'_> {
    /// Whether `song` satisfies every term and every filter.
    pub fn matches(&self, song: &SongMetadata) -> bool {
        if !self.query.filters.iter().all(|f| f.matches(song)) {
            return false;
        }
        if self.query.terms.is_empty() {
            return true;
        }
        let haystack = Haystack::new(song, &self.needed);
        self.query
            .terms
            .iter()
            .all(|term| haystack.matches_term(term, self.query.scope(), self.query.mode))
    }
}

impl Filter {
    fn matches(&self, song: &SongMetadata) -> bool {
        match self {
            Filter::Year { min, max } => song
                .date
                .map(ReleaseDate::year)
                .is_some_and(|y| min.is_none_or(|m| y >= m) && max.is_none_or(|m| y <= m)),
            Filter::Duration { min, max } => {
                min.is_none_or(|m| song.duration >= m) && max.is_none_or(|m| song.duration <= m)
            }
            Filter::Extension(exts) => song
                .path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| exts.iter().any(|x| x == &e.to_ascii_lowercase())),
        }
    }
}

/// A song's searchable fields, lowercased once per query.
///
/// Only the fields the query needs are built — the lyrics join in particular
/// is expensive enough to be worth skipping when nothing searches it.
#[derive(Default)]
struct Haystack {
    artist: Option<String>,
    album: Option<String>,
    song: Option<String>,
    lyrics: Option<String>,
}

impl Haystack {
    fn new(song: &SongMetadata, needed: &[Field]) -> Self {
        let mut haystack = Self::default();
        for field in needed {
            match field {
                Field::Artist => {
                    let mut names = song.artists.clone();
                    if let Some(album_artist) = &song.album_artist {
                        if !names.contains(album_artist) {
                            names.push(album_artist.clone());
                        }
                    }
                    haystack.artist = Some(names.join(", ").to_lowercase());
                }
                Field::Album => haystack.album = Some(song.album().to_lowercase()),
                Field::Song => haystack.song = Some(song.display_title().to_lowercase()),
                Field::Lyrics => {
                    haystack.lyrics = song.lyrics.text().map(|text| text.to_lowercase())
                }
            }
        }
        haystack
    }

    fn field(&self, field: Field) -> Option<&str> {
        match field {
            Field::Artist => self.artist.as_deref(),
            Field::Album => self.album.as_deref(),
            Field::Song => self.song.as_deref(),
            Field::Lyrics => self.lyrics.as_deref(),
        }
    }

    fn matches_term(&self, term: &Term, scope: &[Field], mode: MatchMode) -> bool {
        match term {
            Term::Any(text) => {
                scope.iter().any(|f| self.field(*f).is_some_and(|h| matches_text(h, text, mode)))
            }
            Term::In(field, text) => {
                self.field(*field).is_some_and(|h| matches_text(h, text, mode))
            }
        }
    }
}

/// Whether `needle` matches the (already lowercased) `haystack`.
fn matches_text(haystack: &str, needle: &str, mode: MatchMode) -> bool {
    let needle = needle.trim().to_lowercase();
    if needle.is_empty() {
        return true;
    }
    match mode {
        MatchMode::Contains => haystack.contains(&needle),
        MatchMode::Exact => {
            haystack == needle || haystack.split_whitespace().any(|word| word == needle)
        }
        MatchMode::Fuzzy => is_subsequence(haystack, &needle),
    }
}

/// Whether `needle`'s characters appear in `haystack` in order, with gaps.
fn is_subsequence(haystack: &str, needle: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|want| chars.any(|have| have == want))
}

/// Songs matching a query, grouped by the album playlist they belong to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumGroup {
    /// The autogen album playlist this group is headed by — its title, artist,
    /// cover, and track order all come from here.
    pub playlist: PlaylistId,
    /// The matched songs, in album track order.
    pub songs: Vec<SongId>,
}

/// Every album playlist with all of its songs — the browse view's list.
///
/// The "easy search" the browse container uses: no query, just the library
/// grouped the way the UI wants to draw it.
pub fn all_albums(library: &Library) -> Vec<AlbumGroup> {
    library.select(&Selection {
        scope: Scope::Library,
        query: Query::default(),
        order: Order::AlbumTitle,
    })
}

/// Run a query over the library, grouped by album.
pub fn search(library: &Library, query: &Query) -> Vec<AlbumGroup> {
    library.select(&Selection {
        scope: Scope::Library,
        query: query.clone(),
        order: Order::AlbumTitle,
    })
}

/// Song ids matching a query, flattened out of their album groups in album and
/// track order — the shape a flat "add songs" list wants.
///
/// An empty query matches the whole library, which is what makes an empty
/// search box read as "everything you could add".
pub fn matching_songs(library: &Library, query: &Query) -> Vec<SongId> {
    library.select_songs(&Selection {
        scope: Scope::Library,
        query: query.clone(),
        order: Order::AlbumTitle,
    })
}

/// Parse a query string into a [`Query`] plus any warnings worth showing.
///
/// Warnings are advisory: the parse always succeeds, so the UI can re-parse on
/// every keystroke without handling failure.
pub fn parse(input: &str) -> (Query, Vec<String>) {
    let mut query = Query::default();
    let mut warnings = Vec::new();

    for token in tokenize(input) {
        // `[field]{text}` is the bracket spelling of `field:text` — same
        // keywords, same handling below.
        let (key, value, bracket) = if let Some((key, value)) = split_bracket(&token) {
            (key, value, true)
        } else if let Some((key, value)) = split_keyword(&token) {
            (key, value, false)
        } else {
            query.terms.push(Term::Any(unquote(&token)));
            continue;
        };
        let key = key.to_ascii_lowercase();
        let value = unquote(value);
        let unknown = |key: &str| {
            if bracket {
                format!("unknown keyword `[{key}]{{…}}`")
            } else {
                format!("unknown keyword `{key}:`")
            }
        };

        match key.as_str() {
            "type" => match value.as_str() {
                "exact" => query.mode = MatchMode::Exact,
                "contains" => query.mode = MatchMode::Contains,
                "fuzzy" => query.mode = MatchMode::Fuzzy,
                other => warnings.push(format!("unknown match mode `{other}`")),
            },
            "from" => {
                for name in value.split(',') {
                    let name = name.trim();
                    // `from:all` clears the scope rather than naming a field.
                    if matches!(name.to_ascii_lowercase().as_str(), "all" | "any") {
                        query.fields.clear();
                        continue;
                    }
                    match Field::parse(name) {
                        Some(field) if !query.fields.contains(&field) => query.fields.push(field),
                        Some(_) => {}
                        None => warnings.push(format!("unknown field `{name}`")),
                    }
                }
            }
            "artist" | "album" | "title" | "song" | "lyrics" => {
                let field = match key.as_str() {
                    "artist" => Field::Artist,
                    "album" => Field::Album,
                    "lyrics" => Field::Lyrics,
                    _ => Field::Song,
                };
                query.terms.push(Term::In(field, value));
            }
            "year" | "date" => match parse_year_spec(&value) {
                Ok(filter) => query.filters.push(filter),
                Err(warning) => warnings.push(warning),
            },
            "dur" | "length" => match parse_duration_spec(&value) {
                Ok(filter) => query.filters.push(filter),
                Err(warning) => warnings.push(warning),
            },
            "ext" | "file" | "format" => {
                let extensions: Vec<String> = value
                    .split(',')
                    .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
                    .filter(|e| !e.is_empty())
                    .collect();
                if extensions.is_empty() {
                    warnings.push("empty extension filter".into());
                } else {
                    query.filters.push(Filter::Extension(extensions));
                }
            }
            // A keyword-shaped prefix we don't know is almost certainly a typo
            // (`yeer:2003`). Say so, but keep searching for it as free text.
            _ => {
                warnings.push(unknown(&key));
                query.terms.push(Term::Any(unquote(&token)));
            }
        }
    }

    (query, warnings)
}

/// Split on whitespace, keeping `"quoted runs"` together. The quote
/// characters stay in the token so `unquote` can strip them afterwards.
///
/// A `[field]{…}` group also holds together across whitespace, so a scoped
/// text with spaces stays one term until the closing brace.
fn tokenize(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut bracketed = false;
    for ch in input.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                current.push(ch);
            }
            // A `[` only opens a group at the start of a term; elsewhere it's
            // just a character.
            '[' if current.is_empty() && !quoted => {
                bracketed = true;
                current.push(ch);
            }
            '}' if bracketed => {
                bracketed = false;
                current.push(ch);
                tokens.push(std::mem::take(&mut current));
            }
            c if c.is_whitespace() && !quoted && !bracketed => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// `[field]{value}` — the bracket spelling of `field:value`.
///
/// Returns the raw field and value, so `parse` lowercases and unquotes them
/// exactly like the colon form. An unclosed group (mid-typing) yields `None`
/// and the token falls through as free text.
fn split_bracket(token: &str) -> Option<(&str, &str)> {
    let inner = token.strip_prefix('[')?;
    let (field, value) = inner.split_once("]{")?;
    let value = value.strip_suffix('}')?;
    let field = field.trim();
    (!field.is_empty()).then_some((field, value))
}

/// `key:value` when `key` is a bare word, so `3:00` and `http://…` stay free
/// text instead of being read as unknown keywords.
fn split_keyword(token: &str) -> Option<(&str, &str)> {
    let (key, value) = token.split_once(':')?;
    let is_word = !key.is_empty() && key.chars().all(|c| c.is_ascii_alphabetic());
    is_word.then_some((key, value))
}

/// Strip one pair of surrounding quotes, if present.
fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// `YYYY`, `YYYY-YYYY`, or `>=`/`<=`/`>`/`<` against a year.
///
/// `>`/`<` are exclusive: `year:>2003` means 2004 onwards.
fn parse_year_spec(spec: &str) -> Result<Filter, String> {
    let spec = spec.trim();
    let (min, max) = if let Some(rest) = spec.strip_prefix(">=") {
        (Some(parse_year(rest)?), None)
    } else if let Some(rest) = spec.strip_prefix("<=") {
        (None, Some(parse_year(rest)?))
    } else if let Some(rest) = spec.strip_prefix('>') {
        (Some(parse_year(rest)?.saturating_add(1)), None)
    } else if let Some(rest) = spec.strip_prefix('<') {
        (None, Some(parse_year(rest)?.saturating_sub(1)))
    } else if let Some((from, to)) = spec.split_once('-') {
        (Some(parse_year(from)?), Some(parse_year(to)?))
    } else {
        let year = parse_year(spec)?;
        (Some(year), Some(year))
    };
    Ok(Filter::Year { min, max })
}

fn parse_year(s: &str) -> Result<u16, String> {
    let s = s.trim();
    s.parse::<u16>().map_err(|_| format!("bad year `{s}`"))
}

/// `M:SS`/`H:MM:SS`/plain seconds, a `from-to` range, or `>`/`<`.
///
/// `>`/`<` are exclusive, matching the year filter: `dur:>5:00` means strictly
/// longer than five minutes.
fn parse_duration_spec(spec: &str) -> Result<Filter, String> {
    let spec = spec.trim();
    let (min, max) = if let Some(rest) = spec.strip_prefix(">=") {
        (Some(parse_duration(rest)?), None)
    } else if let Some(rest) = spec.strip_prefix("<=") {
        (None, Some(parse_duration(rest)?))
    } else if let Some(rest) = spec.strip_prefix('>') {
        (Some(parse_duration(rest)? + Duration::from_secs(1)), None)
    } else if let Some(rest) = spec.strip_prefix('<') {
        (
            None,
            Some(parse_duration(rest)?.saturating_sub(Duration::from_secs(1))),
        )
    } else if let Some((from, to)) = spec.split_once('-') {
        (Some(parse_duration(from)?), Some(parse_duration(to)?))
    } else {
        let duration = parse_duration(spec)?;
        (Some(duration), Some(duration))
    };
    Ok(Filter::Duration { min, max })
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let bad = || format!("bad duration `{s}`");
    let parts: Vec<&str> = s.split(':').collect();
    let seconds = match parts.as_slice() {
        [secs] => secs.parse::<f64>().map_err(|_| bad())?,
        [minutes, secs] => {
            let minutes: f64 = minutes.parse().map_err(|_| bad())?;
            let secs: f64 = secs.parse().map_err(|_| bad())?;
            minutes * 60.0 + secs
        }
        [hours, minutes, secs] => {
            let hours: f64 = hours.parse().map_err(|_| bad())?;
            let minutes: f64 = minutes.parse().map_err(|_| bad())?;
            let secs: f64 = secs.parse().map_err(|_| bad())?;
            hours * 3600.0 + minutes * 60.0 + secs
        }
        _ => return Err(bad()),
    };
    Ok(Duration::from_secs_f64(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Lyrics, ReleaseDate};

    fn song(id: SongId, title: &str, artist: &str, album: &str) -> SongMetadata {
        SongMetadata {
            id,
            path: format!("/music/{album}/{title}.flac").into(),
            song_name: Some(title.into()),
            artists: vec![artist.into()],
            album_name: Some(album.into()),
            album_artist: Some(artist.into()),
            track_position: None,
            date: None,
            nominal_bitrate: Some(320_000),
            lyrics: Lyrics::None,
            duration: Duration::from_secs(180),
            has_art: false,
        }
    }

    fn run(input: &str, song: &SongMetadata) -> bool {
        let (query, _) = parse(input);
        query.matches(song)
    }

    #[test]
    fn free_words_are_anded_in_any_order() {
        let s = song(1, "Bang Bang", "Nancy Sinatra", "Kill Bill");
        assert!(run("sinatra nancy", &s), "both words present, any order");
        assert!(run("nancy sinatra", &s));
        assert!(!run("nancy grimes", &s), "one word missing fails");
    }

    #[test]
    fn contains_is_the_default_mode() {
        let s = song(1, "Bang Bang", "Nancy Sinatra", "Kill Bill");
        assert!(run("\"ang b\"", &s), "a quoted phrase may span a word boundary");
        assert!(run("ang", &s), "partial words match by default");
        assert!(!run("grimes", &s));
    }

    #[test]
    fn exact_matches_whole_words_only() {
        let s = song(1, "Bang Bang", "Nancy Sinatra", "Kill Bill");
        assert!(run("type:exact bang", &s), "whole word");
        assert!(!run("type:exact ang", &s), "partial word is not exact");
        assert!(run("type:exact \"bang bang\"", &s), "whole field");
    }

    #[test]
    fn fuzzy_matches_subsequences() {
        let s = song(1, "0004:a galaxy odyssey", "GINGA", "GINGA");
        assert!(run("type:fuzzy gnga", &s), "g-n-g-a is a subsequence of GINGA");
        assert!(!run("type:fuzzy gnaa", &s), "out of order does not match");
    }

    #[test]
    fn from_scopes_free_words() {
        let s = song(1, "Bang Bang", "Nancy Sinatra", "Kill Bill");
        assert!(run("from:artist sinatra", &s));
        assert!(!run("from:album sinatra", &s), "sinatra is not in the album");
        assert!(run("from:artist,song bang", &s), "comma-separated scope");
    }

    #[test]
    fn field_prefixed_terms_scope_a_single_term() {
        let s = song(1, "Bang Bang", "Nancy Sinatra", "Kill Bill");
        assert!(run("artist:sinatra album:\"kill bill\"", &s));
        assert!(!run("artist:bang", &s));
    }

    #[test]
    fn lyrics_are_searchable() {
        let mut s = song(1, "Song", "Artist", "Album");
        s.lyrics = Lyrics::Plain("I saw the sunset on the sea".into());
        assert!(run("lyrics:sunset", &s));
        assert!(run("from:lyrics sea", &s));
        assert!(!run("from:artist sea", &s), "the term is scoped away from lyrics");

        let mut synced = song(2, "Song", "Artist", "Album");
        synced.lyrics = Lyrics::Synced(vec![crate::model::LyricLine {
            timestamp: Duration::from_secs(1),
            text: "hello world".into(),
        }]);
        assert!(run("lyrics:world", &synced));
    }

    #[test]
    fn year_filters_accept_ranges_and_comparisons() {
        let mut s = song(1, "Song", "Artist", "Album");
        s.date = Some(ReleaseDate::new(2003, None, None));
        assert!(run("year:2003", &s));
        assert!(run("year:2000-2010", &s));
        assert!(!run("year:2012", &s));
        assert!(run("year:>=2003", &s));
        assert!(!run("year:>2003", &s), "> is exclusive");
        assert!(run("year:<2004", &s));
        assert!(!run("year:<2003", &s));
    }

    #[test]
    fn songs_without_a_year_fail_year_filters() {
        let s = song(1, "Song", "Artist", "Album");
        assert!(!run("year:2000-2030", &s));
    }

    #[test]
    fn duration_filters_accept_ranges_and_comparisons() {
        let mut s = song(1, "Song", "Artist", "Album");
        s.duration = Duration::from_secs(200); // 3:20
        assert!(run("dur:3:20", &s));
        assert!(run("dur:2:00-4:00", &s));
        assert!(!run("dur:>5:00", &s));
        assert!(run("dur:>3:00", &s));
        assert!(run("dur:200", &s), "plain seconds");
        assert!(!run("length:<3:00", &s));
    }

    #[test]
    fn extension_filters_match_the_path() {
        let s = song(1, "Song", "Artist", "Album"); // .flac
        assert!(run("ext:flac", &s));
        assert!(run("ext:mp3,flac", &s));
        assert!(!run("ext:mp3", &s));
    }

    #[test]
    fn from_all_clears_the_scope() {
        let (query, warnings) = parse("from:all anything");
        assert!(query.fields.is_empty(), "no restriction");
        assert!(warnings.is_empty(), "`all` is a valid field name");
    }

    #[test]
    fn quoted_phrases_stay_one_term() {
        let (query, warnings) = parse("title:\"bang bang\"");
        assert!(warnings.is_empty());
        assert_eq!(query.terms, vec![Term::In(Field::Song, "bang bang".into())]);
    }

    #[test]
    fn bracket_groups_scope_one_term_each() {
        let (query, warnings) = parse("[title]{text1} [lyrics]{text2}");
        assert!(warnings.is_empty());
        assert_eq!(
            query.terms,
            vec![
                Term::In(Field::Song, "text1".into()),
                Term::In(Field::Lyrics, "text2".into()),
            ]
        );
    }

    #[test]
    fn bracket_text_may_contain_spaces_and_quotes() {
        let (query, warnings) = parse("[title]{bang bang} [year]{>2000}");
        assert!(warnings.is_empty());
        assert_eq!(query.terms, vec![Term::In(Field::Song, "bang bang".into())]);
        assert_eq!(query.filters.len(), 1, "the year filter came through");

        let (query, warnings) = parse("[album]{\"the album\"}");
        assert!(warnings.is_empty());
        assert_eq!(query.terms, vec![Term::In(Field::Album, "the album".into())]);
    }

    #[test]
    fn bracket_groups_carry_every_keyword() {
        let (query, warnings) = parse("[type]{fuzzy} [artist]{nancy} [dur]{<5:00} [ext]{flac}");
        assert!(warnings.is_empty());
        assert_eq!(query.mode, MatchMode::Fuzzy);
        assert_eq!(query.terms, vec![Term::In(Field::Artist, "nancy".into())]);
        assert_eq!(query.filters.len(), 2);
    }

    #[test]
    fn unknown_bracket_fields_warn_but_still_search() {
        let (query, warnings) = parse("[yeer]{2003}");
        assert_eq!(query.terms, vec![Term::Any("[yeer]{2003}".into())]);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn an_unclosed_bracket_group_is_free_text() {
        // Mid-typing: `[title]{foo` hasn't closed yet, so it's not a directive.
        let (query, warnings) = parse("[title]{foo");
        assert!(warnings.is_empty());
        assert_eq!(query.terms, vec![Term::Any("[title]{foo".into())]);
    }

    #[test]
    fn unknown_keywords_warn_but_still_search() {
        let (query, warnings) = parse("yeer:2003");
        assert_eq!(query.terms, vec![Term::Any("yeer:2003".into())]);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn times_are_not_mistaken_for_keywords() {
        let (query, warnings) = parse("3:00");
        assert_eq!(query.terms, vec![Term::Any("3:00".into())]);
        assert!(warnings.is_empty(), "a bare time is free text, not a bad keyword");
    }

    #[test]
    fn empty_query_is_empty_and_matches_everything() {
        let (query, warnings) = parse("   ");
        assert!(query.is_empty());
        assert!(warnings.is_empty());
        assert!(query.matches(&song(1, "Any", "Artist", "Album")));
    }

    /// A library with two albums, in a known order. "Kill Bill" is a
    /// compilation: two track artists, one album artist.
    fn library() -> Library {
        let mut library = Library::default();
        for (id, title, artist, album, album_artist) in [
            (1u64, "Bang Bang", "Nancy Sinatra", "Kill Bill", "Various Artists"),
            (2, "Twisted Nerve", "Bernard Herrmann", "Kill Bill", "Various Artists"),
            (3, "Vide Noir", "Lord Huron", "Vide Noir", "Lord Huron"),
        ] {
            let mut s = song(id, title, artist, album);
            s.album_artist = Some(album_artist.into());
            s.track_position = Some(id as u16);
            library.insert_song(s);
        }
        library.rebuild_auto();
        library
    }

    #[test]
    fn results_group_by_album_in_album_and_track_order() {
        let library = library();
        let (query, _) = parse("kill"); // matches the album name on both tracks
        let groups = search(&library, &query);
        assert_eq!(groups.len(), 1, "both tracks live in one album");
        let group = &groups[0];
        let album = library.playlist(group.playlist).unwrap();
        assert_eq!(album.title(), "Kill Bill");
        assert_eq!(group.songs.len(), 2);
        assert_eq!(group.songs, album.song_ids, "album track order is preserved");
    }

    #[test]
    fn search_only_returns_albums_with_matches() {
        let library = library();
        let (query, _) = parse("lord huron");
        let groups = search(&library, &query);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].songs.len(), 1);
    }

    #[test]
    fn an_empty_query_returns_the_whole_library_grouped() {
        let library = library();
        let (query, _) = parse("");
        let groups = search(&library, &query);
        assert_eq!(groups, all_albums(&library));
        assert_eq!(groups.len(), 2, "two albums");
        assert_eq!(groups.iter().map(|g| g.songs.len()).sum::<usize>(), 3);
    }

    #[test]
    fn album_groups_orders_albums_by_title() {
        let library = library();
        let groups = all_albums(&library);
        let titles: Vec<String> = groups
            .iter()
            .map(|g| library.playlist(g.playlist).unwrap().title().to_string())
            .collect();
        assert_eq!(titles, vec!["Kill Bill", "Vide Noir"], "alphabetical");
    }
}
