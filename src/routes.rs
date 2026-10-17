//! The provider protocol's calls, answered from the folder.
//!
//! Stream URLs point back here, at `/v1/files/{id}`, signed and expiring, so
//! they can be handed to a device that holds no token, as every provider's
//! are; covers are not signed, since an `<img>` cannot send one either.

use crate::library::Library;
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Sha256;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use stelly_provider::wire::Problem;
use stelly_provider::{Account, Album, AlbumTracks, Artist, Capabilities, FileUrl, Page, Quality, SearchResults, Track};
use tower::ServiceExt;

/// How long a stream URL works for: longer than any track, shorter than a
/// leaked one should.
const SIGNED_FOR: Duration = Duration::from_secs(6 * 60 * 60);

pub struct Folder {
    pub library: RwLock<Arc<Library>>,
    /// Where devices reach this, for the URLs it hands out.
    pub base: String,
    /// What clients must send as a bearer token, if anything.
    pub token: Option<String>,
    /// Signs stream URLs.
    pub secret: Vec<u8>,
}

impl Folder {
    pub fn library(&self) -> Arc<Library> {
        self.library.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn signature(&self, id: i64, expires: u64) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.secret).expect("any key length works");
        mac.update(format!("{id}.{expires}").as_bytes());
        mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
    }

    fn signed(&self, id: i64, expires: u64, signature: &str) -> bool {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        expires >= now && same(self.signature(id, expires).as_bytes(), signature.as_bytes())
    }
}

type Shared = State<Arc<Folder>>;

pub fn router(folder: Arc<Folder>) -> Router {
    Router::new()
        .route("/v1/capabilities", get(capabilities))
        .route("/v1/account", get(account))
        .route("/v1/search", get(search))
        .route("/v1/tracks/{id}", get(track))
        .route("/v1/tracks/{id}/file", get(file_url))
        .route("/v1/albums/{id}", get(album))
        .route("/v1/artists/{id}", get(artist))
        .route("/v1/artists/{id}/releases", get(artist_releases))
        .route_layer(axum::middleware::from_fn_with_state(folder.clone(), authorise))
        // Outside the token check: up or not; audio by signed URL; covers.
        .route("/v1/health", get(health))
        .route("/v1/files/{id}", get(file))
        .route("/v1/covers/{id}", get(cover))
        .with_state(folder)
}

async fn authorise(State(folder): Shared, request: Request, next: Next) -> Response {
    if let Some(expected) = &folder.token {
        let given = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if !given.is_some_and(|given| same(given.as_bytes(), expected.as_bytes())) {
            return failure(StatusCode::UNAUTHORIZED, "a bearer token is required, STELLY_PROVIDER_TOKEN's");
        }
    }
    next.run(request).await
}

/// Equal, in time that does not depend on where they first differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn failure(status: StatusCode, error: &str) -> Response {
    (status, Json(Problem { error: error.to_string() })).into_response()
}

fn missing(what: &str) -> Response {
    failure(StatusCode::NOT_FOUND, &format!("no {what} in the folder"))
}

type Reply<T> = Result<Json<T>, Response>;

async fn health() -> Json<Value> {
    Json(json!({ "ok": true }))
}

async fn capabilities(State(folder): Shared) -> Json<Capabilities> {
    let library = folder.library();
    let mut qualities: Vec<Quality> = library.tracks.values().map(|f| f.quality).collect();
    qualities.sort();
    qualities.dedup();
    Json(Capabilities {
        name: "local".into(),
        title: "Music folder".into(),
        qualities,
        ..Default::default()
    })
}

async fn account(State(folder): Shared) -> Json<Account> {
    let library = folder.library();
    Json(Account {
        name: Some(library.root.display().to_string()),
        subscription: Some(format!("{} tracks", library.tracks.len())),
        ..Default::default()
    })
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    10
}

async fn search(State(folder): Shared, Query(query): Query<SearchQuery>) -> Json<SearchResults> {
    Json(folder.library().search(&query.q, query.limit))
}

async fn track(State(folder): Shared, Path(id): Path<i64>) -> Reply<Track> {
    let library = folder.library();
    let file = library.tracks.get(&id).ok_or_else(|| missing("such track"))?;
    Ok(Json(file.track.clone()))
}

/// The file as it is, whatever was asked for: there is only one.
async fn file_url(State(folder): Shared, Path(id): Path<i64>) -> Reply<FileUrl> {
    let library = folder.library();
    let file = library.tracks.get(&id).ok_or_else(|| missing("such track"))?;
    let expires = (SystemTime::now() + SIGNED_FOR).duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    Ok(Json(FileUrl {
        url: format!("{}/v1/files/{id}?expires={expires}&signature={}", folder.base, folder.signature(id, expires)),
        quality: file.quality,
        mime_type: Some(file.mime_type.clone()),
        sample: false,
        duration: file.track.duration.map(|d| d as f64),
        constant_bitrate: None,
    }))
}

#[derive(Deserialize)]
struct Signed {
    expires: u64,
    signature: String,
}

