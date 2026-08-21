# Third-party notices

This plugin is MIT licensed, but it links third-party code whose license
requires notices beyond the usual MIT and Apache-2.0 boilerplate. This file
records those notices and the exact text the license asks for.

It is not a complete attribution set. Most of the dependency tree is MIT or
Apache-2.0, and those licenses carry their own notice requirements. An
application that ships a compiled binary must generate a full attribution set
over its whole dependency tree — see "Applications distributing binaries"
below.

## jpeg-encoder

   * Version: 0.7.1
   * Declared license: `(MIT OR Apache-2.0) AND IJG`
   * Repository: <https://github.com/vstroebel/jpeg-encoder>

Choosing MIT or Apache-2.0 does not remove the IJG terms. The crate contains a
Rust translation of the forward DCT from the Independent JPEG Group's libjpeg
(`src/fdct.rs`), and ships the full IJG terms as `LICENSE-IJG`.

The IJG license requires this statement when only executable code is
distributed:

```text
this software is based in part on the work of the Independent JPEG Group
```

When source code is distributed, the IJG license instead requires its own
README to be included unaltered, and any changes to the original files to be
indicated in accompanying documentation. The full text is `LICENSE-IJG` in the
`jpeg-encoder` crate.

Note that the `jpeg-encoder` README's License section mentions only MIT and
Apache-2.0. The crate's package metadata and its bundled `LICENSE-IJG` are the
accurate record.

## openh264

   * Version: 0.9.7, via `openh264-sys2` 0.9.7
   * Declared license: `BSD-2-Clause`
   * Repository: <https://github.com/ralfbiedert/openh264-rs>
   * Vendored upstream: <https://github.com/cisco/openh264> at commit
     `a8e04adb69c79757da014007d4694684a64c7b74`

`openh264-sys2` vendors Cisco's OpenH264 C++ under `upstream/`, and its
`source` feature — enabled by default, and this crate depends on `openh264`
with default features — compiles that C++ and links it statically. Every
binary shipping this plugin therefore contains Cisco's code, and BSD-2-Clause
requires binary redistribution to reproduce the notice below "in the
documentation and/or other materials provided with the distribution":

```text
Copyright (c) 2013, Cisco Systems
All rights reserved.

Redistribution and use in source and binary forms, with or without modification,
are permitted provided that the following conditions are met:

* Redistributions of source code must retain the above copyright notice, this
  list of conditions and the following disclaimer.

* Redistributions in binary form must reproduce the above copyright notice, this
  list of conditions and the following disclaimer in the documentation and/or
  other materials provided with the distribution.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR
ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES
(INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES;
LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON
ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

Unlike IJG, BSD-2-Clause is recognised by the usual attribution tooling, so a
generated attribution set picks this up without extra configuration.

### AVC patents

This is the part no licence scanner will report. BSD-2-Clause covers copyright
only — the vendored `upstream/LICENSE` grants no patent rights, and H.264 is
patent-encumbered. Cisco's arrangement of paying the AVC pool royalties applies
to the prebuilt OpenH264 binary modules Cisco itself distributes from
<https://www.openh264.org/>, under a separate binary licence that is not part
of this dependency tree.

Building from source, as the default `source` feature does, produces a binary
Cisco did not distribute, so that arrangement does not reach it and the AVC
patent position stays with whoever ships the resulting application. An
integrator who wants Cisco's coverage has to consume the prebuilt module
instead — `openh264-sys2` exposes a `libloading` feature for loading it at
runtime — or evaluate the patent position independently.

## Applications distributing binaries

Both licenses above distinguish source distribution from binary distribution,
and both require the binary case to carry the notice in the documentation or
other materials provided with the distribution. A file in this repository
travels with this plugin's source, not with an application's installer.

An application that bundles this plugin therefore carries these obligations
itself and needs to:

   * Generate an attribution set over its whole dependency tree, for example
     with `cargo about` or `cargo-bundle-licenses` plus the equivalent for its
     npm dependencies. `cargo about` needs `IJG` declared explicitly, since it
     is not on the usual allowlists; `BSD-2-Clause` is already on them, so the
     Cisco notice comes through on its own.
   * Include the IJG statement above in that set.
   * Deliver the result with the application — a licenses screen, a bundled
     resource in `tauri.conf.json`, or both.
   * Decide how to handle the AVC patent position described above, which is a
     separate question from attribution and which no tool will raise.
