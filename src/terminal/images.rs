//! Client-side kitty-graphics image store (issue #213).
//!
//! The daemon lifts image transmissions out of the PTY byte stream and forwards
//! them out-of-band as compact frames (`DaemonMsg::Image` / `DaemonMsg::DeleteImage`
//! — see [`tty7_core::core::kitty_graphics`]). This module is the client end: it
//! decodes each frame into a GPUI [`RenderImage`], anchors it to the grid cell the
//! cursor sat on when the command arrived, and hands the placed images to the
//! paint path so the element can blit them over the character grid.
//!
//! # Why anchor by absolute row
//!
//! An image's position has to survive scrolling. A kitty image is placed at the
//! cursor cell as it stood when its command appeared in the stream; once recorded,
//! the grid keeps scrolling under it. We store the row as an absolute index from
//! the top of scrollback (`history_size - display_offset + cursor_line`) and
//! convert back to a screen row at paint time (`anchor_row - history_size +
//! display_offset`, the inverse conversion). Below the scrollback limit — where a
//! pane spends most of its life — this is exact; past it the anchor drifts by the
//! (unobservable) discard count, and a browser that redraws every frame corrects
//! it on the next transmit anyway.
//!
//! The anchor is read off whichever grid is active, and the alt screen has no
//! history of its own — so an image placed there records a small absolute row
//! that resolves against the primary grid once the app exits. A sender that
//! deletes its own images on the way out (the normal case) is unaffected; one
//! that dies without an `a=d` can leave a frame anchored over the primary
//! screen. Modelling that properly wants a per-screen store rather than one
//! keyed on the displayed grid.
//!
//! GPUI's sprite atlas expects **BGRA** pixels (it swaps R↔B when caching an
//! `image` crate `RgbaImage` — see `gpui::img`), so [`decode`] does the swap once
//! at ingest; the placed [`RenderImage`] is uploaded verbatim thereafter.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use gpui::RenderImage;
use image::{Frame, RgbaImage};
use smallvec::SmallVec;

use tty7_core::core::kitty_graphics::{Image, ImageDelete, WireFormat};

/// One image placed on the grid, ready to blit.
#[derive(Clone)]
pub struct PlacedImage {
    /// Decoded pixels as a GPUI render image (BGRA, one frame).
    pub data: Arc<RenderImage>,
    /// Row index from the top of scrollback at placement time — the position
    /// anchor. See the module docs for when it stops being exact.
    pub anchor_row: i64,
    /// Column of the top-left cell.
    pub anchor_col: usize,
    /// Source pixel dimensions, for deriving the cell span when the sender did
    /// not give an explicit one.
    pub width_px: u32,
    pub height_px: u32,
    /// Explicit cell span the sender requested (`c=` / `r=`); 0 means "derive
    /// from the pixel size and the cell size at paint time".
    pub cols: u32,
    pub rows: u32,
    /// The kitty image id (`i=`) and placement id (`p=`), for targeted deletes.
    /// `id == 0` is an anonymous image, removable only by a delete-all.
    pub id: u32,
    pub placement: u32,
    /// Set immediately before the paint path hands this frame to GPUI. Frames
    /// replaced before their first paint have no atlas allocation to evict and
    /// can release their pixel buffer immediately.
    pub painted: Arc<AtomicBool>,
}

/// A pane's placed images plus the retired render images awaiting atlas
/// eviction, shared between the reader thread (writer) and the paint path
/// (reader).
///
/// `retired` is the other half of the fix for a browser that repaints at 60fps:
/// each transmitted frame becomes a fresh [`RenderImage`] with a new atlas id,
/// so the *previous* frame's GPU tile has to be dropped or the sprite atlas
/// grows without bound (see [`take_retired`](ImageStore::take_retired)). The
/// reader can't touch the atlas — that needs `&mut Window` — so it parks the
/// superseded `Arc`s here and the paint path drains and drops them.
#[derive(Default)]
struct StoreInner {
    placed: Vec<PlacedImage>,
    retired: Vec<Arc<RenderImage>>,
}

/// A pane's placed images, shared between the reader thread (writer) and the
/// paint path (reader).
#[derive(Clone, Default)]
pub struct ImageStore(Arc<Mutex<StoreInner>>);

