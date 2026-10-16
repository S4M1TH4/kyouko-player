# Third-party notices

For kyouko-player 0.1.0, default Windows x86_64 MSVC build, Rust 1.99.0.

The project's original code is MIT licensed. Dependencies retain their own
copyrights, licenses and source rights. Keep this document and the complete
`docs/licenses/` directory with every distributed executable, along with the
project's root `LICENSE`. Preserve these relative paths in the release ZIP.

## License texts and attributions

[DEPENDENCIES.txt](licenses/DEPENDENCIES.txt) contains the full selected license
texts, copyright statements and supplementary upstream notices. Identical texts
are shared, with reference numbers below. MIT is selected where upstream offers
it as an alternative; additional BSD/Unicode terms still apply where specified.
Supplementary notices and build-time dependencies are included conservatively;
this does not assert that all their code is linked or that alternative licenses
are cumulative.

| Package | Version | Selected license | Text references |
|---|---|---|---|
| arrayvec | 0.7.8 | MIT | 1 |
| atomic-wait | 1.1.0 | BSD-2-Clause | 2 |
| audio_thread_priority | 0.35.1 | MPL-2.0 | 3 |
| autocfg | 1.5.1 | MIT | 4 |
| bitflags | 1.3.2 | MIT | 5 |
| bytemuck | 1.25.2 | MIT | 6 |
| cfg-if | 1.0.5 | MIT | 7 |
| core_detect | 1.0.0 | MIT | 8 |
| cpal | 0.18.2 | Apache-2.0 | 9 |
| crossbeam-channel | 0.5.17 | MIT | 10, 11 |
| crossbeam-utils | 0.8.23 | MIT | 10 |
| dasp_sample | 0.11.0 | MIT | 12 |
| encoding_rs | 0.8.42 | MIT AND BSD-3-Clause | 13, 14, 15 |
| extended | 0.1.0 | MIT | 16 |
| itoa | 1.0.18 | MIT | 17 |
| lazy_static | 1.5.1 | MIT | 17 |
| log | 0.4.34 | MIT | 5 |
| memchr | 2.8.3 | MIT | 18, 19 |
| multiversion_no_op | 1.0.0 | MIT | 20, 14 |
| num-complex | 0.4.6 | MIT | 5 |
| num-integer | 0.1.47 | MIT | 5 |
| num-traits | 0.2.19 | MIT | 5 |
| primal-check | 0.3.4 | MIT | 21 |
| proc-macro2 | 1.0.107 | MIT | 17 |
| quote | 1.0.47 | MIT | 17 |
| rustfft | 6.4.1 | MIT | 22 |
| rustversion | 1.0.23 | MIT | 17 |
| scopeguard | 1.2.0 | MIT | 23 |
| serde | 1.0.229 | MIT | 17 |
| serde_core | 1.0.229 | MIT | 17 |
| serde_json | 1.0.152 | MIT | 17 |
| simdutf8 | 0.1.5 | MIT | 24 |
| strength_reduce | 0.2.4 | MIT | 22 |
| symphonia | 0.5.5 | MPL-2.0 | 3 |
| symphonia-bundle-flac | 0.5.5 | MPL-2.0 | 3 |
| symphonia-bundle-mp3 | 0.5.5 | MPL-2.0 | 3 |
| symphonia-codec-aac | 0.5.5 | MPL-2.0 | 3 |
| symphonia-codec-adpcm | 0.5.5 | MPL-2.0 | 3 |
| symphonia-codec-pcm | 0.5.5 | MPL-2.0 | 3 |
| symphonia-codec-vorbis | 0.5.5 | MPL-2.0 | 3 |
| symphonia-core | 0.5.5 | MPL-2.0 | 3 |
| symphonia-format-isomp4 | 0.5.5 | MPL-2.0 | 3 |
| symphonia-format-mkv | 0.5.5 | MPL-2.0 | 3 |
| symphonia-format-ogg | 0.5.5 | MPL-2.0 | 3 |
| symphonia-format-riff | 0.5.5 | MPL-2.0 | 3 |
| symphonia-metadata | 0.5.5 | MPL-2.0 | 3 |
| symphonia-utils-xiph | 0.5.5 | MPL-2.0 | 3 |
| syn | 2.0.119 | MIT | 17 |
| toml | 0.5.11 | MIT | 7 |
| transpose | 0.2.3 | MIT | 25 |
| unicode-ident | 1.0.26 | MIT AND Unicode-3.0 | 17, 26 |
| windows | 0.62.2 | MIT | 27 |
| windows-collections | 0.3.2 | MIT | 27 |
| windows-core | 0.62.2 | MIT | 27 |
| windows-future | 0.3.2 | MIT | 27 |
| windows-implement | 0.60.2 | MIT | 27 |
| windows-interface | 0.59.3 | MIT | 27 |
| windows-link | 0.2.1 | MIT | 27 |
| windows-numerics | 0.3.1 | MIT | 27 |
| windows-result | 0.4.1 | MIT | 27 |
| windows-strings | 0.5.1 | MIT | 27 |
| windows-sys | 0.42.0 | MIT | 27 |
| windows-sys | 0.61.2 | MIT | 27 |
| windows-threading | 0.2.1 | MIT | 27 |
| windows_x86_64_msvc | 0.42.2 | MIT | 27 |
| winres | 0.1.12 | MIT | 28 |
| zmij | 1.0.23 | MIT | 17 |

