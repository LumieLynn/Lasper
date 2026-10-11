# Additional Third-Party Licenses and Notices

Lasper is licensed under GPL-3.0-or-later. Third-party components retain their own licenses. This document records adapted source, bundled components, and upstream notices that are not fully represented by Cargo package metadata; it supplements the complete dependency inventory in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

## Dependency inventory

[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) lists the enabled Cargo dependencies and their license texts for the x86_64 and aarch64 Linux builds, including glibc and musl targets. It includes build dependencies and excludes dependencies used only by tests or disabled features. Dual-licensed dependencies use the preferred license choices in [about.toml](about.toml).

Regenerate the inventory with `cargo about generate --locked --fail about.hbs --output-file THIRD_PARTY_NOTICES.md`, using cargo-about 0.9.2 installed with its `cli` feature. Review the generated inventory and the additional notices below whenever dependencies change. Package metadata alone does not describe every bundled component.

Additional copyright and notice files shipped by those crates are preserved in [Cargo-Additional-Notices.txt](licenses/Cargo-Additional-Notices.txt). Upstream notices for test-only material are retained as provided and do not mean that material is linked into Lasper.

Copyright and license headers retained in dependency source files are also reproduced in [Cargo-Source-Notices.txt](licenses/Cargo-Source-Notices.txt), including the authorship notices for gethostname, Ryu, Rust-derived code, and other dependencies.

## dekit (formerly mprocs) terminal data types

`src/tui/foundation/term/attrs.rs` contains portions adapted from [dekit](https://github.com/pvolok/dekit), formerly named mprocs, and `src/tui/foundation/term/color.rs` retains its ported color model and constants. Lasper maintains the surrounding terminal-panel implementation and uses rio-vt as its terminal core.

MIT License

Copyright (c) 2022 Pavel Volokitin

Permission is hereby granted, free of charge, to any person obtaining a copy of this software and associated documentation files (the "Software"), to deal in the Software without restriction, including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

## Rio terminal core

Lasper uses `rio-vt`, `rio-graphics`, `rio-unicode`, and `rio-grapheme-width` 0.5.28 from [Rio](https://github.com/raphamorim/rio). The workspace [MIT license](licenses/Rio-MIT.txt), copyright 2022-present Raphael Amorim, is preserved from upstream commit `37624389e8dde79ba25a8147a5e809c01d16d1c9`.

Parts of rio-vt's grid, selection, terminal state, and related modules originate from [Alacritty](https://github.com/alacritty/alacritty). Their [Apache-2.0 license and copyright notice](licenses/Alacritty-Apache-2.0.txt), copyright 2020 The Alacritty Project, are retained from the source commit identified by rio-vt: `e35e5ad14fce8456afdd89f2b392b9924bb27471`.

rio-unicode includes code from the Rust Project's Unicode width implementation. Its [MIT terms and Rust Project copyright notices](licenses/Rust-Unicode-MIT.txt) are retained alongside the Rio license. rio-grapheme-width contains code and data from WezTerm, copyright 2018-Present Wez Furlong and 2023-Present Raphael Amorim; its complete packaged notice appears in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

## Bundled native and translated code

The `simdutf` 0.7.0 Rust wrapper uses MIT, copyright 2022 Nugine. It bundles simdutf C++ 7.7.1, whose [MIT license](licenses/simdutf-MIT.txt), copyright 2021 The simdutf authors, and [BSD attribution for instruction-set detection](licenses/simdutf-BSD-Notices.txt) are also retained. The C++ version is identified by the bundled `SIMDUTF_VERSION` definition.

`unsafe-libyaml` is a Rust translation of libyaml. The crate's permission notice appears in the generated inventory; the upstream [libyaml MIT license](licenses/libyaml-MIT.txt) preserves copyright 2017-2020 Ingy döt Net and 2006-2016 Kirill Simonov.

## MPL-covered dependency

`option-ext` 0.2.0 is covered by MPL-2.0. Its unmodified source remains available in the [published crate archive](https://static.crates.io/crates/option-ext/option-ext-0.2.0.crate) and [upstream repository](https://github.com/soc/option-ext). Its full license is included in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

## Binary distribution

Release downloads include a license archive with Lasper's LICENSE, this file, THIRD_PARTY_NOTICES.md, and the additional notices in `licenses/`. Each build also supplies notices for its Rust standard library and, for Zig-built musl binaries, the bundled musl and LLVM C++ runtimes. These build-specific notices come from the toolchain used to produce that binary.

The matching source is available from the release tag in [Lasper's repository](https://github.com/LumieLynn/Lasper), with dependency versions and source checksums recorded in Cargo.lock. Redistributors must preserve the applicable license files and provide the corresponding source under their terms.