/// Cap on placed images retained at once. A browser deletes-then-transmits every
/// frame, so the live set is tiny; this only bounds a sender that transmits
/// without ever deleting, dropping the oldest rather than growing without limit.
const MAX_IMAGES: usize = 256;

impl ImageStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Place a freshly received image at (`anchor_row`, `anchor_col`). A new
    /// transmission with the same identity as an existing one replaces it in
    /// place (kitty reuses an id to update an image); otherwise it is appended.
    /// The replaced frame's render image is retired for atlas eviction.
    pub fn place(&self, img: PlacedImage) {
        let Ok(mut inner) = self.0.lock() else { return };
        let StoreInner { placed, retired } = &mut *inner;
        // Same (id, placement) → replace. An anonymous image (id 0) never
        // matches, so each anonymous transmit is a distinct placement.
        if img.id != 0 {
            placed.retain(|p| {
                let same = p.id == img.id && p.placement == img.placement;
                if same {
                    retire_if_painted(p, retired);
                }
                !same
            });
        }
        placed.push(img);
        let overflow = placed.len().saturating_sub(MAX_IMAGES);
        if overflow > 0 {
            for p in placed.drain(..overflow) {
                retire_if_painted(&p, retired);
            }
        }
    }

    /// Apply a delete selector. Only the targets a sender tty7 faces actually
    /// uses are honored (all / by id / by placement); richer kitty selectors
    /// (by cell, by z-index, by number) are left in place rather than guessed.
    /// Removed frames' render images are retired for atlas eviction.
    pub fn delete(&self, del: &ImageDelete) {
        let Ok(mut inner) = self.0.lock() else { return };
        let retire = |p: &PlacedImage, keep: bool, retired: &mut Vec<Arc<RenderImage>>| {
            if !keep {
                retire_if_painted(p, retired);
            }
            keep
        };
        let StoreInner { placed, retired } = &mut *inner;
        match del.target {
            // All visible placements. Case only governs whether kitty also frees
            // the image data; the client frees unconditionally, so both clear.
            b'a' | b'A' => {
                for p in placed.drain(..) {
                    retire_if_painted(&p, retired);
                }
            }
            // By image id.
            b'i' | b'I' => placed.retain(|p| retire(p, p.id != del.id, retired)),
            // By placement id (scoped to its image when an id is also given).
            b'p' | b'P' => placed.retain(|p| {
                let keep = p.placement != del.placement || (del.id != 0 && p.id != del.id);
                retire(p, keep, retired)
            }),
            // A selector we don't model: leave the store untouched.
            _ => {}
        }
    }

    /// Snapshot the placed images for a paint pass. Cheap: the live set is small
    /// and `PlacedImage` is a handful of fields plus an `Arc` clone.
    pub fn snapshot(&self) -> Vec<PlacedImage> {
        self.0.lock().map(|s| s.placed.clone()).unwrap_or_default()
    }

    /// Claim one still-current placement immediately before painting it.
    ///
    /// The store lock closes the race with replacement: once this returns true,
    /// a replacing frame must retire the old atlas image. If the snapshot is
    /// already stale, it is not painted and needs no later atlas cleanup.
    pub fn claim_for_paint(&self, image: &PlacedImage) -> bool {
        self.0
            .lock()
            .map(|inner| {
                let current = inner
                    .placed
                    .iter()
                    .any(|placed| Arc::ptr_eq(&placed.data, &image.data));
                if current {
                    image.painted.store(true, Ordering::Release);
                }
                current
            })
            .unwrap_or(false)
    }

    /// Take the render images retired since the last call, for the paint path to
    /// evict from the sprite atlas (`Window::drop_image`). Draining here — the
    /// one place with `&mut Window` — is what stops a 60fps re-transmitting
    /// sender from leaking a GPU tile per frame and dragging the compositor down.
    pub fn take_retired(&self) -> Vec<Arc<RenderImage>> {
        self.0
            .lock()
            .map(|mut s| std::mem::take(&mut s.retired))
            .unwrap_or_default()
    }

    /// Drain every image that may have an atlas allocation when its pane is
    /// released. The caller must stop the decode worker first so no new frame
    /// can land after this drain.
    pub fn take_for_release(&self) -> Vec<Arc<RenderImage>> {
        self.0
            .lock()
            .map(|mut inner| {
                let mut images = std::mem::take(&mut inner.retired);
                for placed in inner.placed.drain(..) {
                    if placed.painted.load(Ordering::Acquire) {
                        images.push(placed.data);
                    }
                }
                images
            })
            .unwrap_or_default()
    }

    /// Drop everything (the grid was cleared, so every anchor is meaningless).
    /// Placed frames are retired so their atlas tiles are still evicted.
    pub fn clear(&self) {
        if let Ok(mut inner) = self.0.lock() {
            let StoreInner { placed, retired } = &mut *inner;
            for p in placed.drain(..) {
                retire_if_painted(&p, retired);
            }
        }
    }
}

