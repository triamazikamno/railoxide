# Asset sources

- Packaging resolves `gpui-kit-assets` 0.6.0 through `cargo metadata --locked --filter-platform wasm32-unknown-unknown`. Every `assets/icons/*.svg` from that resolved package is copied unchanged to the same package path. Generated `assets/COMPONENT-SOURCES.md` records each output checksum. The Kit package declares Apache-2.0; its Lucide icons use ISC.
- [Kit Apache license](https://github.com/longbridge/gpui-kit/blob/6d802393b4f247c11777aa27c7a46504a1b46d30/LICENSE-APACHE) is copied to `licenses/gpui-kit-APACHE-2.0.txt`. That revision is the published crate's recorded source revision; its source metadata reports a dirty checkout, so packaged icon bytes are taken from the checksummed crates.io package, not reconstructed from Git.
- [Lucide ISC license](https://github.com/lucide-icons/lucide/blob/0.468.0/LICENSE) is copied to `licenses/lucide-ISC.txt` and includes the Feather-derived icon notice.
- Font bytes, exact upstream revisions, checksums, and licenses are recorded in [fonts/SOURCES.md](fonts/SOURCES.md). Inter's actual internal family name is `Inter Variable`; the browser theme uses that name and `JetBrains Mono`. Shared/native font constants are unchanged.
