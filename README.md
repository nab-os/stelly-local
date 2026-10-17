# stelly-local

A folder of music files as a [Stelly](https://github.com/nab-os/stelly)
music provider: it answers the
[provider protocol](https://github.com/nab-os/stelly-provider) for what is
in the folder, so Stelly can crawl, analyse, map and play it like any
catalogue, with no account anywhere.

```sh
cargo run --release -- scan ~/Music     # what is there
cargo run --release -- serve ~/Music    # http://127.0.0.1:7710
STELLY_PROVIDER_URL=http://127.0.0.1:7710 stelly-server whoami
```

- **Tags** are read with [lofty](https://crates.io/crates/lofty): MP3,
  FLAC, Ogg Vorbis, Opus, M4A (AAC and ALAC), WAV, AIFF, WavPack. A file
  without tags is named after itself and its folders,
  `Artist/Album/01 Title.flac`. What each file said is kept in an index in
  `~/.cache/stelly-local`, so a rescan, every 30 minutes by default
  (`--rescan-minutes`), only reads what changed.
- **Ids** are hashes of a track's path, an album's artist and title, and an
  artist's name: stable across scans and restarts, and under 2^53, as far
  as JavaScript numbers are exact.
- **Covers** are the files' own pictures, or an image beside them
  (`cover.jpg`, `folder.jpg`, `front.jpg`...), at `/v1/covers/{album}`.
- **Streams** are the files themselves, served here with byte ranges, at
  signed URLs that work for six hours. Their quality is what each file is:
  lossy, CD quality, or hi-res past 16 bits or 48 kHz.
- **Capabilities**: no similar artists, so Stelly's crawl stays inside the
  folder; no account likes, playlists or export.

## Reaching it

Stream and cover URLs point back here, and the devices playing them fetch
them directly, so they must reach it: `--public-url` names the address they
should use, when that is not the one it listens on. On loopback it needs no
token; anywhere else it refuses to start without `STELLY_PROVIDER_TOKEN`,
which Stelly's server sends and which also signs the URLs. Past the machine,
put it behind WireGuard, Tailscale or TLS, as you would Stelly's server.

## Docker

```sh
docker build -t stelly-local .
docker run -d -p 127.0.0.1:7710:7710 -v ~/Music:/music:ro \
  -e STELLY_PROVIDER_TOKEN=... -e PUBLIC_URL=http://music.example:7710 stelly-local
```

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