fn retire_if_painted(img: &PlacedImage, retired: &mut Vec<Arc<RenderImage>>) {
    if img.painted.load(Ordering::Acquire) {
        retired.push(img.data.clone());
    }
}

/// A raw (still-compressed) image frame handed to the [`DecodeWorker`], with the
/// grid anchor the reader captured the instant the transmission arrived. Keeping
/// the anchor with the frame lets decoding move off the reader thread without
/// losing the cursor position the image was drawn at.
pub struct PendingFrame {
    pub img: Image,
    pub anchor_row: i64,
    pub anchor_col: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FrameIdentity {
    id: u32,
    placement: u32,
}

impl FrameIdentity {
    fn of(img: &Image) -> Self {
        Self {
            id: img.id,
            placement: img.placement,
        }
    }

    fn deleted_by(self, del: &ImageDelete) -> bool {
        match del.target {
            b'a' | b'A' => true,
            b'i' | b'I' => self.id == del.id,
            b'p' | b'P' => self.placement == del.placement && (del.id == 0 || self.id == del.id),
            _ => false,
        }
    }
}

struct QueuedFrame {
    frame: PendingFrame,
    identity: FrameIdentity,
    replaceable: bool,
    sequence: u64,
}

#[derive(Default)]
struct InboxState {
    pending: VecDeque<QueuedFrame>,
    pending_bytes: usize,
    latest: Vec<(FrameIdentity, u64)>,
    in_flight: Option<(u64, FrameIdentity)>,
    cancelled_in_flight: Option<u64>,
    next_sequence: u64,
    closed: bool,
}

struct DecodeInbox {
    state: Mutex<InboxState>,
    ready: Condvar,
}

const MAX_PENDING_FRAMES: usize = 4;
const MAX_PENDING_BYTES: usize = tty7_core::core::kitty_graphics::MAX_IMAGE_BYTES;

impl DecodeInbox {
    fn new() -> Self {
        Self {
            state: Mutex::new(InboxState::default()),
            ready: Condvar::new(),
        }
    }

    fn submit(&self, frame: PendingFrame) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.closed {
            return;
        }

        state.next_sequence = state.next_sequence.wrapping_add(1);
        let sequence = state.next_sequence;
        let identity = FrameIdentity::of(&frame.img);
        let replaceable = frame.img.id != 0;
        if replaceable {
            if let Some(index) = state
                .pending
                .iter()
                .position(|queued| queued.replaceable && queued.identity == identity)
                && let Some(old) = state.pending.remove(index)
            {
                state.pending_bytes = state.pending_bytes.saturating_sub(old.frame.img.data.len());
            }
            if let Some((_, latest)) = state
                .latest
                .iter_mut()
                .find(|(candidate, _)| *candidate == identity)
            {
                *latest = sequence;
            } else {
                state.latest.push((identity, sequence));
            }
        }

        state.pending_bytes = state.pending_bytes.saturating_add(frame.img.data.len());
        state.pending.push_back(QueuedFrame {
            frame,
            identity,
            replaceable,
            sequence,
        });
        while state.pending.len() > MAX_PENDING_FRAMES
            || (state.pending.len() > 1 && state.pending_bytes > MAX_PENDING_BYTES)
        {
            if let Some(old) = state.pending.pop_front() {
                state.pending_bytes = state.pending_bytes.saturating_sub(old.frame.img.data.len());
                remove_latest_if(
                    &mut state.latest,
                    old.replaceable.then_some(old.identity),
                    old.sequence,
                );
            }
        }
        drop(state);
        self.ready.notify_one();
    }

