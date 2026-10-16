# Third-party notices

For kyouko-player 0.1.0, Windows x86_64 MSVC, default features.

The root MIT license applies to the project's original code. Dependencies retain
their own licenses, copyrights, disclaimers and source rights. This inventory
conservatively includes build-time and procedural-macro dependencies as well as
runtime dependencies; it does not assert that every included file is linked.

## Source availability

The MPL-2.0-covered Symphonia components and audio_thread_priority are distributed
in Source Code Form under MPL-2.0 in the exact publisher `.crate` archives in
[licenses/sources/](licenses/sources/). These are gzip-compressed tar archives;
extract with `tar -xzf NAME-VERSION.crate`. Copyright notices and source headers
are retained inside each archive. [MPL-2.0](licenses/MPL-2.0.txt) governs those
sources. The package list below also supplies exact-version download URLs.
These source rights are not restricted by the root MIT license.

The modified CPAL source is in [vendor/cpal/](vendor/cpal/), under Apache-2.0.
Its [modification notice](licenses/cpal-MODIFICATIONS.md) identifies local changes.

## Rust standard library

Built with Rust 1.99.0 (b940084d7eb6a299eb4bfeb8e34901bc051e7ac4). The Rust
standard library is primarily MIT OR Apache-2.0, with component-specific terms.
Its complete supplied [library copyright inventory](licenses/rust/COPYRIGHT-library.html)
and referenced license texts are retained, including notices for components
that may not be linked on this target. Matching source is available at
https://github.com/rust-lang/rust/tree/b940084d7eb6a299eb4bfeb8e34901bc051e7ac4/library.

## Dependency inventory

Where upstream offers alternatives, this distribution selects MIT when available,
plus any conjunctive terms and supplementary notices. Other supplied alternative
license texts are retained for provenance; their presence does not make alternatives
cumulative or relicense the application.

