# Third-Party Notices

Launchpad Studio is licensed under the Apache License 2.0 (see `LICENSE`).
It depends on third-party software listed below. This file exists to satisfy
the attribution requirements of those licences and to let you audit our
dependency licensing without building the project.

**Coverage: 775 of 775 Rust crates and 565 of 565 npm packages. No unknowns.**

## How this was produced

Nothing below this section was transcribed by hand.
`dev/generate-third-party-notices.mjs` wrote all of it, from two committed
files, and you can run it yourself:

```bash
dev/generate-third-party-notices.mjs --check   # is this file still accurate?
dev/generate-third-party-notices.mjs           # make it accurate
```

- **Rust** — `cargo metadata --locked`, every package carrying a `source`, with
  the `license` string read from that crate's own manifest. `cargo metadata`
  resolves the graph for every target platform rather than for the host, so
  Linux- and Windows-gated crates (`wayland-*`, `zbus*`, `secret-service`,
  `linux-keyutils`) are covered even though they never build on macOS.
- **npm** — `frontend/package-lock.json`, every entry under `node_modules/`,
  with the `license` field the lockfile records. The lockfile rather than an
  installed `node_modules/` tree, for the same reason: npm materializes only the
  optional platform binaries matching the host, so reading an installed tree on
  macOS misses eighty packages a Linux contributor has on disk, eleven of them
  under MPL-2.0.

Both lists are **transitive**, not just direct dependencies. Both are ordered by
Unicode code unit rather than by locale, so the output does not depend on the
machine that produced it.

The script also checks the two claims here that are most likely to stop being
true without anyone noticing: that no dependency is under one of the licences
excluded below, and that every dependency which is not plainly permissive is
named in "Obligations we carry". Either one failing fails the script.

### Known limitations of this file

- It reflects the dependency tree at the pinned versions in `Cargo.lock` and
  `package-lock.json`. Regenerate it when those change.
- It records the licence each package **declares**. It does not audit whether a
  package's declared licence is accurate for its contents.
- The licence check reads SPDX expressions without being a full SPDX parser.
  Where it cannot be certain — any expression joining licences with `AND` — it
  requires every licence named to be permissive, which rejects some expressions
  a complete parse would accept. It errs towards asking a human.
- `khroma` 2.1.0 declares no `license` field in its `package.json`. Its bundled
  `license` file is the MIT License (Copyright 2019-present Fabio Spampinato,
  Andrew Maney). It is listed below as MIT on that basis, recorded in the
  generator as a named exception rather than inferred by it.

## Obligations we carry

Most dependencies are permissive (MIT, Apache-2.0, ISC, BSD). Three groups need
more than a mention, and are also named in `NOTICE`:

| Dependency | Licence | Why it is called out |
|---|---|---|
| `cssparser`, `cssparser-macros`, `dtoa-short`, `option-ext`, `selectors` | MPL-2.0 | File-level copyleft. Used unmodified; those files stay under MPL-2.0 and their source is on crates.io at the pinned versions. MPL-2.0 section 3.3 permits distribution in a larger work under other terms. |
| `webpki-roots`, `webpki-root-certs` | CDLA-Permissive-2.0 | Root certificate **data**, used unmodified. |
| `dompurify`, `r-efi` | dual/tri-licensed including MPL-2.0 or LGPL-2.1 | Each offers a permissive alternative. We elect **Apache-2.0** for `dompurify` and `MIT OR Apache-2.0` for `r-efi`, so no MPL or LGPL obligation attaches. |

`lightningcss` (MPL-2.0, together with the eleven `lightningcss-*` packages that
are the same project prebuilt for one platform each) and `caniuse-lite`
(CC-BY-4.0) are **build-time only**, reached through Vite, Tailwind and
browserslist. They are not present in the shipped application bundle.

**No dependency in either ecosystem is licensed under the GPL, AGPL, SSPL, BUSL,
CDDL, EPL, OSL, EUPL, the Commons Clause, or the Elastic License.** There are no
git-sourced Rust dependencies, so every crate's origin is a published crates.io
release.

## Rust crates (775)

### Summary by declared licence