    fn delete(&self, del: &ImageDelete, store: &ImageStore) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state
            .latest
            .retain(|(identity, _)| !identity.deleted_by(del));
        if let Some((sequence, identity)) = state.in_flight
            && identity.deleted_by(del)
        {
            state.cancelled_in_flight = Some(sequence);
        }
        let mut kept = VecDeque::with_capacity(state.pending.len());
        while let Some(queued) = state.pending.pop_front() {
            if queued.identity.deleted_by(del) {
                state.pending_bytes = state
                    .pending_bytes
                    .saturating_sub(queued.frame.img.data.len());
            } else {
                kept.push_back(queued);
            }
        }
        state.pending = kept;
        store.delete(del);
    }

    fn recv(&self) -> Option<QueuedFrame> {
        let mut state = self.state.lock().ok()?;
        while state.pending.is_empty() && !state.closed {
            state = self.ready.wait(state).ok()?;
        }
        let queued = state.pending.pop_front()?;
        state.pending_bytes = state
            .pending_bytes
            .saturating_sub(queued.frame.img.data.len());
        state.in_flight = Some((queued.sequence, queued.identity));
        Some(queued)
    }

    fn place_if_current(
        &self,
        queued: QueuedFrame,
        decoded: (Arc<RenderImage>, u32, u32),
        store: &ImageStore,
    ) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        state.in_flight = None;
        if state.cancelled_in_flight == Some(queued.sequence) {
            state.cancelled_in_flight = None;
            remove_latest_if(
                &mut state.latest,
                queued.replaceable.then_some(queued.identity),
                queued.sequence,
            );
            return false;
        }
        if queued.replaceable
            && !state.latest.iter().any(|(identity, sequence)| {
                *identity == queued.identity && *sequence == queued.sequence
            })
        {
            return false;
        }

        remove_latest_if(
            &mut state.latest,
            queued.replaceable.then_some(queued.identity),
            queued.sequence,
        );
        let (data, width_px, height_px) = decoded;
        store.place(PlacedImage {
            data,
            anchor_row: queued.frame.anchor_row,
            anchor_col: queued.frame.anchor_col,
            width_px,
            height_px,
            cols: queued.frame.img.cols,
            rows: queued.frame.img.rows,
            id: queued.frame.img.id,
            placement: queued.frame.img.placement,
            painted: Arc::new(AtomicBool::new(false)),
        });
        true
    }

    fn discard(&self, queued: QueuedFrame) {
        if let Ok(mut state) = self.state.lock() {
            state.in_flight = None;
            if state.cancelled_in_flight == Some(queued.sequence) {
                state.cancelled_in_flight = None;
            }
            remove_latest_if(
                &mut state.latest,
                queued.replaceable.then_some(queued.identity),
                queued.sequence,
            );
        }
    }

    fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
        }
        self.ready.notify_one();
    }

    #[cfg(test)]
    fn pending_frames(&self) -> usize {
        self.state
            .lock()
            .map(|state| state.pending.len())
            .unwrap_or(0)
    }
}

fn remove_latest_if(
    latest: &mut Vec<(FrameIdentity, u64)>,
    key: Option<FrameIdentity>,
    sequence: u64,
) {
    if let Some(key) = key {
        latest.retain(|(candidate, current)| *candidate != key || *current != sequence);
    }
}