/// The audio, ranges and all, to whoever holds a signed URL for it.
async fn file(State(folder): Shared, Path(id): Path<i64>, Query(signed): Query<Signed>, request: Request) -> Response {
    if !folder.signed(id, signed.expires, &signed.signature) {
        return failure(StatusCode::FORBIDDEN, "this URL is not signed, or no longer");
    }
    let library = folder.library();
    let Some(file) = library.tracks.get(&id) else { return missing("such track") };
    let served = tower_http::services::ServeFile::new(&file.path).oneshot(request).await;
    match served {
        Ok(response) => {
            let mut response = response.map(Body::new);
            // What the tags said it is, rather than a guess from the name.
            if let Ok(mime) = file.mime_type.parse() {
                response.headers_mut().insert(header::CONTENT_TYPE, mime);
            }
            response
        }
        Err(err) => failure(StatusCode::INTERNAL_SERVER_ERROR, &format!("reading the file: {err}")),
    }
}

async fn cover(State(folder): Shared, Path(id): Path<String>) -> Response {
    match folder.library().cover(&id) {
        Some((bytes, mime)) => ([(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "max-age=86400".into())], bytes).into_response(),
        None => missing("cover for that album"),
    }
}

async fn album(State(folder): Shared, Path(id): Path<String>) -> Reply<AlbumTracks> {
    Ok(Json(folder.library().album(&id).ok_or_else(|| missing("such album"))?))
}

async fn artist(State(folder): Shared, Path(id): Path<i64>) -> Reply<Artist> {
    let library = folder.library();
    Ok(Json(library.artists.get(&id).cloned().ok_or_else(|| missing("such artist"))?))
}

#[derive(Deserialize)]
struct ReleasesQuery {
    #[serde(default)]
    own: bool,
    #[serde(default = "default_cap")]
    cap: usize,
}

fn default_cap() -> usize {
    500
}

async fn artist_releases(State(folder): Shared, Path(id): Path<i64>, Query(query): Query<ReleasesQuery>) -> Reply<Page<Album>> {
    let library = folder.library();
    if !library.artists.contains_key(&id) {
        return Err(missing("such artist"));
    }
    let mut items = library.releases(id, query.own);
    let total = items.len();
    items.truncate(query.cap);
    Ok(Json(Page { items, total }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use stelly_provider::{api, Config, ProviderError};

    /// A second of silence as a WAV file, 16-bit mono at 44.1 kHz.
    fn wav() -> Vec<u8> {
        let data = vec![0u8; 44_100 * 2];
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&44_100u32.to_le_bytes());
        out.extend_from_slice(&88_200u32.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        out
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_folder_through_the_protocol() {
        let root = std::env::temp_dir().join(format!("stelly-local-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let album = root.join("Lun").join("Ifern");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("01 Gwerz.wav"), wav()).unwrap();
        std::fs::write(album.join("02 Kan.wav"), wav()).unwrap();
        std::fs::write(album.join("cover.jpg"), b"not really a jpeg").unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let scanned = crate::library::scan(&root, &Default::default());
        let library = Library::build(&root, &base, &scanned);
        let folder = Arc::new(Folder {
            library: RwLock::new(Arc::new(library)),
            base: base.clone(),
            token: Some("family".into()),
            secret: b"family".to_vec(),
        });
        tokio::spawn(async move { axum::serve(listener, router(folder)).await.unwrap() });
        let client = stelly_provider::Client::new(Config { url: base.clone(), token: Some("family".into()) });

        let found = client.send(api::search("lun", 10)).await.unwrap();
        assert_eq!(found.artists[0].name, "Lun");
        let album = client.send(api::album(&found.albums[0].id)).await.unwrap();
        let titles: Vec<&str> = album.tracks.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, ["01 Gwerz", "02 Kan"]);
        assert_eq!(album.tracks[0].duration, Some(1));

        // The cover beside the files, without a token, as an <img> asks.
        let cover = album.album.image.unwrap();
        assert_eq!(fetch(&cover, None).await, (200, b"not really a jpeg".to_vec()));

        // The audio, by signed URL, a range of it.
        let file = client.send(api::file_url(album.tracks[0].id, Quality::Lossy)).await.unwrap();
        assert_eq!((file.quality, file.mime_type.as_deref()), (Quality::Lossless, Some("audio/wav")));
        let (status, bytes) = fetch(&file.url, Some("bytes=0-3")).await;
        assert_eq!((status, bytes.as_slice()), (206, &b"RIFF"[..]));
        let forged = file.url.replace("signature=", "signature=0");
        assert_eq!(fetch(&forged, None).await.0, 403);

        // What it cannot do, it says it cannot.
        let similar = client.send(api::similar_artists(found.artists[0].id, 5)).await.unwrap_err();
        assert_eq!(similar.downcast_ref::<ProviderError>().unwrap().status, 404);
        assert!(!client.send(api::capabilities()).await.unwrap().similar_artists);

        // And without the token, nothing but health, files and covers.
        let stranger = stelly_provider::Client::new(Config { url: base, token: None });
        let refused = stranger.send(api::search("lun", 10)).await.unwrap_err();
        assert_eq!(refused.downcast_ref::<ProviderError>().unwrap().status, 401);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A plain GET, as a device makes it: no token.
    async fn fetch(url: &str, range: Option<&str>) -> (u16, Vec<u8>) {
        let url = url.to_string();
        let range = range.map(str::to_string);
        tokio::task::spawn_blocking(move || {
            let mut request = ureq_agent().get(&url);
            if let Some(range) = range {
                request = request.header("Range", &range);
            }
            let mut response = request.call().unwrap();
            let status = response.status().as_u16();
            (status, response.body_mut().read_to_vec().unwrap_or_default())
        })
        .await
        .unwrap()
    }

    fn ureq_agent() -> ureq::Agent {
        ureq::Agent::config_builder().http_status_as_error(false).build().new_agent()
    }
}
