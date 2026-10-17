//! The folder, read: every audio file's tags, gathered into tracks, albums
//! and artists in the provider protocol's shapes.
//!
//! Ids are hashes, so they stay the same across scans and restarts: a
//! track's of its path within the folder, an album's of its artist and
//! title, an artist's of their name. They are kept under 2^53, which is as
//! far as a JavaScript number is exact, since the app's web view reads them.
//!
//! Reading tags is the slow part, so what each file said is kept in an index
//! beside the folder, by path, size and modification time, and a file that
//! has not changed since is not read again.

use anyhow::{Context, Result};
use lofty::file::FileType;
use lofty::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use stelly_provider::{Album, AlbumTracks, Artist, Credit, Quality, SearchResults, Track};

/// What a file's extension has to be for it to be looked at.
const AUDIO: [&str; 12] = ["mp3", "flac", "ogg", "oga", "opus", "m4a", "mp4", "aac", "wav", "aif", "aiff", "wv"];
/// A folder's own cover, when the files carry none, by these names.
const COVERS: [&str; 4] = ["cover", "folder", "front", "album"];

/// What one file said, as the index keeps it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scanned {
    /// Within the folder, with `/` between components.
    pub path: String,
    pub size: u64,
    pub modified: u64,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub date: Option<String>,
    pub isrc: Option<String>,
    pub track: Option<u32>,
    pub disc: Option<u32>,
    pub seconds: Option<u64>,
    pub quality: Quality,
    pub mime_type: String,
    /// Whether the file carries a picture of its own.
    pub picture: bool,
}

/// A track, and where its audio is.
#[derive(Debug, Clone)]
pub struct File {
    pub track: Track,
    pub path: PathBuf,
    pub quality: Quality,
    pub mime_type: String,
    order: (u32, u32, String),
}

/// Where an album's cover comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum Cover {
    /// Embedded in this file.
    Embedded(PathBuf),
    /// An image beside the files.
    Image(PathBuf),
}

#[derive(Debug, Default)]
pub struct Library {
    pub root: PathBuf,
    pub tracks: HashMap<i64, File>,
    pub albums: HashMap<String, (Album, Vec<i64>)>,
    pub artists: HashMap<i64, Artist>,
    pub covers: HashMap<String, Cover>,
}

/// An id from `parts`, stable, positive and under 2^53.
fn hash(parts: &[&str]) -> u64 {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    u64::from_le_bytes(digest[..8].try_into().expect("a digest has eight bytes")) & ((1 << 53) - 1)
}

/// Names as they are compared: case and surrounding space do not make a
/// second artist.
fn key(name: &str) -> String {
    name.trim().to_lowercase()
}

pub fn track_id(path: &str) -> i64 {
    hash(&["track", path]).max(1) as i64
}

pub fn artist_id(name: &str) -> i64 {
    hash(&["artist", &key(name)]).max(1) as i64
}

pub fn album_id(artist: &str, title: &str) -> String {
    format!("l{:014x}", hash(&["album", &key(artist), &key(title)]))
}

// --------------------------------------------------------------- scanning

/// The index from last time, by path; nothing if there is none.
pub fn load_index(path: &Path) -> HashMap<String, Scanned> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Vec<Scanned>>(&bytes).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|scanned| (scanned.path.clone(), scanned))
        .collect()
}

pub fn save_index(path: &Path, scanned: &[Scanned]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let partial = path.with_extension("json.part");
    std::fs::write(&partial, serde_json::to_vec(scanned)?).with_context(|| format!("writing {}", partial.display()))?;
    std::fs::rename(&partial, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Every audio file under `root`, read again only if it changed since
/// `known` saw it. A file that cannot be read is left out, and said so.
pub fn scan(root: &Path, known: &HashMap<String, Scanned>) -> Vec<Scanned> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(root).follow_links(true).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let audio = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| AUDIO.contains(&e.to_ascii_lowercase().as_str()));
        if !audio {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else { continue };
        let relative = relative.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
        let Ok(meta) = entry.metadata() else { continue };
        let modified = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        if let Some(seen) = known.get(&relative).filter(|seen| seen.size == meta.len() && seen.modified == modified) {
            out.push(seen.clone());
            continue;
        }
        match read(path, relative.clone(), meta.len(), modified) {
            Ok(scanned) => out.push(scanned),
            Err(err) => eprintln!("skipping {relative}: {err:#}"),
        }
    }
    out
}

