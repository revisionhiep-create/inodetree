# inodetree

A fast, open-source disk space analyzer for Windows and Linux, written in Rust. It is inspired by
WizTree and is not affiliated with it.

This fork adds native Windows support with a single portable, self-contained `.exe` (based on
[Juanmidev1/inodetree](https://github.com/Juanmidev1/inodetree)).

inodetree scans a directory tree with batched `FileIdBothDirectoryInfo` queries on Windows
(or a parallel `getdents64` + `statx` walker on Linux), keeps the result in a compact arena,
and shows it in a native GUI with three views of the same data: a size-sorted directory tree,
a per-extension breakdown, and an interactive treemap.

![inodetree scanning a sample home directory: the directory tree, the extension table and the treemap, with search and the context menu](docs/demo.gif)

## Features

- Multi-threaded scan that stays on one filesystem and counts hard links once, so totals
  match `du` on Linux and NTFS on-disk allocation on Windows (while skipping OneDrive
  cloud-only placeholders without hydrating them).
- Directory tree table with size bars, percent of parent, item counts, modification time, and
  a Name column that automatically widens as you browse deeper into nested folders.
- File extension table with a color legend. Click an extension to highlight its files.
- Squarified treemap with nested folders, name labels, hover tooltips, click-to-select
  (the tree scrolls to the item), double-click zoom, and a clickable breadcrumb.
- Multi-selection with `Ctrl+Click` and `Shift+Click` in both the directory tree and the
  treemap color blocks.
- Search box (`Ctrl+F`): plain text matches part of a name, `*` and `?` make a glob such as
  `*.iso`.
- Move to Recycle Bin / Trash or delete permanently (with a confirmation warning) for single or
  multiple selected items, from the context menu or the keyboard. The scan results update
  instantly without rescanning.
- Allocated size (disk usage) or logical size (file length), switchable at any time.
- A `--headless` mode that prints a text report, useful for scripts and benchmarks.

## Install

Pre-built binaries and packages are attached to each
[GitHub release](https://github.com/revisionhiep-create/inodetree/releases). None of the
downloads are code-signed.

### Windows (Portable `.exe`)

1. Download the standalone executable directly from the repository root:
   - Direct link: [Download inodetree.exe](https://github.com/revisionhiep-create/inodetree/raw/main/inodetree.exe)
2. Save `inodetree.exe` to any folder (or a USB drive) and double-click to run — no installer and
   no Visual C++ Redistributable are required.
3. Because the binary is unsigned, Windows SmartScreen may show a prompt on first launch; choose
   **More info → Run anyway** if you trust the build.
4. To scan protected system directories (such as `C:\Windows` or other users' folders), run
   `inodetree.exe` as Administrator. Unreadable folders are skipped and counted in the status bar.

### Fedora, RHEL, Rocky, AlmaLinux

Linux packages are built on AlmaLinux 9, so they need glibc 2.34 or newer. Replace `0.1.1`
with the version you downloaded.

```sh
sudo dnf install ./inodetree-0.1.1-1.x86_64.rpm
```

### openSUSE

```sh
sudo zypper --no-gpg-checks install ./inodetree-0.1.1-1.x86_64.rpm
```

Use the `aarch64` file on ARM machines. Each release also lists `SHA256SUMS`.

### Other Linux distributions (tarball)

Replace `0.1.1` with the version you downloaded.

```sh
tar xzf inodetree-0.1.1-linux-x86_64.tar.gz
sudo install -Dm755 inodetree-0.1.1-linux-x86_64/inodetree /usr/local/bin/inodetree
sudo install -Dm644 inodetree-0.1.1-linux-x86_64/assets/inodetree.desktop \
    /usr/local/share/applications/inodetree.desktop
sudo install -Dm644 inodetree-0.1.1-linux-x86_64/assets/inodetree.svg \
    /usr/local/share/icons/hicolor/scalable/apps/inodetree.svg
```

### Build from source (Windows)

You need Rust 1.85 or newer and the Microsoft C++ Build Tools (no separate runtime installers
are needed for the output `.exe` because `.cargo/config.toml` links the MSVC CRT statically):

```powershell
git clone https://github.com/revisionhiep-create/inodetree.git
cd inodetree
cargo build --release
.\target\release\inodetree.exe
```

### Build from source (Linux)

You need Rust 1.85 or newer and a C linker. No `-devel` packages are required.

```sh
git clone https://github.com/revisionhiep-create/inodetree.git
cd inodetree
cargo build --release
./target/release/inodetree
```

## Runtime requirements

- **Windows:** Windows 10 or Windows 11 (x64) with standard built-in OpenGL graphics support.
  No external DLLs or Visual C++ Redistributable are required.
- **Linux:** An OpenGL stack and a keyboard library, which every desktop distribution already
  ships: `libEGL.so.1` and `libxkbcommon.so.0` (the RPM requires both). Runs on Wayland and X11.

## Usage

**Windows:**

```powershell
inodetree.exe                                   # open the GUI, pick a drive or type a path, press Scan
inodetree.exe C:\Users                          # open the GUI and scan C:\Users right away
inodetree.exe --headless C:\Users               # print a text report instead of opening a window
inodetree.exe --headless C:\Users --top 20 --logical
```

**Linux:**

```sh
inodetree                    # open the GUI, pick a drive or type a path, press Scan
inodetree /home              # open the GUI and scan /home right away
inodetree --headless /var    # print a text report instead of opening a window
inodetree --headless /var --top 20 --logical
```

| Input | Action |
| --- | --- |
| `Click` | Select an item |
| `Ctrl+Click` | Add or remove an item from the selection (tree or treemap) |
| `Shift+Click` | Select a range of items from the last clicked item (tree or treemap) |
| `Ctrl+F` | Focus the search box (`Esc` clears it) |
| `F5` | Rescan |
| `Delete` | Move the selected item(s) to the Recycle Bin / Trash |
| `Shift+Delete` | Delete the selected item(s) permanently (asks for confirmation) |
| `Ctrl+C` | Copy the selected item(s)' full path(s) |

Right-click a row in the tree or a block in the treemap for the same context menu.
The scan root and mount points inside the scan cannot be deleted from the app.

## Troubleshooting

- **Windows SmartScreen warning:** The portable `.exe` is unsigned; click **More info → Run anyway**
  or build from source locally.
- **Linux — window does not open and terminal mentions EGL, GL or xkbcommon:** install the
  missing libraries. On Fedora and RHEL: `sudo dnf install libglvnd-egl libxkbcommon`.
  On openSUSE: `sudo zypper install libglvnd libxkbcommon0`.
- **Linux X11/Wayland sessions:** X11 uses `libX11`, `libXcursor`, `libXi`, and `libXrender`;
  Wayland uses `libwayland-client` and `libwayland-egl`. A normal desktop install has them.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The scan engine, arena, filter and treemap layout have no GUI dependency, so the tests run
without a display server. To build the RPM locally on Linux:

```sh
cargo install cargo-generate-rpm
cargo build --release
cargo generate-rpm
```

Releases are built by GitHub Actions when a tag such as `v0.1.1` is pushed. The tag must
match the version in `Cargo.toml`.

## License

MIT, see [LICENSE](LICENSE).