dasp_sample's published crate omits its license files. Its MIT notice was obtained
from the exact upstream revision recorded by that crate:
https://raw.githubusercontent.com/RustAudio/dasp/97c3bb9b2363c0b46ac1633858bf1054fd02a980/LICENSE-MIT.

## MPL source availability

The following unmodified dependency sources are available under MPL-2.0. The full
MPL text is included in DEPENDENCIES.txt; its terms govern these source files.
Download the exact-version `.crate` archives below and extract them as gzip tar
archives, for example `tar -xzf symphonia-core-0.5.5.crate`. These downloads contain
the preferred source form, Cargo manifests and original notices. The project's
MIT license does not limit recipients' rights to those MPL sources.

- [audio_thread_priority 0.35.1](https://static.crates.io/crates/audio_thread_priority/audio_thread_priority-0.35.1.crate)
- [symphonia 0.5.5](https://static.crates.io/crates/symphonia/symphonia-0.5.5.crate)
- [symphonia-bundle-flac 0.5.5](https://static.crates.io/crates/symphonia-bundle-flac/symphonia-bundle-flac-0.5.5.crate)
- [symphonia-bundle-mp3 0.5.5](https://static.crates.io/crates/symphonia-bundle-mp3/symphonia-bundle-mp3-0.5.5.crate)
- [symphonia-codec-aac 0.5.5](https://static.crates.io/crates/symphonia-codec-aac/symphonia-codec-aac-0.5.5.crate)
- [symphonia-codec-adpcm 0.5.5](https://static.crates.io/crates/symphonia-codec-adpcm/symphonia-codec-adpcm-0.5.5.crate)
- [symphonia-codec-pcm 0.5.5](https://static.crates.io/crates/symphonia-codec-pcm/symphonia-codec-pcm-0.5.5.crate)
- [symphonia-codec-vorbis 0.5.5](https://static.crates.io/crates/symphonia-codec-vorbis/symphonia-codec-vorbis-0.5.5.crate)
- [symphonia-core 0.5.5](https://static.crates.io/crates/symphonia-core/symphonia-core-0.5.5.crate)
- [symphonia-format-isomp4 0.5.5](https://static.crates.io/crates/symphonia-format-isomp4/symphonia-format-isomp4-0.5.5.crate)
- [symphonia-format-mkv 0.5.5](https://static.crates.io/crates/symphonia-format-mkv/symphonia-format-mkv-0.5.5.crate)
- [symphonia-format-ogg 0.5.5](https://static.crates.io/crates/symphonia-format-ogg/symphonia-format-ogg-0.5.5.crate)
- [symphonia-format-riff 0.5.5](https://static.crates.io/crates/symphonia-format-riff/symphonia-format-riff-0.5.5.crate)
- [symphonia-metadata 0.5.5](https://static.crates.io/crates/symphonia-metadata/symphonia-metadata-0.5.5.crate)
- [symphonia-utils-xiph 0.5.5](https://static.crates.io/crates/symphonia-utils-xiph/symphonia-utils-xiph-0.5.5.crate)

Keep these downloads available to recipients. Recheck the exact-version links
before a release; if an upstream download is unavailable, provide the matching
source archive through a maintained mirror. Any local MPL-source modifications
must be made available too; these dependencies are currently unmodified.

## Modified CPAL

CPAL 0.18.2 remains Apache-2.0. Its license text is included in DEPENDENCIES.txt,
and the source license is retained at
[vendor/cpal/LICENSE](https://github.com/S4M1TH4/kyouko-player/blob/main/vendor/cpal/LICENSE).
This project adds output-worker startup observation and limits the source copy
to Windows/Linux platforms. The source files carry modification notices; see
[MODIFICATIONS.md](https://github.com/S4M1TH4/kyouko-player/blob/main/vendor/cpal/MODIFICATIONS.md)
for the complete list.

## Rust standard library

The executable also contains Rust standard-library code. Its supplied
[copyright inventory](licenses/rust/COPYRIGHT-library.html) and referenced license
texts are retained intact. This inventory includes other-platform/build components
that may not be linked on Windows. Rust 1.99.0 source is available at
https://github.com/rust-lang/rust/tree/b940084d7eb6a299eb4bfeb8e34901bc051e7ac4/library.

## External programs and maintenance

yt-dlp, FFmpeg, JavaScript runtimes and Microsoft system/runtime DLLs are not
bundled in the planned player-only release. Users obtain them separately under
their respective terms. Review their redistribution requirements before bundling
them. Review these notices when dependencies, features, compiler or vendored
code change; they describe the versions above, not every possible future build.