fn read(path: &Path, relative: String, size: u64, modified: u64) -> Result<Scanned> {
    let file = lofty::read_from_path(path).context("reading its tags")?;
    let properties = file.properties();
    let tag = file.primary_tag().or_else(|| file.first_tag());
    let text = |key: ItemKey| {
        tag.and_then(|t| t.get_string(key))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let date = tag
        .and_then(|t| t.date())
        .map(|d| d.to_string())
        .or_else(|| text(ItemKey::ReleaseDate))
        .or_else(|| text(ItemKey::OriginalReleaseDate))
        .map(|d| d.get(..10).unwrap_or(&d).to_string());

    let lossless = |depth: Option<u8>, rate: Option<u32>| {
        if depth.unwrap_or(16) > 16 || rate.unwrap_or(44_100) > 48_000 {
            Quality::Hires
        } else {
            Quality::Lossless
        }
    };
    let (depth, rate) = (properties.bit_depth(), properties.sample_rate());
    let (quality, mime_type) = match file.file_type() {
        FileType::Mpeg => (Quality::Lossy, "audio/mpeg"),
        FileType::Flac => (lossless(depth, rate), "audio/flac"),
        FileType::Opus => (Quality::Lossy, "audio/opus"),
        FileType::Vorbis | FileType::Speex => (Quality::Lossy, "audio/ogg"),
        FileType::Aac => (Quality::Lossy, "audio/aac"),
        // ALAC says its bit depth, AAC has none.
        FileType::Mp4 if depth.is_some() => (lossless(depth, rate), "audio/mp4"),
        FileType::Mp4 => (Quality::Lossy, "audio/mp4"),
        FileType::Wav => (lossless(depth, rate), "audio/wav"),
        FileType::Aiff => (lossless(depth, rate), "audio/aiff"),
        FileType::WavPack => (lossless(depth, rate), "audio/x-wavpack"),
        _ => (Quality::Lossless, "application/octet-stream"),
    };

    Ok(Scanned {
        path: relative,
        size,
        modified,
        title: text(ItemKey::TrackTitle),
        artist: text(ItemKey::TrackArtist),
        album: text(ItemKey::AlbumTitle),
        album_artist: text(ItemKey::AlbumArtist),
        genre: text(ItemKey::Genre),
        date,
        isrc: text(ItemKey::Isrc),
        track: tag.and_then(|t| t.track()),
        disc: tag.and_then(|t| t.disk()),
        seconds: Some(properties.duration().as_secs()).filter(|&s| s > 0),
        quality,
        mime_type: mime_type.to_string(),
        picture: tag.is_some_and(|t| !t.pictures().is_empty()),
    })
}

// --------------------------------------------------------------- the library

impl Library {
    /// What `scanned` describes, under `root`, its covers served from
    /// `base`. Untagged files are named after themselves and their folders:
    /// `Artist/Album/01 Title.flac`.
    pub fn build(root: &Path, base: &str, scanned: &[Scanned]) -> Self {
        let mut library = Library { root: root.to_path_buf(), ..Default::default() };
        let mut album_dirs: BTreeMap<String, PathBuf> = BTreeMap::new();

        for file in scanned {
            let parts: Vec<&str> = file.path.split('/').collect();
            let stem = Path::new(parts.last().copied().unwrap_or_default())
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let folder = |back: usize| parts.len().checked_sub(back + 1).and_then(|i| parts.get(i)).map(|s| s.to_string());

            let title = file.title.clone().unwrap_or(stem);
            let artist = file.artist.clone().or_else(|| folder(2)).unwrap_or_else(|| "Unknown Artist".into());
            let album_artist = file.album_artist.clone().unwrap_or_else(|| artist.clone());
            let album_title = file.album.clone().or_else(|| folder(1)).unwrap_or_else(|| "Unknown Album".into());

            let id = track_id(&file.path);
            let artist_id = artist_id(&artist);
            let album_artist_id = self::artist_id(&album_artist);
            let album_id = album_id(&album_artist, &album_title);
            let image = Some(format!("{base}/v1/covers/{album_id}"));

            for (id, name) in [(artist_id, &artist), (album_artist_id, &album_artist)] {
                library.artists.entry(id).or_insert_with(|| Artist { id, name: name.clone(), ..Default::default() });
            }

            let (album, tracks) = library.albums.entry(album_id.clone()).or_insert_with(|| {
                let album = Album {
                    id: album_id.clone(),
                    title: album_title.clone(),
                    artist: album_artist.clone(),
                    artist_id: Some(album_artist_id),
                    released: file.date.clone(),
                    genre: file.genre.clone(),
                    image: image.clone(),
                    kind: None,
                    ..Default::default()
                };
                (album, Vec::new())
            });
            album.released = album.released.clone().or_else(|| file.date.clone());
            album.genre = album.genre.clone().or_else(|| file.genre.clone());
            tracks.push(id);

            let path = root.join(&file.path);
            if file.picture {
                library.covers.entry(album_id.clone()).or_insert_with(|| Cover::Embedded(path.clone()));
            }
            if let Some(dir) = path.parent() {
                album_dirs.entry(album_id.clone()).or_insert_with(|| dir.to_path_buf());
            }

            library.tracks.insert(
                id,
                File {
                    track: Track {
                        id,
                        title,
                        artist: artist.clone(),
                        artist_id: Some(artist_id),
                        album: album_title,
                        album_id: Some(album_id),
                        duration: file.seconds.map(|s| s as i64),
                        streamable: true,
                        hires: file.quality == Quality::Hires,
                        image,
                        isrc: file.isrc.clone(),
                        released: file.date.clone(),
                        performers: None,
                        liked_at: None,
                    },
                    path,
                    quality: file.quality,
                    mime_type: file.mime_type.clone(),
                    order: (file.disc.unwrap_or(1), file.track.unwrap_or(u32::MAX), file.path.clone()),
                },
            );
        }

        // A folder's image for albums whose files carry none.
        for (album_id, dir) in album_dirs {
            if library.covers.contains_key(&album_id) {
                continue;
            }
            if let Some(image) = folder_cover(&dir) {
                library.covers.insert(album_id, Cover::Image(image));
            }
        }
        for (album_id, (album, tracks)) in library.albums.iter_mut() {
            let files = &library.tracks;
            tracks.sort_by(|a, b| files[a].order.cmp(&files[b].order));
            album.tracks_count = Some(tracks.len() as i64);
            if !library.covers.contains_key(album_id) {
                album.image = None;
            }
        }
        // Tracks of an album without a cover have none either.
        let covered: std::collections::HashSet<String> = library.covers.keys().cloned().collect();
        for file in library.tracks.values_mut() {
            if !file.track.album_id.as_ref().is_some_and(|id| covered.contains(id)) {
                file.track.image = None;
            }
        }
        let mut counts: HashMap<i64, i64> = HashMap::new();
        for (album, _) in library.albums.values() {
            if let Some(id) = album.artist_id {
                *counts.entry(id).or_default() += 1;
            }
        }
        for artist in library.artists.values_mut() {
            artist.albums_count = Some(counts.get(&artist.id).copied().unwrap_or(0));
            artist.image = library
                .albums
                .values()
                .find(|(album, _)| album.artist_id == Some(artist.id) && album.image.is_some())
                .and_then(|(album, _)| album.image.clone());
        }
        library
    }

    pub fn album(&self, id: &str) -> Option<AlbumTracks> {
        let (album, ids) = self.albums.get(id)?;
        Some(AlbumTracks {
            album: album.clone(),
            tracks: ids.iter().filter_map(|id| self.tracks.get(id)).map(|f| f.track.clone()).collect(),
        })
    }

    /// An artist's albums, newest first: theirs as album artist, and those
    /// they appear on otherwise.
    pub fn releases(&self, artist_id: i64, own: bool) -> Vec<Album> {
        let mut out: Vec<Album> = self
            .albums
            .values()
            .filter_map(|(album, ids)| {
                let credit = if album.artist_id == Some(artist_id) {
                    Credit::Main
                } else if ids.iter().any(|id| self.tracks.get(id).is_some_and(|f| f.track.artist_id == Some(artist_id))) {
                    Credit::Appears
                } else {
                    return None;
                };
                (!own || credit == Credit::Main).then(|| Album { credit: Some(credit), ..album.clone() })
            })
            .collect();
        out.sort_by(|a, b| b.released.cmp(&a.released).then_with(|| a.title.cmp(&b.title)));
        out
    }

    /// Up to `limit` of each kind matching every word of `query` in a name,
    /// a title or an album, those starting with the first word first.
    pub fn search(&self, query: &str, limit: usize) -> SearchResults {
        let words: Vec<String> = query.split_whitespace().map(key).collect();
        let matches = |fields: &[&str]| {
            let fields: Vec<String> = fields.iter().map(|f| key(f)).collect();
            !words.is_empty() && words.iter().all(|w| fields.iter().any(|f| f.contains(w.as_str())))
        };
        let first = |name: &str| u8::from(!words.first().is_some_and(|w| key(name).starts_with(w.as_str())));

        let mut tracks: Vec<&Track> = self
            .tracks
            .values()
            .map(|f| &f.track)
            .filter(|t| matches(&[&t.title, &t.artist, &t.album]))
            .collect();
        tracks.sort_by(|a, b| (first(&a.title), &a.artist, &a.album, &a.title).cmp(&(first(&b.title), &b.artist, &b.album, &b.title)));
        let mut albums: Vec<&Album> = self.albums.values().map(|(a, _)| a).filter(|a| matches(&[&a.title, &a.artist])).collect();
        albums.sort_by(|a, b| (first(&a.title), &a.title).cmp(&(first(&b.title), &b.title)));
        let mut artists: Vec<&Artist> = self.artists.values().filter(|a| matches(&[&a.name])).collect();
        artists.sort_by(|a, b| (first(&a.name), &a.name).cmp(&(first(&b.name), &b.name)));

        SearchResults {
            tracks: tracks.into_iter().take(limit).cloned().collect(),
            albums: albums.into_iter().take(limit).cloned().collect(),
            artists: artists.into_iter().take(limit).cloned().collect(),
        }
    }

    /// The cover's bytes and type.
    pub fn cover(&self, album_id: &str) -> Option<(Vec<u8>, String)> {
        match self.covers.get(album_id)? {
            Cover::Image(path) => {
                let mime = match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
                    Some("png") => "image/png",
                    Some("webp") => "image/webp",
                    _ => "image/jpeg",
                };
                Some((std::fs::read(path).ok()?, mime.to_string()))
            }
            Cover::Embedded(path) => {
                let file = lofty::read_from_path(path).ok()?;
                let tag = file.primary_tag().or_else(|| file.first_tag())?;
                let pictures = tag.pictures();
                let picture = pictures
                    .iter()
                    .find(|p| p.pic_type() == lofty::picture::PictureType::CoverFront)
                    .or_else(|| pictures.first())?;
                let mime = picture.mime_type().map_or("image/jpeg", |m| m.as_str()).to_string();
                Some((picture.data().to_vec(), mime))
            }
        }
    }
}