| Licence | Crates |
|---|---|
| `MIT OR Apache-2.0` | 335 |
| `MIT` | 211 |
| `Apache-2.0 OR MIT` | 59 |
| `MIT/Apache-2.0` | 40 |
| `Zlib OR Apache-2.0 OR MIT` | 19 |
| `Unicode-3.0` | 18 |
| `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | 15 |
| `Unlicense OR MIT` | 13 |
| `Apache-2.0/MIT` | 7 |
| `Apache-2.0 OR ISC OR MIT` | 6 |
| `Apache-2.0` | 6 |
| `MPL-2.0` | 5 |
| `ISC` | 4 |
| `MIT OR Apache-2.0 OR Zlib` | 3 |
| `CDLA-Permissive-2.0` | 3 |
| `BSD-3-Clause` | 3 |
| `Zlib` | 2 |
| `Unlicense/MIT` | 2 |
| `BSL-1.0` | 2 |
| `BSD-3-Clause OR MIT OR Apache-2.0` | 2 |
| `BSD-2-Clause OR Apache-2.0 OR MIT` | 2 |
| `MIT-0` | 1 |
| `MIT OR Zlib OR Apache-2.0` | 1 |
| `MIT OR Apache-2.0 OR LGPL-2.1-or-later` | 1 |
| `MIT AND BSD-3-Clause` | 1 |
| `MIT / Apache-2.0` | 1 |
| `CC0-1.0 OR MIT-0 OR Apache-2.0` | 1 |
| `BSD-3-Clause/MIT` | 1 |
| `BSD-3-Clause AND MIT` | 1 |
| `BSD-2-Clause` | 1 |
| `Apache-2.0 WITH LLVM-exception` | 1 |
| `Apache-2.0 OR BSL-1.0` | 1 |
| `Apache-2.0 AND MIT` | 1 |
| `Apache-2.0 AND ISC` | 1 |
| `Apache-2.0 / MIT` | 1 |
| `0BSD OR MIT OR Apache-2.0` | 1 |
| `0BSD` | 1 |
| `(MIT OR Apache-2.0) AND Unicode-3.0` | 1 |
| `(Apache-2.0 OR MIT) AND BSD-3-Clause` | 1 |

### Full list

| Crate | Version | Licence |
|---|---|---|
| `adler2` | 2.0.1 | 0BSD OR MIT OR Apache-2.0 |
| `aes` | 0.8.4 | MIT OR Apache-2.0 |
| `ahash` | 0.8.12 | MIT OR Apache-2.0 |
| `aho-corasick` | 1.1.4 | Unlicense OR MIT |
| `aliasable` | 0.1.3 | MIT |
| `alloc-no-stdlib` | 2.0.4 | BSD-3-Clause |
| `alloc-stdlib` | 0.2.2 | BSD-3-Clause |
| `android_system_properties` | 0.1.5 | MIT/Apache-2.0 |
| `anstream` | 0.6.21 | MIT OR Apache-2.0 |
| `anstyle` | 1.0.14 | MIT OR Apache-2.0 |
| `anstyle-parse` | 0.2.7 | MIT OR Apache-2.0 |
| `anstyle-query` | 1.1.5 | MIT OR Apache-2.0 |
| `anstyle-wincon` | 3.0.11 | MIT OR Apache-2.0 |
| `anyhow` | 1.0.102 | MIT OR Apache-2.0 |
| `arbitrary` | 1.4.2 | MIT OR Apache-2.0 |
| `ashpd` | 0.11.1 | MIT |
| `assert-json-diff` | 2.0.2 | MIT |
| `async-broadcast` | 0.7.2 | MIT OR Apache-2.0 |
| `async-channel` | 2.5.0 | Apache-2.0 OR MIT |
| `async-executor` | 1.14.0 | Apache-2.0 OR MIT |
| `async-fs` | 2.2.0 | Apache-2.0 OR MIT |
| `async-io` | 2.6.0 | Apache-2.0 OR MIT |
| `async-lock` | 3.4.2 | Apache-2.0 OR MIT |
| `async-net` | 2.0.0 | Apache-2.0 OR MIT |
| `async-process` | 2.5.0 | Apache-2.0 OR MIT |
| `async-recursion` | 1.1.1 | MIT OR Apache-2.0 |
| `async-signal` | 0.2.13 | Apache-2.0 OR MIT |
| `async-task` | 4.7.1 | Apache-2.0 OR MIT |
| `async-trait` | 0.1.89 | MIT OR Apache-2.0 |
| `atk` | 0.18.2 | MIT |
| `atk-sys` | 0.18.2 | MIT |
| `atomic-waker` | 1.1.2 | Apache-2.0 OR MIT |
| `autocfg` | 1.5.0 | Apache-2.0 OR MIT |
| `axum` | 0.8.8 | MIT |
| `axum-core` | 0.5.6 | MIT |
| `axum-macros` | 0.5.0 | MIT |
| `base64` | 0.21.7 | MIT OR Apache-2.0 |
| `base64` | 0.22.1 | MIT OR Apache-2.0 |
| `bit-set` | 0.8.0 | Apache-2.0 OR MIT |
| `bit-vec` | 0.8.0 | Apache-2.0 OR MIT |
| `bitflags` | 1.3.2 | MIT/Apache-2.0 |
| `bitflags` | 2.11.0 | MIT OR Apache-2.0 |
| `block` | 0.1.6 | MIT |
| `block-buffer` | 0.10.4 | MIT OR Apache-2.0 |
| `block-buffer` | 0.12.1 | MIT OR Apache-2.0 |
| `block-padding` | 0.3.3 | MIT OR Apache-2.0 |
| `block2` | 0.6.2 | MIT |
| `blocking` | 1.6.2 | Apache-2.0 OR MIT |
| `borrow-or-share` | 0.2.4 | MIT-0 |
| `brotli` | 8.0.2 | BSD-3-Clause AND MIT |
| `brotli-decompressor` | 5.0.0 | BSD-3-Clause/MIT |
| `bstr` | 1.12.1 | MIT OR Apache-2.0 |
| `bufstream` | 0.1.4 | MIT/Apache-2.0 |
| `bumpalo` | 3.20.2 | MIT OR Apache-2.0 |
| `bytecount` | 0.6.9 | Apache-2.0/MIT |
| `bytemuck` | 1.25.0 | Zlib OR Apache-2.0 OR MIT |
| `byteorder` | 1.5.0 | Unlicense OR MIT |
| `bytes` | 1.11.1 | MIT |
| `cairo-rs` | 0.18.5 | MIT |
| `cairo-sys-rs` | 0.18.2 | MIT |
| `camino` | 1.2.2 | MIT OR Apache-2.0 |
| `cargo-platform` | 0.1.9 | MIT OR Apache-2.0 |
| `cargo_metadata` | 0.19.2 | MIT |
| `cargo_toml` | 0.22.3 | Apache-2.0 OR MIT |
| `cbc` | 0.1.2 | MIT OR Apache-2.0 |
| `cc` | 1.2.56 | MIT OR Apache-2.0 |
| `cesu8` | 1.1.0 | Apache-2.0/MIT |
| `cfb` | 0.7.3 | MIT |
| `cfg-expr` | 0.15.8 | MIT OR Apache-2.0 |
| `cfg-if` | 1.0.4 | MIT OR Apache-2.0 |
| `cfg_aliases` | 0.1.1 | MIT |
| `cfg_aliases` | 0.2.1 | MIT |
| `chacha20` | 0.10.1 | MIT OR Apache-2.0 |
| `chrono` | 0.4.44 | MIT OR Apache-2.0 |
| `chrono-tz` | 0.10.4 | MIT OR Apache-2.0 |
| `cipher` | 0.4.4 | MIT OR Apache-2.0 |
| `clap` | 4.5.60 | MIT OR Apache-2.0 |
| `clap_builder` | 4.5.60 | MIT OR Apache-2.0 |
| `clap_derive` | 4.5.55 | MIT OR Apache-2.0 |
| `clap_lex` | 1.1.0 | MIT OR Apache-2.0 |
| `clipboard-win` | 5.4.1 | BSL-1.0 |
| `colorchoice` | 1.0.5 | MIT OR Apache-2.0 |
| `combine` | 4.6.7 | MIT |
| `concurrent-queue` | 2.5.0 | Apache-2.0 OR MIT |
| `console` | 0.16.3 | MIT |
| `const-oid` | 0.10.2 | Apache-2.0 OR MIT |
| `convert_case` | 0.4.0 | MIT |
| `cookie` | 0.18.1 | MIT OR Apache-2.0 |
| `core-foundation` | 0.9.4 | MIT OR Apache-2.0 |
| `core-foundation` | 0.10.1 | MIT OR Apache-2.0 |
| `core-foundation-sys` | 0.8.7 | MIT OR Apache-2.0 |
| `core-graphics` | 0.24.0 | MIT OR Apache-2.0 |
| `core-graphics-types` | 0.2.0 | MIT OR Apache-2.0 |
| `cpufeatures` | 0.2.17 | MIT OR Apache-2.0 |
| `cpufeatures` | 0.3.0 | MIT OR Apache-2.0 |
| `crc32fast` | 1.5.0 | MIT OR Apache-2.0 |
| `croner` | 3.0.1 | MIT |
| `crossbeam-channel` | 0.5.15 | MIT OR Apache-2.0 |
| `crossbeam-deque` | 0.8.6 | MIT OR Apache-2.0 |
| `crossbeam-epoch` | 0.9.18 | MIT OR Apache-2.0 |
| `crossbeam-utils` | 0.8.21 | MIT OR Apache-2.0 |
| `crypto-common` | 0.1.7 | MIT OR Apache-2.0 |
| `crypto-common` | 0.2.2 | MIT OR Apache-2.0 |
| `cssparser` | 0.29.6 | MPL-2.0 |
| `cssparser-macros` | 0.6.1 | MPL-2.0 |
| `ctor` | 0.2.9 | Apache-2.0 OR MIT |
| `darling` | 0.20.11 | MIT |
| `darling` | 0.21.3 | MIT |
| `darling_core` | 0.20.11 | MIT |
| `darling_core` | 0.21.3 | MIT |
| `darling_macro` | 0.20.11 | MIT |
| `darling_macro` | 0.21.3 | MIT |
| `dashmap` | 6.1.0 | MIT |
| `data-encoding` | 2.11.0 | MIT |
| `dbus` | 0.9.10 | Apache-2.0/MIT |
| `dbus-secret-service` | 4.1.0 | MIT OR Apache-2.0 |
| `deadpool` | 0.12.3 | MIT OR Apache-2.0 |
| `deadpool-runtime` | 0.1.4 | MIT OR Apache-2.0 |
| `deranged` | 0.5.8 | MIT OR Apache-2.0 |
| `derive_arbitrary` | 1.4.2 | MIT OR Apache-2.0 |
| `derive_builder` | 0.20.2 | MIT OR Apache-2.0 |
| `derive_builder_core` | 0.20.2 | MIT OR Apache-2.0 |
| `derive_builder_macro` | 0.20.2 | MIT OR Apache-2.0 |
| `derive_more` | 0.99.20 | MIT |
| `digest` | 0.10.7 | MIT OR Apache-2.0 |
| `digest` | 0.11.3 | MIT OR Apache-2.0 |
| `dirs` | 6.0.0 | MIT OR Apache-2.0 |
| `dirs-sys` | 0.5.0 | MIT OR Apache-2.0 |
| `dispatch` | 0.2.0 | MIT |
| `dispatch2` | 0.3.0 | Zlib OR Apache-2.0 OR MIT |
| `displaydoc` | 0.2.5 | MIT OR Apache-2.0 |
| `dlib` | 0.5.3 | MIT |
| `dlopen2` | 0.8.2 | MIT |
| `dlopen2_derive` | 0.4.3 | MIT |
| `doc-comment` | 0.3.4 | MIT |
| `downcast-rs` | 1.2.1 | MIT/Apache-2.0 |
| `dpi` | 0.1.2 | Apache-2.0 AND MIT |
| `dtoa` | 1.0.11 | MIT OR Apache-2.0 |
| `dtoa-short` | 0.3.5 | MPL-2.0 |
| `dunce` | 1.0.5 | CC0-1.0 OR MIT-0 OR Apache-2.0 |
| `dyn-clone` | 1.0.20 | MIT OR Apache-2.0 |
| `email-encoding` | 0.4.1 | MIT OR Apache-2.0 |
| `email_address` | 0.2.9 | MIT |
| `embed-resource` | 3.0.6 | MIT |
| `embed_plist` | 1.2.2 | MIT OR Apache-2.0 |
| `encode_unicode` | 1.0.0 | Apache-2.0 OR MIT |
| `encoding_rs` | 0.8.35 | (Apache-2.0 OR MIT) AND BSD-3-Clause |
| `encoding_rs_io` | 0.1.7 | MIT OR Apache-2.0 |
| `endi` | 1.1.1 | MIT |
| `endian-type` | 0.1.2 | MIT |
| `enumflags2` | 0.7.12 | MIT OR Apache-2.0 |
| `enumflags2_derive` | 0.7.12 | MIT OR Apache-2.0 |
| `equivalent` | 1.0.2 | Apache-2.0 OR MIT |
| `erased-serde` | 0.4.9 | MIT OR Apache-2.0 |
| `errno` | 0.3.14 | MIT OR Apache-2.0 |
| `error-code` | 3.3.2 | BSL-1.0 |
| `event-listener` | 5.4.1 | Apache-2.0 OR MIT |
| `event-listener-strategy` | 0.5.4 | Apache-2.0 OR MIT |
| `fallible-iterator` | 0.3.0 | MIT/Apache-2.0 |
| `fallible-streaming-iterator` | 0.1.9 | MIT/Apache-2.0 |
| `fancy-regex` | 0.14.0 | MIT |
| `fancy-regex` | 0.17.0 | MIT |
| `fastrand` | 2.3.0 | Apache-2.0 OR MIT |
| `fd-lock` | 4.0.4 | MIT OR Apache-2.0 |
| `fdeflate` | 0.3.7 | MIT OR Apache-2.0 |
| `field-offset` | 0.3.6 | MIT OR Apache-2.0 |
| `filetime` | 0.2.27 | MIT/Apache-2.0 |
| `find-msvc-tools` | 0.1.9 | MIT OR Apache-2.0 |
| `flate2` | 1.1.9 | MIT OR Apache-2.0 |
| `fluent-uri` | 0.3.2 | MIT |
| `fnv` | 1.0.7 | Apache-2.0 / MIT |
| `foldhash` | 0.1.5 | Zlib |
| `foldhash` | 0.2.0 | Zlib |
| `foreign-types` | 0.5.0 | MIT/Apache-2.0 |
| `foreign-types-macros` | 0.2.3 | MIT/Apache-2.0 |
| `foreign-types-shared` | 0.3.1 | MIT/Apache-2.0 |
| `form_urlencoded` | 1.2.2 | MIT OR Apache-2.0 |
| `fraction` | 0.15.4 | MIT OR Apache-2.0 |
| `futf` | 0.1.5 | MIT / Apache-2.0 |
| `futures` | 0.3.32 | MIT OR Apache-2.0 |
| `futures-channel` | 0.3.32 | MIT OR Apache-2.0 |
| `futures-core` | 0.3.32 | MIT OR Apache-2.0 |
| `futures-executor` | 0.3.32 | MIT OR Apache-2.0 |
| `futures-io` | 0.3.32 | MIT OR Apache-2.0 |
| `futures-lite` | 2.6.1 | Apache-2.0 OR MIT |
| `futures-macro` | 0.3.32 | MIT OR Apache-2.0 |
| `futures-sink` | 0.3.32 | MIT OR Apache-2.0 |
| `futures-task` | 0.3.32 | MIT OR Apache-2.0 |
| `futures-util` | 0.3.32 | MIT OR Apache-2.0 |
| `fxhash` | 0.2.1 | Apache-2.0/MIT |
| `gdk` | 0.18.2 | MIT |
| `gdk-pixbuf` | 0.18.5 | MIT |
| `gdk-pixbuf-sys` | 0.18.0 | MIT |
| `gdk-sys` | 0.18.2 | MIT |
| `gdkwayland-sys` | 0.18.2 | MIT |
| `gdkx11` | 0.18.2 | MIT |
| `gdkx11-sys` | 0.18.2 | MIT |
| `generic-array` | 0.14.7 | MIT |
| `getrandom` | 0.1.16 | MIT OR Apache-2.0 |
| `getrandom` | 0.2.17 | MIT OR Apache-2.0 |
| `getrandom` | 0.3.4 | MIT OR Apache-2.0 |
| `getrandom` | 0.4.1 | MIT OR Apache-2.0 |
| `gio` | 0.18.4 | MIT |
| `gio-sys` | 0.18.1 | MIT |
| `glib` | 0.18.5 | MIT |
| `glib-macros` | 0.18.5 | MIT |
| `glib-sys` | 0.18.1 | MIT |
| `glob` | 0.3.3 | MIT OR Apache-2.0 |
| `globset` | 0.4.18 | Unlicense OR MIT |
| `gobject-sys` | 0.18.0 | MIT |
| `grep` | 0.3.2 | Unlicense OR MIT |
| `grep-cli` | 0.1.12 | Unlicense OR MIT |
| `grep-matcher` | 0.1.8 | Unlicense OR MIT |
| `grep-printer` | 0.2.2 | Unlicense OR MIT |
| `grep-regex` | 0.1.14 | Unlicense OR MIT |
| `grep-searcher` | 0.1.16 | Unlicense OR MIT |
| `gtk` | 0.18.2 | MIT |
| `gtk-sys` | 0.18.2 | MIT |
| `gtk3-macros` | 0.18.2 | MIT |
| `h2` | 0.4.14 | MIT |
| `hashbrown` | 0.12.3 | MIT OR Apache-2.0 |
| `hashbrown` | 0.14.5 | MIT OR Apache-2.0 |
| `hashbrown` | 0.15.5 | MIT OR Apache-2.0 |
| `hashbrown` | 0.16.1 | MIT OR Apache-2.0 |
| `hashify` | 0.2.9 | Apache-2.0 OR MIT |
| `hashlink` | 0.11.1 | MIT OR Apache-2.0 |
| `heck` | 0.4.1 | MIT OR Apache-2.0 |
| `heck` | 0.5.0 | MIT OR Apache-2.0 |
| `hermit-abi` | 0.5.2 | MIT OR Apache-2.0 |
| `hex` | 0.4.3 | MIT OR Apache-2.0 |
| `hkdf` | 0.12.4 | MIT OR Apache-2.0 |
| `hmac` | 0.12.1 | MIT OR Apache-2.0 |
| `home` | 0.5.12 | MIT OR Apache-2.0 |
| `hostname` | 0.4.2 | MIT |
| `html5ever` | 0.29.1 | MIT OR Apache-2.0 |
| `http` | 1.4.0 | MIT OR Apache-2.0 |
| `http-body` | 1.0.1 | MIT |
| `http-body-util` | 0.1.3 | MIT |
| `httparse` | 1.10.1 | MIT OR Apache-2.0 |
| `httpdate` | 1.0.3 | MIT OR Apache-2.0 |
| `hybrid-array` | 0.4.13 | MIT OR Apache-2.0 |
| `hyper` | 1.8.1 | MIT |
| `hyper-rustls` | 0.27.7 | Apache-2.0 OR ISC OR MIT |
| `hyper-util` | 0.1.20 | MIT |
| `iana-time-zone` | 0.1.65 | MIT OR Apache-2.0 |
| `iana-time-zone-haiku` | 0.1.2 | MIT OR Apache-2.0 |
| `ico` | 0.5.0 | MIT |
| `icu_collections` | 2.1.1 | Unicode-3.0 |
| `icu_locale_core` | 2.1.1 | Unicode-3.0 |
| `icu_normalizer` | 2.1.1 | Unicode-3.0 |
| `icu_normalizer_data` | 2.1.1 | Unicode-3.0 |
| `icu_properties` | 2.1.2 | Unicode-3.0 |
| `icu_properties_data` | 2.1.2 | Unicode-3.0 |
| `icu_provider` | 2.1.1 | Unicode-3.0 |
| `id-arena` | 2.3.0 | MIT/Apache-2.0 |
| `ident_case` | 1.0.1 | MIT/Apache-2.0 |
| `idna` | 1.1.0 | MIT OR Apache-2.0 |
| `idna_adapter` | 1.2.1 | Apache-2.0 OR MIT |
| `ignore` | 0.4.25 | Unlicense OR MIT |
| `imap` | 3.0.0-alpha.15 | Apache-2.0 OR MIT |
| `imap-proto` | 0.16.7 | MIT OR Apache-2.0 |
| `indexmap` | 1.9.3 | Apache-2.0 OR MIT |
| `indexmap` | 2.13.0 | Apache-2.0 OR MIT |
| `infer` | 0.19.0 | MIT |
| `inout` | 0.1.4 | MIT OR Apache-2.0 |
| `insta` | 1.47.2 | Apache-2.0 |
| `ipnet` | 2.11.0 | MIT OR Apache-2.0 |
| `iri-string` | 0.7.10 | MIT OR Apache-2.0 |
| `is-docker` | 0.2.0 | MIT |
| `is-wsl` | 0.4.0 | MIT |
| `is_terminal_polyfill` | 1.70.2 | MIT OR Apache-2.0 |
| `itoa` | 1.0.17 | MIT OR Apache-2.0 |
| `javascriptcore-rs` | 1.1.2 | MIT |
| `javascriptcore-rs-sys` | 1.1.1 | MIT |
| `jni` | 0.21.1 | MIT/Apache-2.0 |
| `jni-sys` | 0.3.0 | MIT/Apache-2.0 |
| `js-sys` | 0.3.90 | MIT OR Apache-2.0 |
| `json-patch` | 3.0.1 | MIT/Apache-2.0 |
| `jsonptr` | 0.6.3 | MIT OR Apache-2.0 |
| `jsonschema` | 0.30.0 | MIT |
| `keyboard-types` | 0.7.0 | MIT OR Apache-2.0 |
| `keyring` | 3.6.3 | MIT OR Apache-2.0 |
| `kuchikiki` | 0.8.8-speedreader | MIT |
| `lazy_static` | 1.5.0 | MIT OR Apache-2.0 |
| `leb128fmt` | 0.1.0 | MIT OR Apache-2.0 |
| `lettre` | 0.11.22 | MIT |
| `libappindicator` | 0.9.0 | Apache-2.0 OR MIT |
| `libappindicator-sys` | 0.9.0 | Apache-2.0 OR MIT |
| `libc` | 0.2.182 | MIT OR Apache-2.0 |
| `libdbus-sys` | 0.2.7 | Apache-2.0/MIT |
| `libloading` | 0.7.4 | ISC |
| `libredox` | 0.1.12 | MIT |
| `libsqlite3-sys` | 0.37.0 | MIT |
| `linux-keyutils` | 0.2.5 | Apache-2.0 OR MIT |
| `linux-raw-sys` | 0.12.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| `litemap` | 0.8.1 | Unicode-3.0 |
| `lock_api` | 0.4.14 | MIT OR Apache-2.0 |
| `log` | 0.4.29 | MIT OR Apache-2.0 |
| `lru-slab` | 0.1.2 | MIT OR Apache-2.0 OR Zlib |
| `mac` | 0.1.1 | MIT/Apache-2.0 |
| `mac-notification-sys` | 0.6.15 | MIT/Apache-2.0 |
| `mail-parser` | 0.11.5 | Apache-2.0 OR MIT |
| `malloc_buf` | 0.0.6 | MIT |
| `markup5ever` | 0.14.1 | MIT OR Apache-2.0 |
| `match_token` | 0.1.0 | MIT OR Apache-2.0 |
| `matchers` | 0.2.0 | MIT |
| `matches` | 0.1.10 | MIT |
| `matchit` | 0.8.4 | MIT AND BSD-3-Clause |
| `memchr` | 2.8.0 | Unlicense OR MIT |
| `memmap2` | 0.9.10 | MIT OR Apache-2.0 |
| `memoffset` | 0.9.1 | MIT |
| `mime` | 0.3.17 | MIT OR Apache-2.0 |
| `minimal-lexical` | 0.2.1 | MIT/Apache-2.0 |
| `minisign-verify` | 0.2.5 | MIT |
| `miniz_oxide` | 0.8.9 | MIT OR Zlib OR Apache-2.0 |
| `mio` | 1.1.1 | MIT |
| `muda` | 0.17.1 | Apache-2.0 OR MIT |
| `multer` | 3.1.0 | MIT |
| `ndk` | 0.9.0 | MIT OR Apache-2.0 |
| `ndk-context` | 0.1.1 | MIT OR Apache-2.0 |
| `ndk-sys` | 0.6.0+11769913 | MIT OR Apache-2.0 |
| `new_debug_unreachable` | 1.0.6 | MIT |
| `nibble_vec` | 0.1.0 | MIT |
| `nix` | 0.28.0 | MIT |
| `nix` | 0.29.0 | MIT |
| `nodrop` | 0.1.14 | MIT/Apache-2.0 |
| `nom` | 7.1.3 | MIT |
| `nom` | 8.0.0 | MIT |
| `nosleep` | 0.2.1 | MIT |
| `nosleep-mac-sys` | 0.2.1 | MIT |
| `nosleep-nix` | 0.2.1 | MIT |
| `nosleep-types` | 0.2.1 | MIT |
| `nosleep-windows` | 0.2.1 | MIT |
| `notify-rust` | 4.18.0 | MIT OR Apache-2.0 |
| `nu-ansi-term` | 0.50.3 | MIT |
| `num` | 0.4.3 | MIT OR Apache-2.0 |
| `num-bigint` | 0.4.6 | MIT OR Apache-2.0 |
| `num-cmp` | 0.1.0 | MIT/Apache-2.0 |
| `num-complex` | 0.4.6 | MIT OR Apache-2.0 |
| `num-conv` | 0.2.0 | MIT OR Apache-2.0 |
| `num-integer` | 0.1.46 | MIT OR Apache-2.0 |
| `num-iter` | 0.1.45 | MIT OR Apache-2.0 |
| `num-rational` | 0.4.2 | MIT OR Apache-2.0 |
| `num-traits` | 0.2.19 | MIT OR Apache-2.0 |
| `num_cpus` | 1.17.0 | MIT OR Apache-2.0 |
| `num_enum` | 0.7.5 | BSD-3-Clause OR MIT OR Apache-2.0 |
| `num_enum_derive` | 0.7.5 | BSD-3-Clause OR MIT OR Apache-2.0 |
| `objc` | 0.2.7 | MIT |
| `objc-foundation` | 0.1.1 | MIT |
| `objc2` | 0.6.3 | MIT |
| `objc2-app-kit` | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| `objc2-cloud-kit` | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| `objc2-core-data` | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| `objc2-core-foundation` | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| `objc2-core-graphics` | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| `objc2-core-image` | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| `objc2-core-text` | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| `objc2-core-video` | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| `objc2-encode` | 4.1.0 | MIT |
| `objc2-exception-helper` | 0.1.1 | Zlib OR Apache-2.0 OR MIT |
| `objc2-foundation` | 0.3.2 | MIT |
| `objc2-io-surface` | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| `objc2-javascript-core` | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| `objc2-osa-kit` | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| `objc2-quartz-core` | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| `objc2-security` | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| `objc2-ui-kit` | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| `objc2-web-kit` | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| `objc_id` | 0.1.1 | MIT |
| `once_cell` | 1.21.3 | MIT OR Apache-2.0 |
| `once_cell_polyfill` | 1.70.2 | MIT OR Apache-2.0 |
| `open` | 5.3.3 | MIT |
| `openssl-probe` | 0.1.6 | MIT/Apache-2.0 |
| `openssl-probe` | 0.2.1 | MIT OR Apache-2.0 |
| `option-ext` | 0.2.0 | MPL-2.0 |
| `ordered-stream` | 0.2.0 | MIT OR Apache-2.0 |
| `osakit` | 0.3.1 | MIT OR Apache-2.0 |
| `ouroboros` | 0.18.5 | MIT OR Apache-2.0 |
| `ouroboros_macro` | 0.18.5 | MIT OR Apache-2.0 |
| `outref` | 0.5.2 | MIT |
| `pango` | 0.18.3 | MIT |
| `pango-sys` | 0.18.0 | MIT |
| `parking` | 2.2.1 | Apache-2.0 OR MIT |
| `parking_lot` | 0.12.5 | MIT OR Apache-2.0 |
| `parking_lot_core` | 0.9.12 | MIT OR Apache-2.0 |
| `pathdiff` | 0.2.3 | MIT/Apache-2.0 |
| `percent-encoding` | 2.3.2 | MIT OR Apache-2.0 |
| `phf` | 0.8.0 | MIT |
| `phf` | 0.10.1 | MIT |
| `phf` | 0.11.3 | MIT |
| `phf` | 0.12.1 | MIT |
| `phf_codegen` | 0.8.0 | MIT |
| `phf_codegen` | 0.11.3 | MIT |
| `phf_generator` | 0.8.0 | MIT |
| `phf_generator` | 0.10.0 | MIT |
| `phf_generator` | 0.11.3 | MIT |
| `phf_macros` | 0.10.0 | MIT |
| `phf_macros` | 0.11.3 | MIT |
| `phf_shared` | 0.8.0 | MIT |
| `phf_shared` | 0.10.0 | MIT |
| `phf_shared` | 0.11.3 | MIT |
| `phf_shared` | 0.12.1 | MIT |
| `pin-project-lite` | 0.2.16 | Apache-2.0 OR MIT |
| `pin-utils` | 0.1.0 | MIT OR Apache-2.0 |
| `piper` | 0.2.4 | MIT OR Apache-2.0 |
| `pkg-config` | 0.3.32 | MIT OR Apache-2.0 |
| `plist` | 1.8.0 | MIT |
| `png` | 0.17.16 | MIT OR Apache-2.0 |
| `polling` | 3.11.0 | Apache-2.0 OR MIT |
| `pollster` | 0.4.0 | Apache-2.0/MIT |
| `potential_utf` | 0.1.4 | Unicode-3.0 |
| `powerfmt` | 0.2.0 | MIT OR Apache-2.0 |
| `ppv-lite86` | 0.2.21 | MIT OR Apache-2.0 |
| `precomputed-hash` | 0.1.1 | MIT |
| `prettyplease` | 0.2.37 | MIT OR Apache-2.0 |
| `proc-macro-crate` | 1.3.1 | MIT OR Apache-2.0 |
| `proc-macro-crate` | 2.0.0 | MIT OR Apache-2.0 |
| `proc-macro-crate` | 3.4.0 | MIT OR Apache-2.0 |
| `proc-macro-error` | 1.0.4 | MIT OR Apache-2.0 |
| `proc-macro-error-attr` | 1.0.4 | MIT OR Apache-2.0 |
| `proc-macro-hack` | 0.5.20+deprecated | MIT OR Apache-2.0 |
| `proc-macro2` | 1.0.106 | MIT OR Apache-2.0 |
| `proc-macro2-diagnostics` | 0.10.1 | MIT/Apache-2.0 |
| `quick-xml` | 0.38.4 | MIT |
| `quick-xml` | 0.41.0 | MIT |
| `quinn` | 0.11.9 | MIT OR Apache-2.0 |
| `quinn-proto` | 0.11.14 | MIT OR Apache-2.0 |
| `quinn-udp` | 0.5.14 | MIT OR Apache-2.0 |
| `quote` | 1.0.44 | MIT OR Apache-2.0 |
| `quoted_printable` | 0.5.2 | 0BSD |
| `r-efi` | 5.3.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later |
| `radix_trie` | 0.2.1 | MIT |
| `rand` | 0.7.3 | MIT OR Apache-2.0 |
| `rand` | 0.8.5 | MIT OR Apache-2.0 |
| `rand` | 0.9.4 | MIT OR Apache-2.0 |
| `rand` | 0.10.2 | MIT OR Apache-2.0 |
| `rand_chacha` | 0.2.2 | MIT OR Apache-2.0 |
| `rand_chacha` | 0.3.1 | MIT OR Apache-2.0 |
| `rand_chacha` | 0.9.0 | MIT OR Apache-2.0 |
| `rand_core` | 0.5.1 | MIT OR Apache-2.0 |
| `rand_core` | 0.6.4 | MIT OR Apache-2.0 |
| `rand_core` | 0.9.5 | MIT OR Apache-2.0 |
| `rand_core` | 0.10.1 | MIT OR Apache-2.0 |
| `rand_hc` | 0.2.0 | MIT/Apache-2.0 |
| `rand_pcg` | 0.2.1 | MIT OR Apache-2.0 |
| `raw-window-handle` | 0.6.2 | MIT OR Apache-2.0 OR Zlib |
| `redox_syscall` | 0.5.18 | MIT |
| `redox_syscall` | 0.7.3 | MIT |
| `redox_users` | 0.5.2 | MIT |
| `ref-cast` | 1.0.25 | MIT OR Apache-2.0 |
| `ref-cast-impl` | 1.0.25 | MIT OR Apache-2.0 |
| `referencing` | 0.30.0 | MIT |
| `regex` | 1.12.3 | MIT OR Apache-2.0 |
| `regex-automata` | 0.4.14 | MIT OR Apache-2.0 |
| `regex-syntax` | 0.8.10 | MIT OR Apache-2.0 |
| `reqwest` | 0.12.28 | MIT OR Apache-2.0 |
| `reqwest` | 0.13.2 | MIT OR Apache-2.0 |
| `rfd` | 0.16.0 | MIT |
| `ring` | 0.17.14 | Apache-2.0 AND ISC |
| `rsqlite-vfs` | 0.1.1 | MIT |
| `rusqlite` | 0.39.0 | MIT |
| `rustc-hash` | 1.1.0 | Apache-2.0/MIT |
| `rustc-hash` | 2.1.2 | Apache-2.0 OR MIT |
| `rustc_version` | 0.4.1 | MIT OR Apache-2.0 |
| `rustix` | 1.1.4 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| `rustls` | 0.22.4 | Apache-2.0 OR ISC OR MIT |
| `rustls` | 0.23.37 | Apache-2.0 OR ISC OR MIT |
| `rustls-connector` | 0.19.2 | BSD-2-Clause |
| `rustls-native-certs` | 0.7.3 | Apache-2.0 OR ISC OR MIT |
| `rustls-native-certs` | 0.8.3 | Apache-2.0 OR ISC OR MIT |
| `rustls-pemfile` | 2.2.0 | Apache-2.0 OR ISC OR MIT |
| `rustls-pki-types` | 1.14.0 | MIT OR Apache-2.0 |
| `rustls-platform-verifier` | 0.6.2 | MIT OR Apache-2.0 |
| `rustls-platform-verifier-android` | 0.1.1 | MIT OR Apache-2.0 |
| `rustls-webpki` | 0.102.8 | ISC |
| `rustls-webpki` | 0.103.10 | ISC |
| `rustversion` | 1.0.22 | MIT OR Apache-2.0 |
| `rustyline` | 14.0.0 | MIT |
| `ryu` | 1.0.23 | Apache-2.0 OR BSL-1.0 |
| `same-file` | 1.0.6 | Unlicense/MIT |
| `scc` | 2.4.0 | Apache-2.0 |
| `schannel` | 0.1.29 | MIT |
| `schemars` | 0.8.22 | MIT |
| `schemars` | 0.9.0 | MIT |
| `schemars` | 1.2.1 | MIT |
| `schemars_derive` | 0.8.22 | MIT |
| `scoped-tls` | 1.0.1 | MIT/Apache-2.0 |
| `scopeguard` | 1.2.0 | MIT OR Apache-2.0 |
| `sdd` | 3.0.10 | Apache-2.0 |
| `secret-service` | 4.0.0 | MIT OR Apache-2.0 |
| `security-framework` | 2.11.1 | MIT OR Apache-2.0 |
| `security-framework` | 3.7.0 | MIT OR Apache-2.0 |
| `security-framework-sys` | 2.17.0 | MIT OR Apache-2.0 |
| `selectors` | 0.24.0 | MPL-2.0 |
| `semver` | 1.0.27 | MIT OR Apache-2.0 |
| `serde` | 1.0.228 | MIT OR Apache-2.0 |
| `serde-untagged` | 0.1.9 | MIT OR Apache-2.0 |
| `serde_core` | 1.0.228 | MIT OR Apache-2.0 |
| `serde_derive` | 1.0.228 | MIT OR Apache-2.0 |
| `serde_derive_internals` | 0.29.1 | MIT OR Apache-2.0 |
| `serde_json` | 1.0.149 | MIT OR Apache-2.0 |
| `serde_path_to_error` | 0.1.20 | MIT OR Apache-2.0 |
| `serde_repr` | 0.1.20 | MIT OR Apache-2.0 |
| `serde_spanned` | 0.6.9 | MIT OR Apache-2.0 |
| `serde_spanned` | 1.0.4 | MIT OR Apache-2.0 |
| `serde_urlencoded` | 0.7.1 | MIT/Apache-2.0 |
| `serde_with` | 3.17.0 | MIT OR Apache-2.0 |
| `serde_with_macros` | 3.17.0 | MIT OR Apache-2.0 |
| `serde_yaml` | 0.9.34+deprecated | MIT OR Apache-2.0 |
| `serial_test` | 3.4.0 | MIT |
| `serial_test_derive` | 3.4.0 | MIT |
| `serialize-to-javascript` | 0.1.2 | MIT OR Apache-2.0 |
| `serialize-to-javascript-impl` | 0.1.2 | MIT OR Apache-2.0 |
| `servo_arc` | 0.2.0 | MIT OR Apache-2.0 |
| `sha1` | 0.10.6 | MIT OR Apache-2.0 |
| `sha1` | 0.11.0 | MIT OR Apache-2.0 |
| `sha2` | 0.10.9 | MIT OR Apache-2.0 |
| `sharded-slab` | 0.1.7 | MIT |
| `shlex` | 1.3.0 | MIT OR Apache-2.0 |
| `signal-hook-registry` | 1.4.8 | MIT OR Apache-2.0 |
| `simd-adler32` | 0.3.8 | MIT |
| `similar` | 2.7.0 | Apache-2.0 |
| `siphasher` | 0.3.11 | MIT/Apache-2.0 |
| `siphasher` | 1.0.2 | MIT/Apache-2.0 |
| `slab` | 0.4.12 | MIT |
| `smallvec` | 1.15.1 | MIT OR Apache-2.0 |
| `snafu` | 0.7.5 | MIT OR Apache-2.0 |
| `snafu-derive` | 0.7.5 | MIT OR Apache-2.0 |
| `socket2` | 0.6.2 | MIT OR Apache-2.0 |
| `softbuffer` | 0.4.8 | MIT OR Apache-2.0 |
| `soup3` | 0.5.0 | MIT |
| `soup3-sys` | 0.5.0 | MIT |
| `spin` | 0.9.8 | MIT |
| `sqlite-wasm-rs` | 0.5.5 | MIT |
| `stable_deref_trait` | 1.2.1 | MIT OR Apache-2.0 |
| `static_assertions` | 1.1.0 | MIT OR Apache-2.0 |
| `string_cache` | 0.8.9 | MIT OR Apache-2.0 |
| `string_cache_codegen` | 0.5.4 | MIT OR Apache-2.0 |
| `strsim` | 0.11.1 | MIT |
| `strum` | 0.27.2 | MIT |
| `strum_macros` | 0.27.2 | MIT |
| `subtle` | 2.6.1 | BSD-3-Clause |
| `swift-rs` | 1.0.7 | MIT OR Apache-2.0 |
| `syn` | 1.0.109 | MIT OR Apache-2.0 |
| `syn` | 2.0.117 | MIT OR Apache-2.0 |
| `sync_wrapper` | 1.0.2 | Apache-2.0 |
| `synstructure` | 0.13.2 | MIT |
| `system-deps` | 6.2.2 | MIT OR Apache-2.0 |
| `tao` | 0.34.5 | Apache-2.0 |
| `tao-macros` | 0.1.3 | MIT OR Apache-2.0 |
| `tar` | 0.4.45 | MIT OR Apache-2.0 |
| `target-lexicon` | 0.12.16 | Apache-2.0 WITH LLVM-exception |
| `tauri` | 2.10.2 | Apache-2.0 OR MIT |
| `tauri-build` | 2.5.5 | Apache-2.0 OR MIT |
| `tauri-codegen` | 2.5.4 | Apache-2.0 OR MIT |
| `tauri-macros` | 2.5.4 | Apache-2.0 OR MIT |
| `tauri-plugin` | 2.5.3 | Apache-2.0 OR MIT |
| `tauri-plugin-dialog` | 2.6.0 | Apache-2.0 OR MIT |
| `tauri-plugin-fs` | 2.4.5 | Apache-2.0 OR MIT |
| `tauri-plugin-notification` | 2.3.3 | Apache-2.0 OR MIT |
| `tauri-plugin-opener` | 2.5.3 | Apache-2.0 OR MIT |
| `tauri-plugin-process` | 2.3.1 | Apache-2.0 OR MIT |
| `tauri-plugin-updater` | 2.10.0 | Apache-2.0 OR MIT |
| `tauri-runtime` | 2.10.0 | Apache-2.0 OR MIT |
| `tauri-runtime-wry` | 2.10.0 | Apache-2.0 OR MIT |
| `tauri-utils` | 2.8.2 | Apache-2.0 OR MIT |
| `tauri-winres` | 0.3.5 | MIT |
| `tauri-winrt-notification` | 0.7.3 | MIT OR Apache-2.0 |
| `tempfile` | 3.26.0 | MIT OR Apache-2.0 |
| `tendril` | 0.4.3 | MIT/Apache-2.0 |
| `termcolor` | 1.4.1 | Unlicense OR MIT |
| `thiserror` | 1.0.69 | MIT OR Apache-2.0 |
| `thiserror` | 2.0.18 | MIT OR Apache-2.0 |
| `thiserror-impl` | 1.0.69 | MIT OR Apache-2.0 |
| `thiserror-impl` | 2.0.18 | MIT OR Apache-2.0 |
| `thread_local` | 1.1.9 | MIT OR Apache-2.0 |
| `tiktoken-rs` | 0.11.0 | MIT |
| `time` | 0.3.47 | MIT OR Apache-2.0 |
| `time-core` | 0.1.8 | MIT OR Apache-2.0 |
| `time-macros` | 0.2.27 | MIT OR Apache-2.0 |
| `tinystr` | 0.8.2 | Unicode-3.0 |
| `tinyvec` | 1.11.0 | Zlib OR Apache-2.0 OR MIT |
| `tinyvec_macros` | 0.1.1 | MIT OR Apache-2.0 OR Zlib |
| `tokio` | 1.49.0 | MIT |
| `tokio-macros` | 2.6.0 | MIT |
| `tokio-rustls` | 0.26.4 | MIT OR Apache-2.0 |
| `tokio-stream` | 0.1.18 | MIT |
| `tokio-tungstenite` | 0.30.0 | MIT |
| `tokio-util` | 0.7.18 | MIT |
| `toml` | 0.8.2 | MIT OR Apache-2.0 |
| `toml` | 0.9.12+spec-1.1.0 | MIT OR Apache-2.0 |
| `toml_datetime` | 0.6.11 | MIT OR Apache-2.0 |
| `toml_datetime` | 0.7.5+spec-1.1.0 | MIT OR Apache-2.0 |
| `toml_edit` | 0.19.15 | MIT OR Apache-2.0 |
| `toml_edit` | 0.20.2 | MIT OR Apache-2.0 |
| `toml_edit` | 0.22.27 | MIT OR Apache-2.0 |
| `toml_edit` | 0.23.10+spec-1.0.0 | MIT OR Apache-2.0 |
| `toml_parser` | 1.0.9+spec-1.1.0 | MIT OR Apache-2.0 |
| `toml_write` | 0.1.2 | MIT OR Apache-2.0 |
| `toml_writer` | 1.0.6+spec-1.1.0 | MIT OR Apache-2.0 |
| `tower` | 0.5.3 | MIT |
| `tower-http` | 0.6.8 | MIT |
| `tower-layer` | 0.3.3 | MIT |
| `tower-service` | 0.3.3 | MIT |
| `tracing` | 0.1.44 | MIT |
| `tracing-attributes` | 0.1.31 | MIT |
| `tracing-core` | 0.1.36 | MIT |
| `tracing-log` | 0.2.0 | MIT |
| `tracing-subscriber` | 0.3.22 | MIT |
| `tracing-test` | 0.2.6 | MIT |
| `tracing-test-macro` | 0.2.6 | MIT |
| `tray-icon` | 0.21.3 | MIT OR Apache-2.0 |
| `try-lock` | 0.2.5 | MIT |
| `tungstenite` | 0.30.0 | MIT OR Apache-2.0 |
| `typeid` | 1.0.3 | MIT OR Apache-2.0 |
| `typenum` | 1.20.1 | MIT OR Apache-2.0 |
| `uds_windows` | 1.1.0 | MIT |
| `unic-char-property` | 0.9.0 | MIT/Apache-2.0 |
| `unic-char-range` | 0.9.0 | MIT/Apache-2.0 |
| `unic-common` | 0.9.0 | MIT/Apache-2.0 |
| `unic-ucd-ident` | 0.9.0 | MIT/Apache-2.0 |
| `unic-ucd-version` | 0.9.0 | MIT/Apache-2.0 |
| `unicode-ident` | 1.0.24 | (MIT OR Apache-2.0) AND Unicode-3.0 |
| `unicode-normalization` | 0.1.25 | MIT OR Apache-2.0 |
| `unicode-segmentation` | 1.12.0 | MIT OR Apache-2.0 |
| `unicode-width` | 0.1.14 | MIT OR Apache-2.0 |
| `unicode-xid` | 0.2.6 | MIT OR Apache-2.0 |
| `unsafe-libyaml` | 0.2.11 | MIT |
| `untrusted` | 0.9.0 | ISC |
| `url` | 2.5.8 | MIT OR Apache-2.0 |
| `urlencoding` | 2.1.3 | MIT |
| `urlpattern` | 0.3.0 | MIT |
| `utf-8` | 0.7.6 | MIT OR Apache-2.0 |
| `utf8_iter` | 1.0.4 | Apache-2.0 OR MIT |
| `utf8parse` | 0.2.2 | Apache-2.0 OR MIT |
| `uuid` | 1.21.0 | Apache-2.0 OR MIT |
| `uuid-simd` | 0.8.0 | MIT |
| `valuable` | 0.1.1 | MIT |
| `vcpkg` | 0.2.15 | MIT/Apache-2.0 |
| `version-compare` | 0.2.1 | MIT |
| `version_check` | 0.9.5 | MIT/Apache-2.0 |
| `vsimd` | 0.8.0 | MIT |
| `vswhom` | 0.1.0 | MIT |
| `vswhom-sys` | 0.1.3 | MIT |
| `walkdir` | 2.5.0 | Unlicense/MIT |
| `want` | 0.3.1 | MIT |
| `wasi` | 0.9.0+wasi-snapshot-preview1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| `wasi` | 0.11.1+wasi-snapshot-preview1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| `wasip2` | 1.0.2+wasi-0.2.9 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| `wasip3` | 0.4.0+wasi-0.3.0-rc-2026-01-06 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| `wasm-bindgen` | 0.2.113 | MIT OR Apache-2.0 |
| `wasm-bindgen-futures` | 0.4.63 | MIT OR Apache-2.0 |
| `wasm-bindgen-macro` | 0.2.113 | MIT OR Apache-2.0 |
| `wasm-bindgen-macro-support` | 0.2.113 | MIT OR Apache-2.0 |
| `wasm-bindgen-shared` | 0.2.113 | MIT OR Apache-2.0 |
| `wasm-encoder` | 0.244.0 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| `wasm-metadata` | 0.244.0 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| `wasm-streams` | 0.4.2 | MIT OR Apache-2.0 |
| `wasm-streams` | 0.5.0 | MIT OR Apache-2.0 |
| `wasmparser` | 0.244.0 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| `wayland-backend` | 0.3.16 | MIT |
| `wayland-client` | 0.31.15 | MIT |
| `wayland-protocols` | 0.32.13 | MIT |
| `wayland-scanner` | 0.31.11 | MIT |
| `wayland-sys` | 0.31.11 | MIT |
| `web-sys` | 0.3.90 | MIT OR Apache-2.0 |
| `web-time` | 1.1.0 | MIT OR Apache-2.0 |
| `webkit2gtk` | 2.0.2 | MIT |
| `webkit2gtk-sys` | 2.0.2 | MIT |
| `webpki-root-certs` | 1.0.6 | CDLA-Permissive-2.0 |
| `webpki-roots` | 0.26.11 | CDLA-Permissive-2.0 |
| `webpki-roots` | 1.0.7 | CDLA-Permissive-2.0 |
| `webview2-com` | 0.38.2 | MIT |
| `webview2-com-macros` | 0.8.1 | MIT |
| `webview2-com-sys` | 0.38.2 | MIT |
| `winapi` | 0.3.9 | MIT/Apache-2.0 |
| `winapi-i686-pc-windows-gnu` | 0.4.0 | MIT/Apache-2.0 |
| `winapi-util` | 0.1.11 | Unlicense OR MIT |
| `winapi-x86_64-pc-windows-gnu` | 0.4.0 | MIT/Apache-2.0 |
| `window-vibrancy` | 0.6.0 | Apache-2.0 OR MIT |
| `windows` | 0.36.1 | MIT OR Apache-2.0 |
| `windows` | 0.61.3 | MIT OR Apache-2.0 |
| `windows-collections` | 0.2.0 | MIT OR Apache-2.0 |
| `windows-core` | 0.61.2 | MIT OR Apache-2.0 |
| `windows-core` | 0.62.2 | MIT OR Apache-2.0 |
| `windows-future` | 0.2.1 | MIT OR Apache-2.0 |
| `windows-implement` | 0.60.2 | MIT OR Apache-2.0 |
| `windows-interface` | 0.59.3 | MIT OR Apache-2.0 |
| `windows-link` | 0.1.3 | MIT OR Apache-2.0 |
| `windows-link` | 0.2.1 | MIT OR Apache-2.0 |
| `windows-numerics` | 0.2.0 | MIT OR Apache-2.0 |
| `windows-result` | 0.3.4 | MIT OR Apache-2.0 |
| `windows-result` | 0.4.1 | MIT OR Apache-2.0 |
| `windows-strings` | 0.4.2 | MIT OR Apache-2.0 |
| `windows-strings` | 0.5.1 | MIT OR Apache-2.0 |
| `windows-sys` | 0.45.0 | MIT OR Apache-2.0 |
| `windows-sys` | 0.52.0 | MIT OR Apache-2.0 |
| `windows-sys` | 0.59.0 | MIT OR Apache-2.0 |
| `windows-sys` | 0.60.2 | MIT OR Apache-2.0 |
| `windows-sys` | 0.61.2 | MIT OR Apache-2.0 |
| `windows-targets` | 0.42.2 | MIT OR Apache-2.0 |
| `windows-targets` | 0.52.6 | MIT OR Apache-2.0 |
| `windows-targets` | 0.53.5 | MIT OR Apache-2.0 |
| `windows-threading` | 0.1.0 | MIT OR Apache-2.0 |
| `windows-version` | 0.1.7 | MIT OR Apache-2.0 |
| `windows_aarch64_gnullvm` | 0.42.2 | MIT OR Apache-2.0 |
| `windows_aarch64_gnullvm` | 0.52.6 | MIT OR Apache-2.0 |
| `windows_aarch64_gnullvm` | 0.53.1 | MIT OR Apache-2.0 |
| `windows_aarch64_msvc` | 0.36.1 | MIT OR Apache-2.0 |
| `windows_aarch64_msvc` | 0.42.2 | MIT OR Apache-2.0 |
| `windows_aarch64_msvc` | 0.52.6 | MIT OR Apache-2.0 |
| `windows_aarch64_msvc` | 0.53.1 | MIT OR Apache-2.0 |
| `windows_i686_gnu` | 0.36.1 | MIT OR Apache-2.0 |
| `windows_i686_gnu` | 0.42.2 | MIT OR Apache-2.0 |
| `windows_i686_gnu` | 0.52.6 | MIT OR Apache-2.0 |
| `windows_i686_gnu` | 0.53.1 | MIT OR Apache-2.0 |
| `windows_i686_gnullvm` | 0.52.6 | MIT OR Apache-2.0 |
| `windows_i686_gnullvm` | 0.53.1 | MIT OR Apache-2.0 |
| `windows_i686_msvc` | 0.36.1 | MIT OR Apache-2.0 |
| `windows_i686_msvc` | 0.42.2 | MIT OR Apache-2.0 |
| `windows_i686_msvc` | 0.52.6 | MIT OR Apache-2.0 |
| `windows_i686_msvc` | 0.53.1 | MIT OR Apache-2.0 |
| `windows_x86_64_gnu` | 0.36.1 | MIT OR Apache-2.0 |
| `windows_x86_64_gnu` | 0.42.2 | MIT OR Apache-2.0 |
| `windows_x86_64_gnu` | 0.52.6 | MIT OR Apache-2.0 |
| `windows_x86_64_gnu` | 0.53.1 | MIT OR Apache-2.0 |
| `windows_x86_64_gnullvm` | 0.42.2 | MIT OR Apache-2.0 |
| `windows_x86_64_gnullvm` | 0.52.6 | MIT OR Apache-2.0 |
| `windows_x86_64_gnullvm` | 0.53.1 | MIT OR Apache-2.0 |
| `windows_x86_64_msvc` | 0.36.1 | MIT OR Apache-2.0 |
| `windows_x86_64_msvc` | 0.42.2 | MIT OR Apache-2.0 |
| `windows_x86_64_msvc` | 0.52.6 | MIT OR Apache-2.0 |
| `windows_x86_64_msvc` | 0.53.1 | MIT OR Apache-2.0 |
| `winnow` | 0.5.40 | MIT |
| `winnow` | 0.7.14 | MIT |
| `winreg` | 0.55.0 | MIT |
| `wiremock` | 0.6.5 | MIT/Apache-2.0 |
| `wit-bindgen` | 0.51.0 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| `wit-bindgen-core` | 0.51.0 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| `wit-bindgen-rust` | 0.51.0 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| `wit-bindgen-rust-macro` | 0.51.0 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| `wit-component` | 0.244.0 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| `wit-parser` | 0.244.0 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| `writeable` | 0.6.2 | Unicode-3.0 |
| `wry` | 0.54.2 | Apache-2.0 OR MIT |
| `x11` | 2.21.0 | MIT |
| `x11-dl` | 2.21.0 | MIT |
| `xattr` | 1.6.1 | MIT OR Apache-2.0 |
| `xdg-home` | 1.3.0 | MIT |
| `yansi` | 1.0.1 | MIT OR Apache-2.0 |
| `yoke` | 0.8.1 | Unicode-3.0 |
| `yoke-derive` | 0.8.1 | Unicode-3.0 |
| `zbus` | 4.4.0 | MIT |
| `zbus` | 5.14.0 | MIT |
| `zbus_macros` | 4.4.0 | MIT |
| `zbus_macros` | 5.14.0 | MIT |
| `zbus_names` | 3.0.0 | MIT |
| `zbus_names` | 4.3.1 | MIT |
| `zerocopy` | 0.8.39 | BSD-2-Clause OR Apache-2.0 OR MIT |
| `zerocopy-derive` | 0.8.39 | BSD-2-Clause OR Apache-2.0 OR MIT |
| `zerofrom` | 0.1.6 | Unicode-3.0 |
| `zerofrom-derive` | 0.1.6 | Unicode-3.0 |
| `zeroize` | 1.8.2 | Apache-2.0 OR MIT |
| `zeroize_derive` | 1.4.3 | Apache-2.0 OR MIT |
| `zerotrie` | 0.2.3 | Unicode-3.0 |
| `zerovec` | 0.11.5 | Unicode-3.0 |
| `zerovec-derive` | 0.11.2 | Unicode-3.0 |
| `zip` | 4.6.1 | MIT |
| `zmij` | 1.0.21 | MIT |
| `zvariant` | 4.2.0 | MIT |
| `zvariant` | 5.10.0 | MIT |
| `zvariant_derive` | 4.2.0 | MIT |
| `zvariant_derive` | 5.10.0 | MIT |
| `zvariant_utils` | 2.1.0 | MIT |
| `zvariant_utils` | 3.3.0 | MIT |

## npm packages (565)

### Summary by declared licence

| Licence | Packages |
|---|---|
| `MIT` | 463 |
| `ISC` | 44 |
| `Apache-2.0 OR MIT` | 13 |
| `MPL-2.0` | 12 |
| `BSD-3-Clause` | 8 |
| `MIT OR Apache-2.0` | 6 |
| `Apache-2.0` | 6 |
| `BSD-2-Clause` | 3 |
| `MIT-0` | 2 |
| `Unlicense` | 1 |
| `Python-2.0` | 1 |
| MIT, declared only in a bundled `license` file (`khroma`) | 1 |
| `CC0-1.0` | 1 |
| `CC-BY-4.0` | 1 |
| `BlueOak-1.0.0` | 1 |
| `0BSD` | 1 |
| `(MPL-2.0 OR Apache-2.0)` | 1 |

### Full list

| Package | Version | Licence |
|---|---|---|
| `@antfu/install-pkg` | 1.1.0 | MIT |
| `@asamuzakjp/css-color` | 5.1.10 | MIT |
| `@asamuzakjp/dom-selector` | 7.0.9 | MIT |
| `@asamuzakjp/nwsapi` | 2.3.9 | MIT |
| `@babel/code-frame` | 7.29.7 | MIT |
| `@babel/compat-data` | 7.29.7 | MIT |
| `@babel/core` | 7.29.7 | MIT |
| `@babel/generator` | 7.29.8 | MIT |
| `@babel/helper-compilation-targets` | 7.29.7 | MIT |
| `@babel/helper-globals` | 7.29.7 | MIT |
| `@babel/helper-module-imports` | 7.29.7 | MIT |
| `@babel/helper-module-transforms` | 7.29.7 | MIT |
| `@babel/helper-plugin-utils` | 7.28.6 | MIT |
| `@babel/helper-string-parser` | 7.29.7 | MIT |
| `@babel/helper-validator-identifier` | 7.29.7 | MIT |
| `@babel/helper-validator-option` | 7.29.7 | MIT |
| `@babel/helpers` | 7.29.7 | MIT |
| `@babel/parser` | 7.29.8 | MIT |
| `@babel/plugin-transform-react-jsx-self` | 7.27.1 | MIT |
| `@babel/plugin-transform-react-jsx-source` | 7.27.1 | MIT |
| `@babel/template` | 7.29.7 | MIT |
| `@babel/traverse` | 7.29.8 | MIT |
| `@babel/types` | 7.29.8 | MIT |
| `@braintree/sanitize-url` | 7.1.2 | MIT |
| `@bramus/specificity` | 2.4.2 | MIT |
| `@chevrotain/types` | 11.1.2 | Apache-2.0 |
| `@csstools/color-helpers` | 6.0.2 | MIT-0 |
| `@csstools/css-calc` | 3.1.1 | MIT |
| `@csstools/css-color-parser` | 4.0.2 | MIT |
| `@csstools/css-parser-algorithms` | 4.0.0 | MIT |
| `@csstools/css-syntax-patches-for-csstree` | 1.1.2 | MIT-0 |
| `@csstools/css-tokenizer` | 4.0.0 | MIT |
| `@emnapi/core` | 1.8.1 | MIT |
| `@emnapi/runtime` | 1.8.1 | MIT |
| `@emnapi/wasi-threads` | 1.1.0 | MIT |
| `@emoji-mart/data` | 1.2.1 | MIT |
| `@emoji-mart/react` | 1.1.1 | MIT |
| `@esbuild/aix-ppc64` | 0.28.2 | MIT |
| `@esbuild/android-arm` | 0.28.2 | MIT |
| `@esbuild/android-arm64` | 0.28.2 | MIT |
| `@esbuild/android-x64` | 0.28.2 | MIT |
| `@esbuild/darwin-arm64` | 0.28.2 | MIT |
| `@esbuild/darwin-x64` | 0.28.2 | MIT |
| `@esbuild/freebsd-arm64` | 0.28.2 | MIT |
| `@esbuild/freebsd-x64` | 0.28.2 | MIT |
| `@esbuild/linux-arm` | 0.28.2 | MIT |
| `@esbuild/linux-arm64` | 0.28.2 | MIT |
| `@esbuild/linux-ia32` | 0.28.2 | MIT |
| `@esbuild/linux-loong64` | 0.28.2 | MIT |
| `@esbuild/linux-mips64el` | 0.28.2 | MIT |
| `@esbuild/linux-ppc64` | 0.28.2 | MIT |
| `@esbuild/linux-riscv64` | 0.28.2 | MIT |
| `@esbuild/linux-s390x` | 0.28.2 | MIT |
| `@esbuild/linux-x64` | 0.28.2 | MIT |
| `@esbuild/netbsd-arm64` | 0.28.2 | MIT |
| `@esbuild/netbsd-x64` | 0.28.2 | MIT |
| `@esbuild/openbsd-arm64` | 0.28.2 | MIT |
| `@esbuild/openbsd-x64` | 0.28.2 | MIT |
| `@esbuild/openharmony-arm64` | 0.28.2 | MIT |
| `@esbuild/sunos-x64` | 0.28.2 | MIT |
| `@esbuild/win32-arm64` | 0.28.2 | MIT |
| `@esbuild/win32-ia32` | 0.28.2 | MIT |
| `@esbuild/win32-x64` | 0.28.2 | MIT |
| `@exodus/bytes` | 1.15.0 | MIT |
| `@floating-ui/core` | 1.7.5 | MIT |
| `@floating-ui/dom` | 1.7.6 | MIT |
| `@floating-ui/utils` | 0.2.11 | MIT |
| `@iconify/types` | 2.0.0 | MIT |
| `@iconify/utils` | 3.1.0 | MIT |
| `@jridgewell/gen-mapping` | 0.3.13 | MIT |
| `@jridgewell/remapping` | 2.3.5 | MIT |
| `@jridgewell/resolve-uri` | 3.1.2 | MIT |
| `@jridgewell/sourcemap-codec` | 1.5.5 | MIT |
| `@jridgewell/trace-mapping` | 0.3.31 | MIT |
| `@mermaid-js/parser` | 1.2.0 | MIT |
| `@napi-rs/lzma-linux-x64-gnu` | 1.5.1 | MIT |
| `@napi-rs/wasm-runtime` | 1.1.1 | MIT |
| `@remirror/core-constants` | 3.0.0 | MIT |
| `@rolldown/pluginutils` | 1.0.0-beta.27 | MIT |
| `@rollup/rollup-android-arm-eabi` | 4.62.4 | MIT |
| `@rollup/rollup-android-arm64` | 4.62.4 | MIT |
| `@rollup/rollup-darwin-arm64` | 4.62.4 | MIT |
| `@rollup/rollup-darwin-x64` | 4.62.4 | MIT |
| `@rollup/rollup-freebsd-arm64` | 4.62.4 | MIT |
| `@rollup/rollup-freebsd-x64` | 4.62.4 | MIT |
| `@rollup/rollup-linux-arm-gnueabihf` | 4.62.4 | MIT |
| `@rollup/rollup-linux-arm-musleabihf` | 4.62.4 | MIT |
| `@rollup/rollup-linux-arm64-gnu` | 4.62.4 | MIT |
| `@rollup/rollup-linux-arm64-musl` | 4.62.4 | MIT |
| `@rollup/rollup-linux-loong64-gnu` | 4.62.4 | MIT |
| `@rollup/rollup-linux-loong64-musl` | 4.62.4 | MIT |
| `@rollup/rollup-linux-ppc64-gnu` | 4.62.4 | MIT |
| `@rollup/rollup-linux-ppc64-musl` | 4.62.4 | MIT |
| `@rollup/rollup-linux-riscv64-gnu` | 4.62.4 | MIT |
| `@rollup/rollup-linux-riscv64-musl` | 4.62.4 | MIT |
| `@rollup/rollup-linux-s390x-gnu` | 4.62.4 | MIT |
| `@rollup/rollup-linux-x64-gnu` | 4.62.4 | MIT |
| `@rollup/rollup-linux-x64-musl` | 4.62.4 | MIT |
| `@rollup/rollup-openbsd-x64` | 4.62.4 | MIT |
| `@rollup/rollup-openharmony-arm64` | 4.62.4 | MIT |
| `@rollup/rollup-win32-arm64-msvc` | 4.62.4 | MIT |
| `@rollup/rollup-win32-ia32-msvc` | 4.62.4 | MIT |
| `@rollup/rollup-win32-x64-gnu` | 4.62.4 | MIT |
| `@rollup/rollup-win32-x64-msvc` | 4.62.4 | MIT |
| `@standard-schema/spec` | 1.1.0 | MIT |
| `@tailwindcss/node` | 4.2.0 | MIT |
| `@tailwindcss/oxide` | 4.2.0 | MIT |
| `@tailwindcss/oxide-android-arm64` | 4.2.0 | MIT |
| `@tailwindcss/oxide-darwin-arm64` | 4.2.0 | MIT |
| `@tailwindcss/oxide-darwin-x64` | 4.2.0 | MIT |
| `@tailwindcss/oxide-freebsd-x64` | 4.2.0 | MIT |
| `@tailwindcss/oxide-linux-arm-gnueabihf` | 4.2.0 | MIT |
| `@tailwindcss/oxide-linux-arm64-gnu` | 4.2.0 | MIT |
| `@tailwindcss/oxide-linux-arm64-musl` | 4.2.0 | MIT |
| `@tailwindcss/oxide-linux-x64-gnu` | 4.2.0 | MIT |
| `@tailwindcss/oxide-linux-x64-musl` | 4.2.0 | MIT |
| `@tailwindcss/oxide-wasm32-wasi` | 4.2.0 | MIT |
| `@tailwindcss/oxide-win32-arm64-msvc` | 4.2.0 | MIT |
| `@tailwindcss/oxide-win32-x64-msvc` | 4.2.0 | MIT |
| `@tailwindcss/typography` | 0.5.19 | MIT |
| `@tailwindcss/vite` | 4.2.0 | MIT |
| `@tanstack/react-virtual` | 3.13.23 | MIT |
| `@tanstack/virtual-core` | 3.13.23 | MIT |
| `@tauri-apps/api` | 2.10.1 | Apache-2.0 OR MIT |
| `@tauri-apps/cli` | 2.10.0 | Apache-2.0 OR MIT |
| `@tauri-apps/cli-darwin-arm64` | 2.10.0 | Apache-2.0 OR MIT |
| `@tauri-apps/cli-darwin-x64` | 2.10.0 | Apache-2.0 OR MIT |
| `@tauri-apps/cli-linux-arm-gnueabihf` | 2.10.0 | Apache-2.0 OR MIT |
| `@tauri-apps/cli-linux-arm64-gnu` | 2.10.0 | Apache-2.0 OR MIT |
| `@tauri-apps/cli-linux-arm64-musl` | 2.10.0 | Apache-2.0 OR MIT |
| `@tauri-apps/cli-linux-riscv64-gnu` | 2.10.0 | Apache-2.0 OR MIT |
| `@tauri-apps/cli-linux-x64-gnu` | 2.10.0 | Apache-2.0 OR MIT |
| `@tauri-apps/cli-linux-x64-musl` | 2.10.0 | Apache-2.0 OR MIT |
| `@tauri-apps/cli-win32-arm64-msvc` | 2.10.0 | Apache-2.0 OR MIT |
| `@tauri-apps/cli-win32-ia32-msvc` | 2.10.0 | Apache-2.0 OR MIT |
| `@tauri-apps/cli-win32-x64-msvc` | 2.10.0 | Apache-2.0 OR MIT |
| `@tauri-apps/plugin-dialog` | 2.6.0 | MIT OR Apache-2.0 |
| `@tauri-apps/plugin-fs` | 2.4.5 | MIT OR Apache-2.0 |
| `@tauri-apps/plugin-notification` | 2.3.3 | MIT OR Apache-2.0 |
| `@tauri-apps/plugin-opener` | 2.5.3 | MIT OR Apache-2.0 |
| `@tauri-apps/plugin-process` | 2.3.1 | MIT OR Apache-2.0 |
| `@tauri-apps/plugin-updater` | 2.10.0 | MIT OR Apache-2.0 |
| `@tiptap/core` | 3.20.1 | MIT |
| `@tiptap/extension-blockquote` | 3.20.1 | MIT |
| `@tiptap/extension-bold` | 3.20.1 | MIT |
| `@tiptap/extension-bubble-menu` | 3.20.1 | MIT |
| `@tiptap/extension-bullet-list` | 3.20.1 | MIT |
| `@tiptap/extension-code` | 3.20.1 | MIT |
| `@tiptap/extension-code-block` | 3.20.1 | MIT |
| `@tiptap/extension-document` | 3.20.1 | MIT |
| `@tiptap/extension-dropcursor` | 3.20.1 | MIT |
| `@tiptap/extension-floating-menu` | 3.20.1 | MIT |
| `@tiptap/extension-gapcursor` | 3.20.1 | MIT |
| `@tiptap/extension-hard-break` | 3.20.1 | MIT |
| `@tiptap/extension-heading` | 3.20.1 | MIT |
| `@tiptap/extension-horizontal-rule` | 3.20.1 | MIT |
| `@tiptap/extension-italic` | 3.20.1 | MIT |
| `@tiptap/extension-link` | 3.20.1 | MIT |
| `@tiptap/extension-list` | 3.20.1 | MIT |
| `@tiptap/extension-list-item` | 3.20.1 | MIT |
| `@tiptap/extension-list-keymap` | 3.20.1 | MIT |
| `@tiptap/extension-mention` | 3.20.1 | MIT |
| `@tiptap/extension-ordered-list` | 3.20.1 | MIT |
| `@tiptap/extension-paragraph` | 3.20.1 | MIT |
| `@tiptap/extension-placeholder` | 3.20.1 | MIT |
| `@tiptap/extension-strike` | 3.20.1 | MIT |
| `@tiptap/extension-text` | 3.20.1 | MIT |
| `@tiptap/extension-underline` | 3.20.1 | MIT |
| `@tiptap/extensions` | 3.20.1 | MIT |
| `@tiptap/pm` | 3.20.1 | MIT |
| `@tiptap/react` | 3.20.1 | MIT |
| `@tiptap/starter-kit` | 3.20.1 | MIT |
| `@tiptap/suggestion` | 3.20.1 | MIT |
| `@tybys/wasm-util` | 0.10.1 | MIT |
| `@types/babel__core` | 7.20.5 | MIT |
| `@types/babel__generator` | 7.27.0 | MIT |
| `@types/babel__template` | 7.4.4 | MIT |
| `@types/babel__traverse` | 7.28.0 | MIT |
| `@types/chai` | 5.2.3 | MIT |
| `@types/d3` | 7.4.3 | MIT |
| `@types/d3-array` | 3.2.2 | MIT |
| `@types/d3-axis` | 3.0.6 | MIT |
| `@types/d3-brush` | 3.0.6 | MIT |
| `@types/d3-chord` | 3.0.6 | MIT |
| `@types/d3-color` | 3.1.3 | MIT |
| `@types/d3-contour` | 3.0.6 | MIT |
| `@types/d3-delaunay` | 6.0.4 | MIT |
| `@types/d3-dispatch` | 3.0.7 | MIT |
| `@types/d3-drag` | 3.0.7 | MIT |
| `@types/d3-dsv` | 3.0.7 | MIT |
| `@types/d3-ease` | 3.0.2 | MIT |
| `@types/d3-fetch` | 3.0.7 | MIT |
| `@types/d3-force` | 3.0.10 | MIT |
| `@types/d3-format` | 3.0.4 | MIT |
| `@types/d3-geo` | 3.1.0 | MIT |
| `@types/d3-hierarchy` | 3.1.7 | MIT |
| `@types/d3-interpolate` | 3.0.4 | MIT |
| `@types/d3-path` | 3.1.1 | MIT |
| `@types/d3-polygon` | 3.0.2 | MIT |
| `@types/d3-quadtree` | 3.0.6 | MIT |
| `@types/d3-random` | 3.0.3 | MIT |
| `@types/d3-scale` | 4.0.9 | MIT |
| `@types/d3-scale-chromatic` | 3.1.0 | MIT |
| `@types/d3-selection` | 3.0.11 | MIT |
| `@types/d3-shape` | 3.1.8 | MIT |
| `@types/d3-time` | 3.0.4 | MIT |
| `@types/d3-time-format` | 4.0.3 | MIT |
| `@types/d3-timer` | 3.0.2 | MIT |
| `@types/d3-transition` | 3.0.9 | MIT |
| `@types/d3-zoom` | 3.0.8 | MIT |
| `@types/debug` | 4.1.12 | MIT |
| `@types/deep-eql` | 4.0.2 | MIT |
| `@types/estree` | 1.0.9 | MIT |
| `@types/estree-jsx` | 1.0.5 | MIT |
| `@types/geojson` | 7946.0.16 | MIT |
| `@types/hast` | 3.0.4 | MIT |
| `@types/katex` | 0.16.8 | MIT |
| `@types/linkify-it` | 5.0.0 | MIT |
| `@types/markdown-it` | 14.1.2 | MIT |
| `@types/mdast` | 4.0.4 | MIT |
| `@types/mdurl` | 2.0.0 | MIT |
| `@types/ms` | 2.1.0 | MIT |
| `@types/react` | 19.2.14 | MIT |
| `@types/react-dom` | 19.2.3 | MIT |
| `@types/trusted-types` | 2.0.7 | MIT |
| `@types/unist` | 2.0.11 | MIT |
| `@types/unist` | 3.0.3 | MIT |
| `@types/use-sync-external-store` | 0.0.6 | MIT |
| `@ungap/structured-clone` | 1.3.0 | ISC |
| `@upsetjs/venn.js` | 2.0.0 | MIT |
| `@vitejs/plugin-react` | 4.7.0 | MIT |
| `@vitest/expect` | 4.1.4 | MIT |
| `@vitest/mocker` | 4.1.4 | MIT |
| `@vitest/pretty-format` | 4.1.4 | MIT |
| `@vitest/runner` | 4.1.4 | MIT |
| `@vitest/snapshot` | 4.1.4 | MIT |
| `@vitest/spy` | 4.1.4 | MIT |
| `@vitest/utils` | 4.1.4 | MIT |
| `acorn` | 8.16.0 | MIT |
| `argparse` | 2.0.1 | Python-2.0 |
| `assertion-error` | 2.0.1 | MIT |
| `bail` | 2.0.2 | MIT |
| `baseline-browser-mapping` | 2.11.14 | Apache-2.0 |
| `bidi-js` | 1.0.3 | MIT |
| `browserslist` | 4.28.8 | MIT |
| `caniuse-lite` | 1.0.30001809 | CC-BY-4.0 |
| `ccount` | 2.0.1 | MIT |
| `chai` | 6.2.2 | MIT |
| `character-entities` | 2.0.2 | MIT |
| `character-entities-html4` | 2.1.0 | MIT |
| `character-entities-legacy` | 3.0.0 | MIT |
| `character-reference-invalid` | 2.0.1 | MIT |
| `comma-separated-tokens` | 2.0.3 | MIT |
| `commander` | 7.2.0 | MIT |
| `commander` | 8.3.0 | MIT |
| `confbox` | 0.1.8 | MIT |
| `convert-source-map` | 2.0.0 | MIT |
| `cookie` | 1.1.1 | MIT |
| `cose-base` | 1.0.3 | MIT |
| `cose-base` | 2.2.0 | MIT |
| `crelt` | 1.0.6 | MIT |
| `cron-parser` | 5.6.1 | MIT |
| `cronstrue` | 3.14.0 | MIT |
| `css-tree` | 3.2.1 | MIT |
| `cssesc` | 3.0.0 | MIT |
| `csstype` | 3.2.3 | MIT |
| `cytoscape` | 3.34.1 | MIT |
| `cytoscape-cose-bilkent` | 4.1.0 | MIT |
| `cytoscape-fcose` | 2.2.0 | MIT |
| `d3` | 7.9.0 | ISC |
| `d3-array` | 2.12.1 | BSD-3-Clause |
| `d3-array` | 3.2.4 | ISC |
| `d3-axis` | 3.0.0 | ISC |
| `d3-brush` | 3.0.0 | ISC |
| `d3-chord` | 3.0.1 | ISC |
| `d3-color` | 3.1.0 | ISC |
| `d3-contour` | 4.0.2 | ISC |
| `d3-delaunay` | 6.0.4 | ISC |
| `d3-dispatch` | 3.0.1 | ISC |
| `d3-drag` | 3.0.0 | ISC |
| `d3-dsv` | 3.0.1 | ISC |
| `d3-ease` | 3.0.1 | BSD-3-Clause |
| `d3-fetch` | 3.0.1 | ISC |
| `d3-force` | 3.0.0 | ISC |
| `d3-format` | 3.1.2 | ISC |
| `d3-geo` | 3.1.1 | ISC |
| `d3-hierarchy` | 3.1.2 | ISC |
| `d3-interpolate` | 3.0.1 | ISC |
| `d3-path` | 1.0.9 | BSD-3-Clause |
| `d3-path` | 3.1.0 | ISC |
| `d3-polygon` | 3.0.1 | ISC |
| `d3-quadtree` | 3.0.1 | ISC |
| `d3-random` | 3.0.1 | ISC |
| `d3-sankey` | 0.12.3 | BSD-3-Clause |
| `d3-scale` | 4.0.2 | ISC |
| `d3-scale-chromatic` | 3.1.0 | ISC |
| `d3-selection` | 3.0.0 | ISC |
| `d3-shape` | 1.3.7 | BSD-3-Clause |
| `d3-shape` | 3.2.0 | ISC |
| `d3-time` | 3.1.0 | ISC |
| `d3-time-format` | 4.1.0 | ISC |
| `d3-timer` | 3.0.1 | ISC |
| `d3-transition` | 3.0.1 | ISC |
| `d3-zoom` | 3.0.0 | ISC |
| `dagre-d3-es` | 7.0.14 | MIT |
| `data-urls` | 7.0.0 | MIT |
| `dayjs` | 1.11.22 | MIT |
| `debug` | 4.4.3 | MIT |
| `decimal.js` | 10.6.0 | MIT |
| `decode-named-character-reference` | 1.3.0 | MIT |
| `delaunator` | 5.1.0 | ISC |
| `dequal` | 2.0.3 | MIT |
| `detect-libc` | 2.1.2 | Apache-2.0 |
| `devlop` | 1.1.0 | MIT |
| `dompurify` | 3.4.13 | (MPL-2.0 OR Apache-2.0) |
| `electron-to-chromium` | 1.5.407 | ISC |
| `emoji-mart` | 5.6.0 | MIT |
| `enhanced-resolve` | 5.19.0 | MIT |
| `entities` | 4.5.0 | BSD-2-Clause |
| `entities` | 6.0.1 | BSD-2-Clause |
| `es-module-lexer` | 2.0.0 | MIT |
| `es-toolkit` | 1.50.0 | MIT |
| `esbuild` | 0.28.2 | MIT |
| `escalade` | 3.2.0 | MIT |
| `escape-string-regexp` | 4.0.0 | MIT |
| `escape-string-regexp` | 5.0.0 | MIT |
| `estree-util-is-identifier-name` | 3.0.0 | MIT |
| `estree-walker` | 3.0.3 | MIT |
| `expect-type` | 1.3.0 | Apache-2.0 |
| `extend` | 3.0.2 | MIT |
| `fast-equals` | 5.4.0 | MIT |
| `fdir` | 6.5.0 | MIT |
| `framer-motion` | 12.34.3 | MIT |
| `fsevents` | 2.3.3 | MIT |
| `gensync` | 1.0.0-beta.2 | MIT |
| `graceful-fs` | 4.2.11 | ISC |
| `hachure-fill` | 0.5.2 | MIT |
| `hast-util-from-dom` | 5.0.1 | ISC |
| `hast-util-from-html` | 2.0.3 | MIT |
| `hast-util-from-html-isomorphic` | 2.0.0 | MIT |
| `hast-util-from-parse5` | 8.0.3 | MIT |
| `hast-util-is-element` | 3.0.0 | MIT |
| `hast-util-parse-selector` | 4.0.0 | MIT |
| `hast-util-to-jsx-runtime` | 2.3.6 | MIT |
| `hast-util-to-text` | 4.0.2 | MIT |
| `hast-util-whitespace` | 3.0.0 | MIT |
| `hastscript` | 9.0.1 | MIT |
| `html-encoding-sniffer` | 6.0.0 | MIT |
| `html-to-image` | 1.11.13 | MIT |
| `html-url-attributes` | 3.0.1 | MIT |
| `iconv-lite` | 0.6.3 | MIT |
| `inline-style-parser` | 0.2.7 | MIT |
| `internmap` | 1.0.1 | ISC |
| `internmap` | 2.0.3 | ISC |
| `is-alphabetical` | 2.0.1 | MIT |
| `is-alphanumerical` | 2.0.1 | MIT |
| `is-decimal` | 2.0.1 | MIT |
| `is-hexadecimal` | 2.0.1 | MIT |
| `is-plain-obj` | 4.1.0 | MIT |
| `is-potential-custom-element-name` | 1.0.1 | MIT |
| `jiti` | 2.6.1 | MIT |
| `js-tokens` | 4.0.0 | MIT |
| `jsdom` | 29.0.2 | MIT |
| `jsesc` | 3.1.0 | MIT |
| `json5` | 2.2.3 | MIT |
| `katex` | 0.16.47 | MIT |
| `khroma` | 2.1.0 | MIT (from bundled `license` file; no `license` field declared) |
| `layout-base` | 1.0.2 | MIT |
| `layout-base` | 2.0.1 | MIT |
| `lightningcss` | 1.31.1 | MPL-2.0 |
| `lightningcss-android-arm64` | 1.31.1 | MPL-2.0 |
| `lightningcss-darwin-arm64` | 1.31.1 | MPL-2.0 |
| `lightningcss-darwin-x64` | 1.31.1 | MPL-2.0 |
| `lightningcss-freebsd-x64` | 1.31.1 | MPL-2.0 |
| `lightningcss-linux-arm-gnueabihf` | 1.31.1 | MPL-2.0 |
| `lightningcss-linux-arm64-gnu` | 1.31.1 | MPL-2.0 |
| `lightningcss-linux-arm64-musl` | 1.31.1 | MPL-2.0 |
| `lightningcss-linux-x64-gnu` | 1.31.1 | MPL-2.0 |
| `lightningcss-linux-x64-musl` | 1.31.1 | MPL-2.0 |
| `lightningcss-win32-arm64-msvc` | 1.31.1 | MPL-2.0 |
| `lightningcss-win32-x64-msvc` | 1.31.1 | MPL-2.0 |
| `linkify-it` | 5.0.2 | MIT |
| `linkifyjs` | 4.3.2 | MIT |
| `lodash-es` | 4.18.1 | MIT |
| `longest-streak` | 3.1.0 | MIT |
| `lru-cache` | 5.1.1 | ISC |
| `lru-cache` | 11.3.3 | BlueOak-1.0.0 |
| `lucide-react` | 0.575.0 | ISC |
| `luxon` | 3.7.2 | MIT |
| `magic-string` | 0.30.21 | MIT |
| `markdown-it` | 14.3.0 | MIT |
| `markdown-table` | 3.0.4 | MIT |
| `marked` | 16.4.2 | MIT |
| `mdast-util-find-and-replace` | 3.0.2 | MIT |
| `mdast-util-from-markdown` | 2.0.3 | MIT |
| `mdast-util-gfm` | 3.1.0 | MIT |
| `mdast-util-gfm-autolink-literal` | 2.0.1 | MIT |
| `mdast-util-gfm-footnote` | 2.1.0 | MIT |
| `mdast-util-gfm-strikethrough` | 2.0.0 | MIT |
| `mdast-util-gfm-table` | 2.0.0 | MIT |
| `mdast-util-gfm-task-list-item` | 2.0.0 | MIT |
| `mdast-util-math` | 3.0.0 | MIT |
| `mdast-util-mdx-expression` | 2.0.1 | MIT |
| `mdast-util-mdx-jsx` | 3.2.0 | MIT |
| `mdast-util-mdxjs-esm` | 2.0.1 | MIT |
| `mdast-util-phrasing` | 4.1.0 | MIT |
| `mdast-util-to-hast` | 13.2.1 | MIT |
| `mdast-util-to-markdown` | 2.1.2 | MIT |
| `mdast-util-to-string` | 4.0.0 | MIT |
| `mdn-data` | 2.27.1 | CC0-1.0 |
| `mdurl` | 2.0.0 | MIT |
| `mermaid` | 11.16.1 | MIT |
| `micromark` | 4.0.2 | MIT |
| `micromark-core-commonmark` | 2.0.3 | MIT |
| `micromark-extension-gfm` | 3.0.0 | MIT |
| `micromark-extension-gfm-autolink-literal` | 2.1.0 | MIT |
| `micromark-extension-gfm-footnote` | 2.1.0 | MIT |
| `micromark-extension-gfm-strikethrough` | 2.1.0 | MIT |
| `micromark-extension-gfm-table` | 2.1.1 | MIT |
| `micromark-extension-gfm-tagfilter` | 2.0.0 | MIT |
| `micromark-extension-gfm-task-list-item` | 2.1.0 | MIT |
| `micromark-extension-math` | 3.1.0 | MIT |
| `micromark-factory-destination` | 2.0.1 | MIT |
| `micromark-factory-label` | 2.0.1 | MIT |
| `micromark-factory-space` | 2.0.1 | MIT |
| `micromark-factory-title` | 2.0.1 | MIT |
| `micromark-factory-whitespace` | 2.0.1 | MIT |
| `micromark-util-character` | 2.1.1 | MIT |
| `micromark-util-chunked` | 2.0.1 | MIT |
| `micromark-util-classify-character` | 2.0.1 | MIT |
| `micromark-util-combine-extensions` | 2.0.1 | MIT |
| `micromark-util-decode-numeric-character-reference` | 2.0.2 | MIT |
| `micromark-util-decode-string` | 2.0.1 | MIT |
| `micromark-util-encode` | 2.0.1 | MIT |
| `micromark-util-html-tag-name` | 2.0.1 | MIT |
| `micromark-util-normalize-identifier` | 2.0.1 | MIT |
| `micromark-util-resolve-all` | 2.0.1 | MIT |
| `micromark-util-sanitize-uri` | 2.0.1 | MIT |
| `micromark-util-subtokenize` | 2.1.0 | MIT |
| `micromark-util-symbol` | 2.0.1 | MIT |
| `micromark-util-types` | 2.0.2 | MIT |
| `mlly` | 1.8.0 | MIT |
| `motion-dom` | 12.34.3 | MIT |
| `motion-utils` | 12.29.2 | MIT |
| `ms` | 2.1.3 | MIT |
| `nanoid` | 3.3.18 | MIT |
| `node-releases` | 2.0.53 | MIT |
| `obug` | 2.1.1 | MIT |
| `orderedmap` | 2.1.1 | MIT |
| `package-manager-detector` | 1.6.0 | MIT |
| `parse-entities` | 4.0.2 | MIT |
| `parse5` | 7.3.0 | MIT |
| `parse5` | 8.0.0 | MIT |
| `path-data-parser` | 0.1.0 | MIT |
| `pathe` | 2.0.3 | MIT |
| `picocolors` | 1.1.1 | ISC |
| `picomatch` | 4.0.5 | MIT |
| `pkg-types` | 1.3.1 | MIT |
| `points-on-curve` | 0.2.0 | MIT |
| `points-on-path` | 0.2.1 | MIT |
| `postcss` | 8.5.26 | MIT |
| `postcss-selector-parser` | 6.0.10 | MIT |
| `property-information` | 7.1.0 | MIT |
| `prosemirror-changeset` | 2.4.0 | MIT |
| `prosemirror-collab` | 1.3.1 | MIT |
| `prosemirror-commands` | 1.7.1 | MIT |
| `prosemirror-dropcursor` | 1.8.2 | MIT |
| `prosemirror-gapcursor` | 1.4.1 | MIT |
| `prosemirror-history` | 1.5.0 | MIT |
| `prosemirror-inputrules` | 1.5.1 | MIT |
| `prosemirror-keymap` | 1.2.3 | MIT |
| `prosemirror-markdown` | 1.13.4 | MIT |
| `prosemirror-menu` | 1.3.0 | MIT |
| `prosemirror-model` | 1.25.4 | MIT |
| `prosemirror-schema-basic` | 1.2.4 | MIT |
| `prosemirror-schema-list` | 1.5.1 | MIT |
| `prosemirror-state` | 1.4.4 | MIT |
| `prosemirror-tables` | 1.8.5 | MIT |
| `prosemirror-trailing-node` | 3.0.0 | MIT |
| `prosemirror-transform` | 1.11.0 | MIT |
| `prosemirror-view` | 1.41.6 | MIT |
| `punycode` | 2.3.1 | MIT |
| `punycode.js` | 2.3.1 | MIT |
| `react` | 19.2.4 | MIT |
| `react-dom` | 19.2.4 | MIT |
| `react-icons` | 5.5.0 | MIT |
| `react-markdown` | 10.1.0 | MIT |
| `react-refresh` | 0.17.0 | MIT |
| `react-router` | 7.18.2 | MIT |
| `react-router-dom` | 7.18.2 | MIT |
| `rehype-katex` | 7.0.1 | MIT |
| `remark-gfm` | 4.0.1 | MIT |
| `remark-math` | 6.0.0 | MIT |
| `remark-parse` | 11.0.0 | MIT |
| `remark-rehype` | 11.1.2 | MIT |
| `remark-stringify` | 11.0.0 | MIT |
| `require-from-string` | 2.0.2 | MIT |
| `robust-predicates` | 3.0.3 | Unlicense |
| `rollup` | 4.62.4 | MIT |
| `rope-sequence` | 1.3.4 | MIT |
| `roughjs` | 4.6.6 | MIT |
| `rw` | 1.3.3 | BSD-3-Clause |
| `safer-buffer` | 2.1.2 | MIT |
| `saxes` | 6.0.0 | ISC |
| `scheduler` | 0.27.0 | MIT |
| `semver` | 6.3.1 | ISC |
| `set-cookie-parser` | 2.7.2 | MIT |
| `siginfo` | 2.0.0 | ISC |
| `source-map-js` | 1.2.1 | BSD-3-Clause |
| `space-separated-tokens` | 2.0.2 | MIT |
| `stackback` | 0.0.2 | MIT |
| `std-env` | 4.0.0 | MIT |
| `stringify-entities` | 4.0.4 | MIT |
| `style-to-js` | 1.1.21 | MIT |
| `style-to-object` | 1.0.14 | MIT |
| `stylis` | 4.3.6 | MIT |
| `symbol-tree` | 3.2.4 | MIT |
| `tailwind-merge` | 3.5.0 | MIT |
| `tailwindcss` | 4.2.0 | MIT |
| `tapable` | 2.3.0 | MIT |
| `tinybench` | 2.9.0 | MIT |
| `tinyexec` | 1.0.2 | MIT |
| `tinyglobby` | 0.2.15 | MIT |
| `tinyrainbow` | 3.1.0 | MIT |
| `tldts` | 7.0.28 | MIT |
| `tldts-core` | 7.0.28 | MIT |
| `tough-cookie` | 6.0.1 | BSD-3-Clause |
| `tr46` | 6.0.0 | MIT |
| `trim-lines` | 3.0.1 | MIT |
| `trough` | 2.2.0 | MIT |
| `ts-dedent` | 2.2.0 | MIT |
| `tslib` | 2.8.1 | 0BSD |
| `typescript` | 5.8.3 | Apache-2.0 |
| `uc.micro` | 2.1.0 | MIT |
| `ufo` | 1.6.3 | MIT |
| `undici` | 7.29.0 | MIT |
| `unified` | 11.0.5 | MIT |
| `unist-util-find-after` | 5.0.0 | MIT |
| `unist-util-is` | 6.0.1 | MIT |
| `unist-util-position` | 5.0.0 | MIT |
| `unist-util-remove-position` | 5.0.0 | MIT |
| `unist-util-stringify-position` | 4.0.0 | MIT |
| `unist-util-visit` | 5.1.0 | MIT |
| `unist-util-visit-parents` | 6.0.2 | MIT |
| `update-browserslist-db` | 1.3.1 | MIT |
| `use-sync-external-store` | 1.6.0 | MIT |
| `util-deprecate` | 1.0.2 | MIT |
| `uuid` | 14.0.1 | MIT |
| `vfile` | 6.0.3 | MIT |
| `vfile-location` | 5.0.3 | MIT |
| `vfile-message` | 4.0.3 | MIT |
| `vite` | 7.3.6 | MIT |
| `vitest` | 4.1.4 | MIT |
| `w3c-keyname` | 2.2.8 | MIT |
| `w3c-xmlserializer` | 5.0.0 | MIT |
| `web-namespaces` | 2.0.1 | MIT |
| `webidl-conversions` | 8.0.1 | BSD-2-Clause |
| `whatwg-mimetype` | 5.0.0 | MIT |
| `whatwg-url` | 16.0.1 | MIT |
| `why-is-node-running` | 2.3.0 | MIT |
| `xml-name-validator` | 5.0.0 | Apache-2.0 |
| `xmlchars` | 2.2.0 | MIT |
| `yallist` | 3.1.1 | ISC |
| `zustand` | 5.0.11 | MIT |
| `zwitch` | 2.0.4 | MIT |
