//! Build script — Windows-only: embed the app icon into the `.exe`.
//!
//! On Windows the taskbar / window / Explorer icon comes from an icon *resource*
//! compiled into the executable; there's no equivalent of macOS's `.app` bundle
//! (which gets its icon from `tty7.icns` via `.github/scripts/bundle.sh`). So we
//! compile `assets/favicon.ico` (a multi-res 16–256px ICO) into the binary here.
//!
//! On every other platform this is a no-op.

fn main() {
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/favicon.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/favicon.ico");
        if let Err(e) = res.compile() {
            // Don't fail the build just because the resource compiler is missing;
            // the app still runs, it just falls back to the default Windows icon.
            println!("cargo:warning=failed to embed Windows icon: {e}");
        }
    }
}
