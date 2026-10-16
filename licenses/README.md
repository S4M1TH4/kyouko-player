# Third-party licensing materials

This directory accompanies kyouko-player's default Windows x86_64 MSVC build.
It includes runtime, build-time and procedural-macro dependencies conservatively.
It is not an assertion that every upstream file is linked into the executable.

- `crates/`: upstream license, copyright and supplementary notice files.
- `sources/`: complete, unmodified publisher archives for every MPL-2.0 package
  in the reviewed dependency graph. The SHA-256 values match `Cargo.lock`.
- `MPL-2.0.txt`: full MPL license, supplied with audio_thread_priority 0.35.1.
- `rust/`: Rust 1.99.0's supplied standard-library copyright inventory and all
  referenced license texts. Some entries apply only to other platforms or tools.
- `dependencies.json`: versions, features, source origins, selected license
  alternatives, original archive hashes and distributed-file hashes.
- `cpal-MODIFICATIONS.md`: modification notice for the vendored Apache-2.0 code.

The Symphonia crates omit a standalone license file, but retain MPL notices in
their sources; the full MPL text and exact source archives are provided here.
The published dasp_sample archive omits its standalone license files. Its MIT
text was recovered from the exact upstream Git revision recorded in the crate;
the source URL and hash are recorded in the inventory. No copyright attribution
was invented to fill those omissions.

Run `pwsh -NoProfile -File scripts/check-third-party.ps1` with PowerShell 7.2 or
newer after `cargo fetch --locked`. It rejects dependency, feature, compiler,
notice, MPL archive or vendored-source drift. Source/notice bytes are preserved
by `.gitattributes`; vendored text hashes normalize CRLF to LF for Git checkouts.

To stage the files required alongside a binary into a new ignored directory:

```powershell
pwsh -NoProfile -File scripts/check-third-party.ps1 -StageDirectory target/release-legal
```

Copy **all** staged files into the eventual release ZIP, preserving paths. Keep
the complete ZIP available to recipients; distributing only the executable
omits these materials. This command stages legal files, not a binary or a release.

When Cargo.lock, features, vendored code, compiler or version changes, re-audit
the affected packages rather than merely updating hashes. Verify downloaded
publisher archives against Cargo.lock, inspect all license/NOTICE/COPYRIGHT
files and embedded licensing exceptions, and preserve exact MPL source forms.
Refresh the notices and this inventory together. The runtime/build dependency
graph is obtained with `cargo metadata --locked --filter-platform
x86_64-pc-windows-msvc`; application-root reachability selects the packages.

These materials address the reviewed dependencies' copyright-license notices
and source availability. They do not establish rights to third-party media,
trademarks, external program redistribution or all codec patents in every
jurisdiction. Review a change of distribution scope before shipping it.
