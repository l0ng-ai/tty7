# tty7 mobile

Watch and drive your desktop's tty7 panes and agents from a phone.

```
phone (Tauri: WebView + Rust)  ──iroh──▶  tty7-gateway  ──local sockets──▶  tty7 server
   xterm.js ◀─ raw bytes ─ tty7-mobile-client          (crates/tty7-gateway)
```

- **Transport**: [iroh](https://iroh.computer). The phone dials the desktop by public key.
  Connections hole-punch to a direct path when the network allows and fall back to a relay
  when it doesn't. Either way they're end-to-end encrypted. No port forwarding, no VPN.
- **Protocol**: `crates/tty7-mobile-proto`. The phone never speaks the daemon's own
  protocols. The gateway exposes a small vocabulary: pair, a live tree of workspaces, tabs,
  panes and agent status, and one stream per open pane.
- **Panes are observed, not attached.** A phone never takes a pane away from the desktop
  window showing it. Keystrokes go in beside the observer (`SendInput`). The terminal keeps
  the desktop's size, and the app shrinks the font to fit the width.
- **Take over.** The phone button runs the pane at the phone's grid instead: a size
  lease the daemon holds for the observer (`ClientMsg::Lease`, feature `size-lease`). The
  desktop window keeps its grid, shows "In use on <phone>" with **Take Back**, and a resize
  there is remembered for when the lease ends. It ends when the phone lets go, when the
  desktop takes it back, or when the phone's stream closes — a phone that just drops off
  gives the pane back once the connection times out.
- **Auth**: pairing with a one-time code, valid 10 minutes, single use. After that the
  gateway admits only the phone keys on its device list (`<config dir>/mobile/devices.json`).

## Run it

On the desktop: **Settings → Mobile → Allow phone access**. The local daemon
runs the gateway from then on, with every window closed too. **Show code** there
gives a QR code and a `tty7pair:` code, and the phones it paired are listed
below it, each with an Unpair button.

Whichever daemon is running starts the gateway as its own child process and
stops it with the switch. `tty7-app --daemon` runs itself as
`tty7-app --mobile-gateway`. The lean `tty7-server` runs a `tty7-gateway`
from beside it or on PATH, and without one it says so in Settings. The gateway
exits when its daemon does, so an update never leaves an old one running.

Without the GUI, the same gateway runs from the command line. It shares the
state in `<config dir>/mobile/`, so the two never run at once:

```sh
cargo run -p tty7-gateway -- serve     # keep this running
cargo run -p tty7-gateway -- pair      # prints a QR code + a tty7pair:… code
cargo run -p tty7-gateway -- devices   # paired phones; `revoke <name>` removes one
```

### The app on the desktop (fastest loop)

Tauri builds the same app for macOS, which is the quickest way to work on the UI:

```sh
cd mobile
npm install
npm run tauri dev
```

Paste the `tty7pair:` code and tap **Pair**.

### iOS

Needs the full Xcode, not just the Command Line Tools, plus an Apple developer account to
run on a device.

```sh
rustup target add aarch64-apple-ios aarch64-apple-ios-sim
cd mobile
npm run tauri ios init       # once: generates src-tauri/gen/apple
npm run tauri ios dev        # simulator, or pick a connected device
```

To send a build to TestFlight (Xcode signed in to an account on the team):

```sh
scripts/testflight.sh              # archive, sign for the App Store, upload
scripts/testflight.sh --no-upload  # a signed .ipa in build/testflight/ instead
```

The build is `<version>.<n>`; `n` defaults to the time, so each upload is higher than the
last. `--build-number 7` picks it.

### Android

Needs Android Studio's SDK and NDK, with `ANDROID_HOME` and `NDK_HOME` set.

```sh
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
cd mobile
npm run tauri android init
npm run tauri android dev
```

### Without a phone

`crates/tty7-mobile-client/examples/probe.rs` is a phone in a shell. It pairs, prints the
tree, and times keystroke echo on a pane:

```sh
cargo run -p tty7-mobile-client --example probe -- pair '<code>'
cargo run -p tty7-mobile-client --example probe -- tree
cargo run -p tty7-mobile-client --example probe -- type 1 'echo hi'
```

## Finding the computer again

A pairing code carries the gateway's addresses at the time it was made. Those
stay good across restarts: `serve` listens on the same UDP port every time
(`<config dir>/mobile/port`, a fresh one only if it is taken). When they go stale
anyway — a new DHCP lease, a new IPv6 prefix — the phone looks the gateway up by
key: through n0's relay and DNS where those are reachable, and by mDNS
(`_tty7._udp`) on the same local network where they are not.

mDNS needs the OS's local-network permission on both ends. On macOS that belongs
to whatever launched the gateway (your terminal), and it answers "Allow … to find
devices on local networks" once. On iOS the app declares it in
`src-tauri/Info.ios.plist`. Without it, known addresses and the relay still work.

## Not done yet

- **QR scanning.** The app takes a pasted code for now. Next step is
  `tauri-plugin-barcode-scanner` on mobile.
- **Push notifications** when an agent starts waiting. The gateway sees the status change,
  but APNs/FCM delivery needs a small native plugin and a push relay.
- **Keychain / Keystore** for the phone's key. It lives in the app's private data
  directory today.
- **A self-hosted iroh relay** for production, instead of n0's public ones.
