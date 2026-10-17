//! `stelly-local`: a folder of music files as a Stelly music provider.
//!
//! ```sh
//! stelly-local scan ~/Music            # what is there, and what cannot be read
//! stelly-local serve ~/Music           # http://127.0.0.1:7710
//! ```
//!
//! Plain HTTP, loopback by default. Listening anywhere else wants
//! `STELLY_PROVIDER_TOKEN` set, which Stelly's server then sends. Stream
//! URLs point back here, so devices playing them have to reach it: say how
//! with `--public-url` when that is not the address it listens on.

mod library;
mod routes;

use anyhow::{bail, Context, Result};
use argh::FromArgs;
use library::Library;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// A folder of music files as a Stelly music provider.
#[derive(FromArgs)]
#[argh(note = "The index of what each file said is kept in $XDG_CACHE_HOME/stelly-local,
or ~/.cache/stelly-local, so a rescan only reads what changed.")]
struct Cli {
    #[argh(subcommand)]
    command: Command,
}

#[derive(FromArgs)]
#[argh(subcommand)]
enum Command {
    Serve(Serve),
    Scan(Scan),
}

/// Answer the provider protocol for the folder.
#[derive(FromArgs)]
#[argh(subcommand, name = "serve")]
struct Serve {
    /// the music folder
    #[argh(positional)]
    music: PathBuf,
    /// where to listen (default 127.0.0.1:7710)
    #[argh(option, default = "\"127.0.0.1:7710\".parse().unwrap()")]
    bind: SocketAddr,
    /// how devices reach this, for the stream and cover URLs it hands out
    /// (default http://<bind>)
    #[argh(option)]
    public_url: Option<String>,
    /// minutes between rescans of the folder; 0 for never (default 30)
    #[argh(option, default = "30")]
    rescan_minutes: u64,
}

/// Read the folder and say what is in it.
#[derive(FromArgs)]
#[argh(subcommand, name = "scan")]
struct Scan {
    /// the music folder
    #[argh(positional)]
    music: PathBuf,
}

fn index_path(music: &Path) -> PathBuf {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    // One index per folder.
    let name: String = music.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    cache.join("stelly-local").join(format!("{name}.json"))
}

/// Scan `music`, keep the index, and build the library.
fn load(music: &Path, base: &str) -> Result<Library> {
    let index = index_path(music);
    let known = library::load_index(&index);
    let scanned = library::scan(music, &known);
    library::save_index(&index, &scanned)?;
    Ok(Library::build(music, base, &scanned))
}

fn main() -> Result<()> {
    let cli: Cli = argh::from_env();
    match cli.command {
        Command::Scan(args) => {
            let music = args.music.canonicalize().with_context(|| format!("no folder at {}", args.music.display()))?;
            let library = load(&music, "")?;
            println!(
                "{}: {} tracks, {} albums, {} artists, {} covers",
                music.display(),
                library.tracks.len(),
                library.albums.len(),
                library.artists.len(),
                library.covers.len()
            );
            Ok(())
        }
        Command::Serve(args) => serve(args),
    }
}

fn serve(args: Serve) -> Result<()> {
    let token = std::env::var("STELLY_PROVIDER_TOKEN").ok().filter(|t| !t.is_empty());
    if !args.bind.ip().is_loopback() && token.is_none() {
        bail!("listening on {} hands out stream URLs to anyone: set STELLY_PROVIDER_TOKEN", args.bind);
    }
    let music = args.music.canonicalize().with_context(|| format!("no folder at {}", args.music.display()))?;
    let base = args.public_url.unwrap_or_else(|| format!("http://{}", args.bind)).trim_end_matches('/').to_string();

    let library = load(&music, &base)?;
    eprintln!("{}: {} tracks, {} albums", music.display(), library.tracks.len(), library.albums.len());
    // The token signs URLs when there is one, so they outlive a restart;
    // otherwise a secret of this run's.
    let secret = match &token {
        Some(token) => token.as_bytes().to_vec(),
        None => {
            let seed = format!("{:?}{}", std::time::SystemTime::now(), std::process::id());
            <sha2::Sha256 as sha2::Digest>::digest(seed.as_bytes()).to_vec()
        }
    };
    let folder = Arc::new(routes::Folder { library: RwLock::new(Arc::new(library)), base: base.clone(), token, secret });

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        if args.rescan_minutes > 0 {
            let (folder, music, base) = (folder.clone(), music.clone(), base.clone());
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(args.rescan_minutes * 60)).await;
                    let (music, base) = (music.clone(), base.clone());
                    match tokio::task::spawn_blocking(move || load(&music, &base)).await {
                        Ok(Ok(library)) => *folder.library.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(library),
                        Ok(Err(err)) => eprintln!("rescanning failed: {err:#}"),
                        Err(err) => eprintln!("rescanning panicked: {err}"),
                    }
                }
            });
        }
        let listener = tokio::net::TcpListener::bind(args.bind).await?;
        println!("stelly-local on http://{}, serving {}", args.bind, music.display());
        axum::serve(listener, routes::router(folder))
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await?;
        Ok(())
    })
}
