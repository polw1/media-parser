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

## Applications distributing binaries

The IJG license distinguishes source distribution from binary distribution, and
requires the binary case to carry the notice in the documentation or other
materials provided with the distribution. A file in this repository travels
with this plugin's source, not with an application's installer.

An application that bundles this plugin therefore carries this obligation
itself and needs to:

   * Generate an attribution set over its whole dependency tree, for example
     with `cargo about` or `cargo-bundle-licenses` plus the equivalent for its
     npm dependencies. `cargo about` needs `IJG` declared explicitly, since it
     is not on the usual allowlists.
   * Include the IJG statement above in that set.
   * Deliver the result with the application — a licenses screen, a bundled
     resource in `tauri.conf.json`, or both.