/// `cover.jpg` and its like, in `dir`.
fn folder_cover(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut images: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| ["jpg", "jpeg", "png", "webp"].contains(&e.to_ascii_lowercase().as_str()))
        })
        .collect();
    images.sort();
    let named = |p: &PathBuf| {
        p.file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| COVERS.contains(&s.to_ascii_lowercase().as_str()))
    };
    images.iter().find(|p| named(p)).or_else(|| images.first()).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scanned(path: &str, title: Option<&str>, artist: Option<&str>, album: Option<&str>, track: Option<u32>) -> Scanned {
        Scanned {
            path: path.into(),
            size: 1,
            modified: 1,
            title: title.map(str::to_string),
            artist: artist.map(str::to_string),
            album: album.map(str::to_string),
            album_artist: None,
            genre: None,
            date: Some("2021-03-04".into()),
            isrc: None,
            track,
            disc: None,
            seconds: Some(200),
            quality: Quality::Lossless,
            mime_type: "audio/flac".into(),
            picture: false,
        }
    }

    #[test]
    fn ids_are_stable_and_small_enough_for_javascript() {
        assert_eq!(track_id("a/b.flac"), track_id("a/b.flac"));
        assert_ne!(track_id("a/b.flac"), track_id("a/c.flac"));
        assert_eq!(artist_id("Fleuves"), artist_id(" fleuves "));
        assert!(track_id("x") > 0 && track_id("x") < 1 << 53);
        assert_eq!(album_id("Fleuves", "Fleuves").len(), 15);
    }

    #[test]
    fn files_gather_into_albums_in_track_order() {
        let library = Library::build(
            Path::new("/music"),
            "http://music",
            &[
                scanned("Fleuves/Fleuves/02.flac", Some("Le fil"), Some("Fleuves"), Some("Fleuves"), Some(2)),
                scanned("Fleuves/Fleuves/01.flac", Some("Dañs plinn"), Some("Fleuves"), Some("Fleuves"), Some(1)),
                // Untagged: named after its folders and itself.
                scanned("Lun/Ifern/03 Gwerz.mp3", None, None, None, None),
            ],
        );
        assert_eq!(library.tracks.len(), 3);
        assert_eq!(library.albums.len(), 2);

        let album = library.album(&album_id("Fleuves", "Fleuves")).unwrap();
        let titles: Vec<&str> = album.tracks.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, ["Dañs plinn", "Le fil"]);
        assert_eq!(album.album.tracks_count, Some(2));
        assert_eq!(album.album.released.as_deref(), Some("2021-03-04"));
        // No picture and no folder image: no cover to point at.
        assert_eq!(album.album.image, None);

        let untagged = &library.album(&album_id("Lun", "Ifern")).unwrap().tracks[0];
        assert_eq!((untagged.title.as_str(), untagged.artist.as_str()), ("03 Gwerz", "Lun"));

        let found = library.search("fle fil", 10);
        assert_eq!(found.tracks.len(), 1);
        assert_eq!(found.tracks[0].title, "Le fil");
        assert_eq!(library.search("lun", 10).artists[0].name, "Lun");

        let releases = library.releases(artist_id("Fleuves"), true);
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].credit, Some(Credit::Main));
    }
}