/// Off-thread image decoder with newest-frame-wins coalescing — the crux of the
/// performance story (issue #213).
///
/// A full-window browser frame is ~28 MB of RGBA after zlib inflate, and the
/// inflate alone measures ~42 ms. Doing that inline on the reader thread (which
/// also services PTY output and scrolling) blocks the whole pane for ~42 ms per
/// frame, and a 60fps sender queues frames faster than they drain — latency
/// grows without bound and scrolling stutters. This is the cost the
/// device-pixel resolution bump quadrupled.
///
/// The fix mirrors how kitty/ghostty stay smooth: decode off the I/O thread, and
/// when the producer outruns the decoder, **drop stale frames instead of
/// queuing them**. A bounded [`inbox`] of one *replaceable slot per image id*
/// means a re-transmitting browser only ever has its latest frame per image
/// waiting; older undecoded frames are discarded before they cost an inflate.
/// The worker decodes the newest, `place`s it, and wakes the view.
///
/// [`inbox`]: DecodeWorker::inbox
pub struct DecodeWorker {
    inbox: Arc<DecodeInbox>,
    store: ImageStore,
    handle: Option<JoinHandle<()>>,
}

impl DecodeWorker {
    /// Spawn the decode thread. `store` is the shared placement store the worker
    /// writes decoded frames into; `wake` is called after each successful decode
    /// so the view repaints (a cloned `EventProxy::send_event(Wakeup)` in
    /// practice). The thread ends after the returned worker closes and drains
    /// its bounded inbox.
    pub fn spawn(store: ImageStore, wake: impl Fn() + Send + 'static) -> Self {
        Self::spawn_with_decoder(store, wake, decode)
    }

    fn spawn_with_decoder(
        store: ImageStore,
        wake: impl Fn() + Send + 'static,
        decoder: impl Fn(&mut Image) -> Option<(Arc<RenderImage>, u32, u32)> + Send + 'static,
    ) -> Self {
        let inbox = Arc::new(DecodeInbox::new());
        let worker_inbox = inbox.clone();
        let worker_store = store.clone();
        let handle = std::thread::Builder::new()
            .name("tty7-image-decode".to_string())
            .spawn(move || Self::run(worker_inbox, worker_store, wake, decoder))
            .ok();
        Self {
            inbox,
            store,
            handle,
        }
    }

    /// Hand a raw frame to the worker. Never blocks the caller (the reader
    /// thread): the frame is queued and decoded asynchronously. If the worker
    /// has gone away the frame is silently dropped — the pane is tearing down.
    pub fn submit(&self, frame: PendingFrame) {
        self.inbox.submit(frame);
    }

    pub fn delete(&self, del: &ImageDelete) {
        self.inbox.delete(del, &self.store);
    }

    /// The decode loop blocks for the next already-coalesced frame. Replacement
    /// happens synchronously in `submit`, before a large payload can accumulate
    /// behind a slow inflate.
    fn run(
        inbox: Arc<DecodeInbox>,
        store: ImageStore,
        wake: impl Fn(),
        decoder: impl Fn(&mut Image) -> Option<(Arc<RenderImage>, u32, u32)>,
    ) {
        while let Some(mut queued) = inbox.recv() {
            match decoder(&mut queued.frame.img) {
                Some(decoded) => {
                    if inbox.place_if_current(queued, decoded, &store) {
                        wake();
                    }
                }
                None => inbox.discard(queued),
            }
        }
    }

    #[cfg(test)]
    fn pending_frames(&self) -> usize {
        self.inbox.pending_frames()
    }
}

