# Third-party licences

Lemon Mix itself is licensed under the MIT licence in [`../LICENSE`](../LICENSE).
`LemonMix.exe` also contains, or works with, the components below.

| File | Covers |
|---|---|
| [`Rust-crates.txt`](Rust-crates.txt) | The Rust crates compiled into the executable (egui and eframe, tray-icon, windows, serde_json, toml_edit, clap and their dependencies), with each crate's licence and copyright notice, and the licences of the fonts egui builds into the window (Ubuntu Font Licence, SIL Open Font Licence, Hack, emoji icon font). |
| [`Rust-standard-library.html`](Rust-standard-library.html) | The Rust standard library, which every Rust program contains (MIT or Apache-2.0), and the crates it is built from. |
| [`GCC-runtime-library-exception.txt`](GCC-runtime-library-exception.txt) | Small parts of the GCC and MinGW-w64 runtime that the GNU toolchain links into the executable. The GCC Runtime Library Exception places no conditions on programs built this way; it is included for reference. MinGW-w64 sources: https://www.mingw-w64.org/ |
| [`usbip-win2_BSD-2-Clause.txt`](usbip-win2_BSD-2-Clause.txt) | usbip-win2, the separately installed USB/IP driver the application uses. It is not part of the executable. |
| [`notices/`](notices) | Licence notices copied from the repositories of crates that are published without one; `Rust-crates.txt` includes them. |

## Updating `Rust-crates.txt`

After adding, removing or upgrading a dependency, regenerate the file from the repository root:

```
powershell -ExecutionPolicy Bypass -File scripts\update-licenses.ps1
```

The script asks Cargo which crates are compiled into `LemonMix.exe` (`cargo tree -p lemon-mix -e normal,no-proc-macro`), then writes each crate's licence, source and notice file, taken from the crate's own files in the Cargo registry, plus any `NOTICE` file. Where a crate offers MIT, its MIT notice is used.

Some crates (the egui family among them) are published without a licence file. For those the script uses the notice in `notices/`, listed in its `$fallbacks` table. If it warns that a crate has no licence file, copy the crate's licence from its repository into `notices/`, add the crate to `$fallbacks`, and run it again.

Crates used only while building (procedural macros such as `serde_derive`, and the build script's dependencies) do not end up in the executable, so they are left out, as are the workspace's own crates and the `vendor/webbrowser` stand-in, which are this repository's code.