### arrayvec 0.7.8

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [LICENSE-APACHE](licenses/crates/arrayvec-0.7.8/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/arrayvec-0.7.8/LICENSE-MIT).
Exact-version source: [arrayvec 0.7.8](https://static.crates.io/crates/arrayvec/arrayvec-0.7.8.crate).

### atomic-wait 1.1.0

Declared license: `BSD-2-Clause`. Selected: `BSD-2-Clause`.
License and notice files: [LICENSE](licenses/crates/atomic-wait-1.1.0/LICENSE).
Exact-version source: [atomic-wait 1.1.0](https://static.crates.io/crates/atomic-wait/atomic-wait-1.1.0.crate).

### audio_thread_priority 0.35.1

Declared license: `MPL-2.0`. Selected: `MPL-2.0`.
License and notice files: [LICENSE](licenses/crates/audio_thread_priority-0.35.1/LICENSE), [MPL-2.0.txt](licenses/MPL-2.0.txt).
Exact-version source: [audio_thread_priority 0.35.1](https://static.crates.io/crates/audio_thread_priority/audio_thread_priority-0.35.1.crate).
Included MPL source: [audio_thread_priority-0.35.1.crate](licenses/sources/audio_thread_priority-0.35.1.crate).

### autocfg 1.5.1

Declared license: `Apache-2.0 OR MIT`. Selected: `MIT`.
License and notice files: [LICENSE-APACHE](licenses/crates/autocfg-1.5.1/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/autocfg-1.5.1/LICENSE-MIT).
Exact-version source: [autocfg 1.5.1](https://static.crates.io/crates/autocfg/autocfg-1.5.1.crate).

### bitflags 1.3.2

Declared license: `MIT/Apache-2.0`. Selected: `MIT`.
License and notice files: [LICENSE-APACHE](licenses/crates/bitflags-1.3.2/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/bitflags-1.3.2/LICENSE-MIT).
Exact-version source: [bitflags 1.3.2](https://static.crates.io/crates/bitflags/bitflags-1.3.2.crate).

### bytemuck 1.25.2

Declared license: `Zlib OR Apache-2.0 OR MIT`. Selected: `MIT`.
License and notice files: [LICENSE-APACHE](licenses/crates/bytemuck-1.25.2/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/bytemuck-1.25.2/LICENSE-MIT), [LICENSE-ZLIB](licenses/crates/bytemuck-1.25.2/LICENSE-ZLIB).
Exact-version source: [bytemuck 1.25.2](https://static.crates.io/crates/bytemuck/bytemuck-1.25.2.crate).

### cfg-if 1.0.5

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [LICENSE-APACHE](licenses/crates/cfg-if-1.0.5/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/cfg-if-1.0.5/LICENSE-MIT).
Exact-version source: [cfg-if 1.0.5](https://static.crates.io/crates/cfg-if/cfg-if-1.0.5.crate).

### core_detect 1.0.0

Declared license: `MIT/Apache-2.0`. Selected: `MIT`.
License and notice files: [LICENSE-APACHE](licenses/crates/core_detect-1.0.0/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/core_detect-1.0.0/LICENSE-MIT).
Exact-version source: [core_detect 1.0.0](https://static.crates.io/crates/core_detect/core_detect-1.0.0.crate).

### cpal 0.18.2

Declared license: `Apache-2.0`. Selected: `Apache-2.0`.
License and notice files: [LICENSE](licenses/crates/cpal-0.18.2/LICENSE).

### crossbeam-channel 0.5.17

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [LICENSE-APACHE](licenses/crates/crossbeam-channel-0.5.17/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/crossbeam-channel-0.5.17/LICENSE-MIT), [LICENSE-THIRD-PARTY](licenses/crates/crossbeam-channel-0.5.17/LICENSE-THIRD-PARTY).
Exact-version source: [crossbeam-channel 0.5.17](https://static.crates.io/crates/crossbeam-channel/crossbeam-channel-0.5.17.crate).

### crossbeam-utils 0.8.23

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [LICENSE-APACHE](licenses/crates/crossbeam-utils-0.8.23/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/crossbeam-utils-0.8.23/LICENSE-MIT).
Exact-version source: [crossbeam-utils 0.8.23](https://static.crates.io/crates/crossbeam-utils/crossbeam-utils-0.8.23.crate).

### dasp_sample 0.11.0

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [LICENSE-MIT](licenses/crates/dasp_sample-0.11.0/LICENSE-MIT).
Exact-version source: [dasp_sample 0.11.0](https://static.crates.io/crates/dasp_sample/dasp_sample-0.11.0.crate).
License provenance: https://raw.githubusercontent.com/RustAudio/dasp/97c3bb9b2363c0b46ac1633858bf1054fd02a980/LICENSE-MIT (published crate omits root license files; retrieved from its exact VCS revision).

### encoding_rs 0.8.42

Declared license: `(Apache-2.0 OR MIT) AND BSD-3-Clause`. Selected: `MIT AND BSD-3-Clause`.
License and notice files: [COPYRIGHT](licenses/crates/encoding_rs-0.8.42/COPYRIGHT), [LICENSE-APACHE](licenses/crates/encoding_rs-0.8.42/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/encoding_rs-0.8.42/LICENSE-MIT), [LICENSE-WHATWG](licenses/crates/encoding_rs-0.8.42/LICENSE-WHATWG).
Exact-version source: [encoding_rs 0.8.42](https://static.crates.io/crates/encoding_rs/encoding_rs-0.8.42.crate).

### extended 0.1.0

Declared license: `MIT`. Selected: `MIT`.
License and notice files: [LICENSE.txt](licenses/crates/extended-0.1.0/LICENSE.txt).
Exact-version source: [extended 0.1.0](https://static.crates.io/crates/extended/extended-0.1.0.crate).

### itoa 1.0.18

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [LICENSE-APACHE](licenses/crates/itoa-1.0.18/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/itoa-1.0.18/LICENSE-MIT).
Exact-version source: [itoa 1.0.18](https://static.crates.io/crates/itoa/itoa-1.0.18.crate).

### lazy_static 1.5.1

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [LICENSE-APACHE](licenses/crates/lazy_static-1.5.1/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/lazy_static-1.5.1/LICENSE-MIT).
Exact-version source: [lazy_static 1.5.1](https://static.crates.io/crates/lazy_static/lazy_static-1.5.1.crate).

### log 0.4.34

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [LICENSE-APACHE](licenses/crates/log-0.4.34/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/log-0.4.34/LICENSE-MIT).
Exact-version source: [log 0.4.34](https://static.crates.io/crates/log/log-0.4.34.crate).

### memchr 2.8.3

Declared license: `Unlicense OR MIT`. Selected: `MIT`.
License and notice files: [COPYING](licenses/crates/memchr-2.8.3/COPYING), [LICENSE-MIT](licenses/crates/memchr-2.8.3/LICENSE-MIT), [UNLICENSE](licenses/crates/memchr-2.8.3/UNLICENSE).
Exact-version source: [memchr 2.8.3](https://static.crates.io/crates/memchr/memchr-2.8.3.crate).

### multiversion_no_op 1.0.0

Declared license: `Apache-2.0 OR MIT`. Selected: `MIT`.
License and notice files: [COPYRIGHT](licenses/crates/multiversion_no_op-1.0.0/COPYRIGHT), [LICENSE-APACHE](licenses/crates/multiversion_no_op-1.0.0/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/multiversion_no_op-1.0.0/LICENSE-MIT).
Exact-version source: [multiversion_no_op 1.0.0](https://static.crates.io/crates/multiversion_no_op/multiversion_no_op-1.0.0.crate).

### num-complex 0.4.6

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [LICENSE-APACHE](licenses/crates/num-complex-0.4.6/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/num-complex-0.4.6/LICENSE-MIT).
Exact-version source: [num-complex 0.4.6](https://static.crates.io/crates/num-complex/num-complex-0.4.6.crate).

### num-integer 0.1.47

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [LICENSE-APACHE](licenses/crates/num-integer-0.1.47/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/num-integer-0.1.47/LICENSE-MIT).
Exact-version source: [num-integer 0.1.47](https://static.crates.io/crates/num-integer/num-integer-0.1.47.crate).

### num-traits 0.2.19

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [LICENSE-APACHE](licenses/crates/num-traits-0.2.19/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/num-traits-0.2.19/LICENSE-MIT).
Exact-version source: [num-traits 0.2.19](https://static.crates.io/crates/num-traits/num-traits-0.2.19.crate).

### primal-check 0.3.4

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [LICENSE-APACHE](licenses/crates/primal-check-0.3.4/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/primal-check-0.3.4/LICENSE-MIT).
Exact-version source: [primal-check 0.3.4](https://static.crates.io/crates/primal-check/primal-check-0.3.4.crate).

### proc-macro2 1.0.107

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [LICENSE-APACHE](licenses/crates/proc-macro2-1.0.107/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/proc-macro2-1.0.107/LICENSE-MIT).
Exact-version source: [proc-macro2 1.0.107](https://static.crates.io/crates/proc-macro2/proc-macro2-1.0.107.crate).

### quote 1.0.47

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [LICENSE-APACHE](licenses/crates/quote-1.0.47/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/quote-1.0.47/LICENSE-MIT).
Exact-version source: [quote 1.0.47](https://static.crates.io/crates/quote/quote-1.0.47.crate).

### rustfft 6.4.1

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [LICENSE-APACHE](licenses/crates/rustfft-6.4.1/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/rustfft-6.4.1/LICENSE-MIT).
Exact-version source: [rustfft 6.4.1](https://static.crates.io/crates/rustfft/rustfft-6.4.1.crate).

### rustversion 1.0.23

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [LICENSE-APACHE](licenses/crates/rustversion-1.0.23/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/rustversion-1.0.23/LICENSE-MIT).
Exact-version source: [rustversion 1.0.23](https://static.crates.io/crates/rustversion/rustversion-1.0.23.crate).

### scopeguard 1.2.0

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [LICENSE-APACHE](licenses/crates/scopeguard-1.2.0/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/scopeguard-1.2.0/LICENSE-MIT).
Exact-version source: [scopeguard 1.2.0](https://static.crates.io/crates/scopeguard/scopeguard-1.2.0.crate).

### serde 1.0.229

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [LICENSE-APACHE](licenses/crates/serde-1.0.229/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/serde-1.0.229/LICENSE-MIT).
Exact-version source: [serde 1.0.229](https://static.crates.io/crates/serde/serde-1.0.229.crate).

### serde_core 1.0.229

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [LICENSE-APACHE](licenses/crates/serde_core-1.0.229/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/serde_core-1.0.229/LICENSE-MIT).
Exact-version source: [serde_core 1.0.229](https://static.crates.io/crates/serde_core/serde_core-1.0.229.crate).

### serde_json 1.0.152

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [LICENSE-APACHE](licenses/crates/serde_json-1.0.152/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/serde_json-1.0.152/LICENSE-MIT).
Exact-version source: [serde_json 1.0.152](https://static.crates.io/crates/serde_json/serde_json-1.0.152.crate).

### simdutf8 0.1.5

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [LICENSE-Apache](licenses/crates/simdutf8-0.1.5/LICENSE-Apache), [LICENSE-MIT](licenses/crates/simdutf8-0.1.5/LICENSE-MIT).
Exact-version source: [simdutf8 0.1.5](https://static.crates.io/crates/simdutf8/simdutf8-0.1.5.crate).

### strength_reduce 0.2.4

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [LICENSE-APACHE](licenses/crates/strength_reduce-0.2.4/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/strength_reduce-0.2.4/LICENSE-MIT).
Exact-version source: [strength_reduce 0.2.4](https://static.crates.io/crates/strength_reduce/strength_reduce-0.2.4.crate).

### symphonia 0.5.5

Declared license: `MPL-2.0`. Selected: `MPL-2.0`.
License and notice files: [MPL-2.0.txt](licenses/MPL-2.0.txt).
Exact-version source: [symphonia 0.5.5](https://static.crates.io/crates/symphonia/symphonia-0.5.5.crate).
Included MPL source: [symphonia-0.5.5.crate](licenses/sources/symphonia-0.5.5.crate).

### symphonia-bundle-flac 0.5.5

Declared license: `MPL-2.0`. Selected: `MPL-2.0`.
License and notice files: [MPL-2.0.txt](licenses/MPL-2.0.txt).
Exact-version source: [symphonia-bundle-flac 0.5.5](https://static.crates.io/crates/symphonia-bundle-flac/symphonia-bundle-flac-0.5.5.crate).
Included MPL source: [symphonia-bundle-flac-0.5.5.crate](licenses/sources/symphonia-bundle-flac-0.5.5.crate).

### symphonia-bundle-mp3 0.5.5

Declared license: `MPL-2.0`. Selected: `MPL-2.0`.
License and notice files: [MPL-2.0.txt](licenses/MPL-2.0.txt).
Exact-version source: [symphonia-bundle-mp3 0.5.5](https://static.crates.io/crates/symphonia-bundle-mp3/symphonia-bundle-mp3-0.5.5.crate).
Included MPL source: [symphonia-bundle-mp3-0.5.5.crate](licenses/sources/symphonia-bundle-mp3-0.5.5.crate).

### symphonia-codec-aac 0.5.5

Declared license: `MPL-2.0`. Selected: `MPL-2.0`.
License and notice files: [MPL-2.0.txt](licenses/MPL-2.0.txt).
Exact-version source: [symphonia-codec-aac 0.5.5](https://static.crates.io/crates/symphonia-codec-aac/symphonia-codec-aac-0.5.5.crate).
Included MPL source: [symphonia-codec-aac-0.5.5.crate](licenses/sources/symphonia-codec-aac-0.5.5.crate).

### symphonia-codec-adpcm 0.5.5

Declared license: `MPL-2.0`. Selected: `MPL-2.0`.
License and notice files: [MPL-2.0.txt](licenses/MPL-2.0.txt).
Exact-version source: [symphonia-codec-adpcm 0.5.5](https://static.crates.io/crates/symphonia-codec-adpcm/symphonia-codec-adpcm-0.5.5.crate).
Included MPL source: [symphonia-codec-adpcm-0.5.5.crate](licenses/sources/symphonia-codec-adpcm-0.5.5.crate).

### symphonia-codec-pcm 0.5.5

Declared license: `MPL-2.0`. Selected: `MPL-2.0`.
License and notice files: [MPL-2.0.txt](licenses/MPL-2.0.txt).
Exact-version source: [symphonia-codec-pcm 0.5.5](https://static.crates.io/crates/symphonia-codec-pcm/symphonia-codec-pcm-0.5.5.crate).
Included MPL source: [symphonia-codec-pcm-0.5.5.crate](licenses/sources/symphonia-codec-pcm-0.5.5.crate).

### symphonia-codec-vorbis 0.5.5

Declared license: `MPL-2.0`. Selected: `MPL-2.0`.
License and notice files: [MPL-2.0.txt](licenses/MPL-2.0.txt).
Exact-version source: [symphonia-codec-vorbis 0.5.5](https://static.crates.io/crates/symphonia-codec-vorbis/symphonia-codec-vorbis-0.5.5.crate).
Included MPL source: [symphonia-codec-vorbis-0.5.5.crate](licenses/sources/symphonia-codec-vorbis-0.5.5.crate).

### symphonia-core 0.5.5

Declared license: `MPL-2.0`. Selected: `MPL-2.0`.
License and notice files: [MPL-2.0.txt](licenses/MPL-2.0.txt).
Exact-version source: [symphonia-core 0.5.5](https://static.crates.io/crates/symphonia-core/symphonia-core-0.5.5.crate).
Included MPL source: [symphonia-core-0.5.5.crate](licenses/sources/symphonia-core-0.5.5.crate).

### symphonia-format-isomp4 0.5.5

Declared license: `MPL-2.0`. Selected: `MPL-2.0`.
License and notice files: [MPL-2.0.txt](licenses/MPL-2.0.txt).
Exact-version source: [symphonia-format-isomp4 0.5.5](https://static.crates.io/crates/symphonia-format-isomp4/symphonia-format-isomp4-0.5.5.crate).
Included MPL source: [symphonia-format-isomp4-0.5.5.crate](licenses/sources/symphonia-format-isomp4-0.5.5.crate).

### symphonia-format-mkv 0.5.5

Declared license: `MPL-2.0`. Selected: `MPL-2.0`.
License and notice files: [MPL-2.0.txt](licenses/MPL-2.0.txt).
Exact-version source: [symphonia-format-mkv 0.5.5](https://static.crates.io/crates/symphonia-format-mkv/symphonia-format-mkv-0.5.5.crate).
Included MPL source: [symphonia-format-mkv-0.5.5.crate](licenses/sources/symphonia-format-mkv-0.5.5.crate).

### symphonia-format-ogg 0.5.5

Declared license: `MPL-2.0`. Selected: `MPL-2.0`.
License and notice files: [MPL-2.0.txt](licenses/MPL-2.0.txt).
Exact-version source: [symphonia-format-ogg 0.5.5](https://static.crates.io/crates/symphonia-format-ogg/symphonia-format-ogg-0.5.5.crate).
Included MPL source: [symphonia-format-ogg-0.5.5.crate](licenses/sources/symphonia-format-ogg-0.5.5.crate).

### symphonia-format-riff 0.5.5

Declared license: `MPL-2.0`. Selected: `MPL-2.0`.
License and notice files: [MPL-2.0.txt](licenses/MPL-2.0.txt).
Exact-version source: [symphonia-format-riff 0.5.5](https://static.crates.io/crates/symphonia-format-riff/symphonia-format-riff-0.5.5.crate).
Included MPL source: [symphonia-format-riff-0.5.5.crate](licenses/sources/symphonia-format-riff-0.5.5.crate).

### symphonia-metadata 0.5.5

Declared license: `MPL-2.0`. Selected: `MPL-2.0`.
License and notice files: [MPL-2.0.txt](licenses/MPL-2.0.txt).
Exact-version source: [symphonia-metadata 0.5.5](https://static.crates.io/crates/symphonia-metadata/symphonia-metadata-0.5.5.crate).
Included MPL source: [symphonia-metadata-0.5.5.crate](licenses/sources/symphonia-metadata-0.5.5.crate).

### symphonia-utils-xiph 0.5.5

Declared license: `MPL-2.0`. Selected: `MPL-2.0`.
License and notice files: [MPL-2.0.txt](licenses/MPL-2.0.txt).
Exact-version source: [symphonia-utils-xiph 0.5.5](https://static.crates.io/crates/symphonia-utils-xiph/symphonia-utils-xiph-0.5.5.crate).
Included MPL source: [symphonia-utils-xiph-0.5.5.crate](licenses/sources/symphonia-utils-xiph-0.5.5.crate).

### syn 2.0.119

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [LICENSE-APACHE](licenses/crates/syn-2.0.119/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/syn-2.0.119/LICENSE-MIT).
Exact-version source: [syn 2.0.119](https://static.crates.io/crates/syn/syn-2.0.119.crate).

### toml 0.5.11

Declared license: `MIT/Apache-2.0`. Selected: `MIT`.
License and notice files: [LICENSE-APACHE](licenses/crates/toml-0.5.11/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/toml-0.5.11/LICENSE-MIT).
Exact-version source: [toml 0.5.11](https://static.crates.io/crates/toml/toml-0.5.11.crate).

### transpose 0.2.3

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [LICENSE-APACHE](licenses/crates/transpose-0.2.3/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/transpose-0.2.3/LICENSE-MIT).
Exact-version source: [transpose 0.2.3](https://static.crates.io/crates/transpose/transpose-0.2.3.crate).

### unicode-ident 1.0.26

Declared license: `(MIT OR Apache-2.0) AND Unicode-3.0`. Selected: `MIT AND Unicode-3.0`.
License and notice files: [LICENSE-APACHE](licenses/crates/unicode-ident-1.0.26/LICENSE-APACHE), [LICENSE-MIT](licenses/crates/unicode-ident-1.0.26/LICENSE-MIT), [LICENSE-UNICODE](licenses/crates/unicode-ident-1.0.26/LICENSE-UNICODE).
Exact-version source: [unicode-ident 1.0.26](https://static.crates.io/crates/unicode-ident/unicode-ident-1.0.26.crate).

### windows 0.62.2

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [license-apache-2.0](licenses/crates/windows-0.62.2/license-apache-2.0), [license-mit](licenses/crates/windows-0.62.2/license-mit).
Exact-version source: [windows 0.62.2](https://static.crates.io/crates/windows/windows-0.62.2.crate).

### windows-collections 0.3.2

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [license-apache-2.0](licenses/crates/windows-collections-0.3.2/license-apache-2.0), [license-mit](licenses/crates/windows-collections-0.3.2/license-mit).
Exact-version source: [windows-collections 0.3.2](https://static.crates.io/crates/windows-collections/windows-collections-0.3.2.crate).

### windows-core 0.62.2

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [license-apache-2.0](licenses/crates/windows-core-0.62.2/license-apache-2.0), [license-mit](licenses/crates/windows-core-0.62.2/license-mit).
Exact-version source: [windows-core 0.62.2](https://static.crates.io/crates/windows-core/windows-core-0.62.2.crate).

### windows-future 0.3.2

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [license-apache-2.0](licenses/crates/windows-future-0.3.2/license-apache-2.0), [license-mit](licenses/crates/windows-future-0.3.2/license-mit).
Exact-version source: [windows-future 0.3.2](https://static.crates.io/crates/windows-future/windows-future-0.3.2.crate).

### windows-implement 0.60.2

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [license-apache-2.0](licenses/crates/windows-implement-0.60.2/license-apache-2.0), [license-mit](licenses/crates/windows-implement-0.60.2/license-mit).
Exact-version source: [windows-implement 0.60.2](https://static.crates.io/crates/windows-implement/windows-implement-0.60.2.crate).

### windows-interface 0.59.3

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [license-apache-2.0](licenses/crates/windows-interface-0.59.3/license-apache-2.0), [license-mit](licenses/crates/windows-interface-0.59.3/license-mit).
Exact-version source: [windows-interface 0.59.3](https://static.crates.io/crates/windows-interface/windows-interface-0.59.3.crate).

### windows-link 0.2.1

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [license-apache-2.0](licenses/crates/windows-link-0.2.1/license-apache-2.0), [license-mit](licenses/crates/windows-link-0.2.1/license-mit).
Exact-version source: [windows-link 0.2.1](https://static.crates.io/crates/windows-link/windows-link-0.2.1.crate).

### windows-numerics 0.3.1

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [license-apache-2.0](licenses/crates/windows-numerics-0.3.1/license-apache-2.0), [license-mit](licenses/crates/windows-numerics-0.3.1/license-mit).
Exact-version source: [windows-numerics 0.3.1](https://static.crates.io/crates/windows-numerics/windows-numerics-0.3.1.crate).

### windows-result 0.4.1

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [license-apache-2.0](licenses/crates/windows-result-0.4.1/license-apache-2.0), [license-mit](licenses/crates/windows-result-0.4.1/license-mit).
Exact-version source: [windows-result 0.4.1](https://static.crates.io/crates/windows-result/windows-result-0.4.1.crate).

### windows-strings 0.5.1

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [license-apache-2.0](licenses/crates/windows-strings-0.5.1/license-apache-2.0), [license-mit](licenses/crates/windows-strings-0.5.1/license-mit).
Exact-version source: [windows-strings 0.5.1](https://static.crates.io/crates/windows-strings/windows-strings-0.5.1.crate).

### windows-sys 0.42.0

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [license-apache-2.0](licenses/crates/windows-sys-0.42.0/license-apache-2.0), [license-mit](licenses/crates/windows-sys-0.42.0/license-mit).
Exact-version source: [windows-sys 0.42.0](https://static.crates.io/crates/windows-sys/windows-sys-0.42.0.crate).

### windows-sys 0.61.2

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [license-apache-2.0](licenses/crates/windows-sys-0.61.2/license-apache-2.0), [license-mit](licenses/crates/windows-sys-0.61.2/license-mit).
Exact-version source: [windows-sys 0.61.2](https://static.crates.io/crates/windows-sys/windows-sys-0.61.2.crate).

### windows-threading 0.2.1

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [license-apache-2.0](licenses/crates/windows-threading-0.2.1/license-apache-2.0), [license-mit](licenses/crates/windows-threading-0.2.1/license-mit).
Exact-version source: [windows-threading 0.2.1](https://static.crates.io/crates/windows-threading/windows-threading-0.2.1.crate).

### windows_x86_64_msvc 0.42.2

Declared license: `MIT OR Apache-2.0`. Selected: `MIT`.
License and notice files: [license-apache-2.0](licenses/crates/windows_x86_64_msvc-0.42.2/license-apache-2.0), [license-mit](licenses/crates/windows_x86_64_msvc-0.42.2/license-mit).
Exact-version source: [windows_x86_64_msvc 0.42.2](https://static.crates.io/crates/windows_x86_64_msvc/windows_x86_64_msvc-0.42.2.crate).

### winres 0.1.12

Declared license: `MIT`. Selected: `MIT`.
License and notice files: [LICENSE](licenses/crates/winres-0.1.12/LICENSE).
Exact-version source: [winres 0.1.12](https://static.crates.io/crates/winres/winres-0.1.12.crate).

### zmij 1.0.23

Declared license: `MIT`. Selected: `MIT`.
License and notice files: [LICENSE-MIT](licenses/crates/zmij-1.0.23/LICENSE-MIT).
Exact-version source: [zmij 1.0.23](https://static.crates.io/crates/zmij/zmij-1.0.23.crate).

## External programs and platform libraries

yt-dlp, FFmpeg, JavaScript runtimes and Microsoft system/runtime DLLs are not
included in these legal materials or supplied by the planned player-only ZIP.
Users obtain external programs/runtimes separately under their own terms.
Bundling any of them requires a separate inventory and redistribution review.
Windows API imports do not mean this project distributes Windows DLLs.

## Distribution

Keep LICENSE, THIRD_PARTY_NOTICES.md and the complete licenses/ directory
(including MPL source archives) alongside every distributed executable.
Use scripts/check-third-party.ps1 to verify the inventory and optionally stage
the required files for packaging. A standalone executable omits these materials.