impl Drop for DecodeWorker {
    fn drop(&mut self) {
        self.inbox.close();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Decode a daemon [`Image`] frame into GPUI-ready pixels: inflate + expand to
/// RGBA (via the protocol type's own [`Image::to_rgba8`]), or decode a PNG
/// payload, then swap R↔B to the BGRA the sprite atlas wants. Returns the render
/// image and its true pixel dimensions (a PNG carries its own, overriding any
/// `s=`/`v=` the sender may have omitted). `None` if the payload can't be decoded.
///
/// Takes the [`Image`] by `&mut` so the uncompressed path can *move* its pixel
/// buffer straight into the render image (via [`Image::take_rgba8`]) rather than
/// cloning ~26 MiB per frame — the client hot path for a re-transmitting browser.
/// Only the pixel buffer is consumed; the image's placement metadata is left
/// intact for the caller.
pub fn decode(img: &mut Image) -> Option<(Arc<RenderImage>, u32, u32)> {
    let (src_w, src_h) = (img.width, img.height);
    let (mut rgba, w, h) = match img.format {
        WireFormat::Png => {
            // The one path pixels stay encoded through: decode with the `image`
            // crate (a direct dep, same version gpui uses) rather than the
            // protocol type, which declines PNG on purpose.
            let dyn_img = image::load_from_memory(&img.data).ok()?;
            let buf = dyn_img.into_rgba8();
            let (w, h) = buf.dimensions();
            (buf.into_raw(), w, h)
        }
        WireFormat::Rgb | WireFormat::Rgba => {
            let rgba = img.take_rgba8()?;
            (rgba, src_w, src_h)
        }
    };
    if w == 0 || h == 0 || rgba.len() < (w as usize * h as usize * 4) {
        return None;
    }
    rgba.truncate(w as usize * h as usize * 4);
    // RGBA → BGRA for the atlas. A channel swap and nothing else: `RenderImage`
    // holds *straight* alpha, not premultiplied. gpui's own producers say so —
    // `swap_rgba_pa_to_bgra`, which both the CoreGraphics text rasterizer and
    // the SVG renderer run their premultiplied output through, divides the
    // color channels back out by alpha on the way in. Premultiplying here would
    // darken every translucent pixel of an `f=100` PNG twice over.
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let buffer = RgbaImage::from_raw(w, h, rgba)?;
    let render = RenderImage::new(SmallVec::from_elem(Frame::new(buffer), 1));
    Some((Arc::new(render), w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A raw 1x1 opaque-red RGBA image, the minimum a placement needs, built the
    /// way the daemon delivers one (base64-decoded, uncompressed).
    fn red_pixel() -> Image {
        Image {
            id: 0,
            number: 0,
            placement: 0,
            width: 1,
            height: 1,
            cols: 0,
            rows: 0,
            data: vec![0xff, 0x00, 0x00, 0xff],
            format: WireFormat::Rgba,
            compressed: false,
        }
    }

    fn placed(id: u32, placement: u32) -> PlacedImage {
        let (data, w, h) = decode(&mut red_pixel()).unwrap();
        PlacedImage {
            data,
            anchor_row: 0,
            anchor_col: 0,
            width_px: w,
            height_px: h,
            cols: 0,
            rows: 0,
            id,
            placement,
            painted: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn decodes_rgba_and_swaps_to_bgra() {
        let mut img = Image {
            data: vec![1, 2, 3, 4],
            ..red_pixel()
        };
        let (data, w, h) = decode(&mut img).unwrap();
        assert_eq!((w, h), (1, 1));
        // Red/blue swapped: RGBA [1,2,3,4] → BGRA [3,2,1,4].
        assert_eq!(data.as_bytes(0).unwrap(), &[3, 2, 1, 4]);
    }

    #[test]
    fn same_id_replaces_in_place() {
        let store = ImageStore::new();
        store.place(placed(7, 1));
        store.place(placed(7, 1));
        assert_eq!(
            store.snapshot().len(),
            1,
            "a re-transmit replaces, not stacks"
        );
    }

    #[test]
    fn anonymous_images_coexist() {
        let store = ImageStore::new();
        store.place(placed(0, 0));
        store.place(placed(0, 0));
        assert_eq!(
            store.snapshot().len(),
            2,
            "id 0 is a fresh placement each time"
        );
    }

    #[test]
    fn delete_all_clears_everything() {
        let store = ImageStore::new();
        store.place(placed(1, 0));
        store.place(placed(2, 0));
        store.delete(&ImageDelete {
            target: b'A',
            id: 0,
            placement: 0,
        });
        assert!(store.snapshot().is_empty());
    }

    #[test]
    fn delete_by_id_leaves_others() {
        let store = ImageStore::new();
        store.place(placed(1, 0));
        store.place(placed(2, 0));
        store.delete(&ImageDelete {
            target: b'i',
            id: 1,
            placement: 0,
        });
        let left = store.snapshot();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, 2);
    }

    #[test]
    fn unknown_selector_is_a_no_op() {
        let store = ImageStore::new();
        store.place(placed(1, 0));
        // `z` (by z-index) isn't modeled; the image must survive.
        store.delete(&ImageDelete {
            target: b'z',
            id: 0,
            placement: 0,
        });
        assert_eq!(store.snapshot().len(), 1);
    }

    #[test]
    fn overflow_drops_the_oldest() {
        let store = ImageStore::new();
        for i in 0..(MAX_IMAGES as u32 + 5) {
            store.place(placed(i + 1, 0));
        }
        let left = store.snapshot();
        assert_eq!(left.len(), MAX_IMAGES);
        assert_eq!(left[0].id, 6, "the five oldest aged out");
    }

    #[test]
    fn replacing_a_frame_retires_the_old_render_image() {
        let store = ImageStore::new();
        store.place(placed(7, 1));
        let image = store.snapshot().pop().unwrap();
        assert!(store.claim_for_paint(&image));
        // A same-id re-transmit (what a 60fps browser does) must retire the old
        // frame's render image so the paint path can evict its atlas tile.
        store.place(placed(7, 1));
        assert_eq!(store.snapshot().len(), 1, "still one live placement");
        assert_eq!(
            store.take_retired().len(),
            1,
            "the superseded frame is retired"
        );
        assert!(store.take_retired().is_empty(), "draining is one-shot");
    }

    #[test]
    fn deletes_and_clear_retire_render_images() {
        let store = ImageStore::new();
        store.place(placed(1, 0));
        store.place(placed(2, 0));
        for image in store.snapshot() {
            assert!(store.claim_for_paint(&image));
        }
        let _ = store.take_retired(); // drain the (none) from placement
        store.delete(&ImageDelete {
            target: b'i',
            id: 1,
            placement: 0,
        });
        assert_eq!(
            store.take_retired().len(),
            1,
            "the deleted frame is retired"
        );
        store.clear();
        assert_eq!(
            store.take_retired().len(),
            1,
            "clear retires the survivor too"
        );
    }

    #[test]
    fn unpainted_replacements_do_not_accumulate_retired_pixels() {
        let store = ImageStore::new();
        for _ in 0..100 {
            store.place(placed(7, 1));
        }
        assert_eq!(store.snapshot().len(), 1);
        assert!(
            store.take_retired().is_empty(),
            "a background tab never uploaded these frames to the atlas"
        );
    }

    #[test]
    fn release_drains_retired_and_current_atlas_images() {
        let store = ImageStore::new();
        store.place(placed(1, 0));
        store.place(placed(2, 0));
        for image in store.snapshot() {
            assert!(store.claim_for_paint(&image));
        }
        store.delete(&delete_id(1));

        assert_eq!(
            store.take_for_release().len(),
            2,
            "release evicts both a retired frame and the current painted frame"
        );
        assert!(store.snapshot().is_empty());
        assert!(store.take_for_release().is_empty());
    }

    #[test]
    fn stale_snapshot_cannot_be_claimed_after_replacement() {
        let store = ImageStore::new();
        store.place(placed(7, 1));
        let stale = store.snapshot().pop().unwrap();
        store.place(placed(7, 1));

        assert!(
            !store.claim_for_paint(&stale),
            "a snapshot replaced before paint must not allocate an orphan atlas tile"
        );
    }

    fn frame(id: u32) -> PendingFrame {
        PendingFrame {
            img: Image { id, ..red_pixel() },
            anchor_row: 0,
            anchor_col: 0,
        }
    }

    fn delete_id(id: u32) -> ImageDelete {
        ImageDelete {
            target: b'I',
            id,
            placement: 0,
        }
    }

    /// The worker decodes off-thread and places what it receives. A single frame
    /// lands in the store, at the anchor the reader captured.
    #[test]
    fn worker_decodes_and_places_off_thread() {
        let store = ImageStore::new();
        let woken = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let w = woken.clone();
        let worker = DecodeWorker::spawn(store.clone(), move || {
            w.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        worker.submit(frame(7));
        drop(worker); // joins the thread, so the decode is finished on return
        let placed = store.snapshot();
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].id, 7);
        assert!(
            woken.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "a successful decode wakes the view"
        );
    }

    /// A burst of same-id frames (what a re-transmitting browser produces faster
    /// than the decoder drains) collapses to a single live placement — stale
    /// frames are coalesced away rather than each costing an inflate. The store's
    /// own same-id replacement guarantees the end state even if the worker only
    /// sees them one at a time, so this asserts the invariant the pipeline keeps.
    #[test]
    fn worker_coalesces_a_same_id_burst_to_one_placement() {
        let store = ImageStore::new();
        let worker = DecodeWorker::spawn(store.clone(), || {});
        for _ in 0..50 {
            worker.submit(frame(9));
        }
        drop(worker); // drains + joins
        assert_eq!(
            store.snapshot().len(),
            1,
            "a same-id burst leaves exactly one live frame"
        );
    }

    #[test]
    fn same_id_burst_is_bounded_before_decoder_can_drain() {
        use std::sync::Barrier;

        let store = ImageStore::new();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let blocked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker = DecodeWorker::spawn_with_decoder(store, || {}, {
            let entered = entered.clone();
            let release = release.clone();
            move |img| {
                if !blocked.swap(true, Ordering::SeqCst) {
                    entered.wait();
                    release.wait();
                }
                decode(img)
            }
        });

        worker.submit(frame(9));
        entered.wait();
        for _ in 0..16 {
            worker.submit(frame(9));
        }
        let pending = worker.pending_frames();
        release.wait();
        drop(worker);

        assert_eq!(
            pending, 1,
            "same-id frames must replace the pending frame before decode"
        );
    }

    #[test]
    fn delete_cancels_a_frame_already_being_decoded() {
        use std::sync::Barrier;

        let store = ImageStore::new();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let worker = DecodeWorker::spawn_with_decoder(store.clone(), || {}, {
            let entered = entered.clone();
            let release = release.clone();
            move |img| {
                entered.wait();
                release.wait();
                decode(img)
            }
        });

        worker.submit(frame(9));
        entered.wait();
        worker.delete(&delete_id(9));
        release.wait();
        drop(worker);

        assert!(
            store.snapshot().is_empty(),
            "an in-flight frame deleted before decode completed must stay deleted"
        );
    }

    #[test]
    fn delete_all_cancels_an_anonymous_frame_already_being_decoded() {
        use std::sync::Barrier;

        let store = ImageStore::new();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let worker = DecodeWorker::spawn_with_decoder(store.clone(), || {}, {
            let entered = entered.clone();
            let release = release.clone();
            move |img| {
                entered.wait();
                release.wait();
                decode(img)
            }
        });

        worker.submit(frame(0));
        entered.wait();
        worker.delete(&ImageDelete {
            target: b'A',
            id: 0,
            placement: 0,
        });
        release.wait();
        drop(worker);

        assert!(
            store.snapshot().is_empty(),
            "delete-all must cancel anonymous in-flight frames too"
        );
    }

    #[test]
    fn retransmit_after_delete_wins_over_the_old_in_flight_frame() {
        use std::sync::Barrier;

        let store = ImageStore::new();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let blocked = Arc::new(AtomicBool::new(false));
        let worker = DecodeWorker::spawn_with_decoder(store.clone(), || {}, {
            let entered = entered.clone();
            let release = release.clone();
            move |img| {
                if !blocked.swap(true, Ordering::SeqCst) {
                    entered.wait();
                    release.wait();
                }
                decode(img)
            }
        });

        worker.submit(frame(9));
        entered.wait();
        worker.delete(&delete_id(9));
        let mut replacement = frame(9);
        replacement.anchor_row = 42;
        worker.submit(replacement);
        release.wait();
        drop(worker);

        let placed = store.snapshot();
        assert_eq!(placed.len(), 1);
        assert_eq!(
            placed[0].anchor_row, 42,
            "the pre-delete decode must not overwrite its retransmission"
        );
    }

    /// Distinct ids are independent placements — coalescing is per id, so two
    /// different images both survive.
    #[test]
    fn worker_keeps_distinct_ids() {
        let store = ImageStore::new();
        let worker = DecodeWorker::spawn(store.clone(), || {});
        worker.submit(frame(1));
        worker.submit(frame(2));
        drop(worker);
        let mut ids: Vec<u32> = store.snapshot().iter().map(|p| p.id).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2]);
    }
}
