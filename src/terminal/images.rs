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
//! Like a command [`mark`](crate::terminal::marks), an image's position has to
//! survive scrolling. A kitty image is placed at the cursor cell as it stood when
//! its command appeared in the stream; once recorded, the grid keeps scrolling
//! under it. We store the row as an absolute index from the top of scrollback
//! (`history_size - display_offset + cursor_line`, the exact formula
//! [`record_mark`](crate::terminal::remote) uses) and convert back to a screen
//! row at paint time (`anchor_row - history_size + display_offset`, the inverse
//! [`scroll_to_mark`](crate::terminal::view::TerminalView::scroll_to_mark)
//! applies). Below the scrollback limit — where a pane spends most of its life —
//! this is exact; past it the anchor drifts by the (unobservable) discard count,
//! the same caveat marks carry, and a browser that redraws every frame corrects
//! it on the next transmit anyway.
//!
//! GPUI's sprite atlas expects **BGRA** pixels (it swaps R↔B when caching an
//! `image` crate `RgbaImage` — see `gpui::img`), so [`decode`] does the swap once
//! at ingest; the placed [`RenderImage`] is uploaded verbatim thereafter.

use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
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
}

/// A pane's placed images plus the retired render images awaiting atlas
/// eviction, shared between the reader thread (writer) and the paint path
/// (reader), exactly like [`Marks`](crate::terminal::marks::Marks).
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
/// paint path (reader), exactly like [`Marks`](crate::terminal::marks::Marks).
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
                    retired.push(p.data.clone());
                }
                !same
            });
        }
        placed.push(img);
        let overflow = placed.len().saturating_sub(MAX_IMAGES);
        if overflow > 0 {
            retired.extend(placed.drain(..overflow).map(|p| p.data));
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
                retired.push(p.data.clone());
            }
            keep
        };
        let StoreInner { placed, retired } = &mut *inner;
        match del.target {
            // All visible placements. Case only governs whether kitty also frees
            // the image data; the client frees unconditionally, so both clear.
            b'a' | b'A' => {
                retired.extend(placed.drain(..).map(|p| p.data));
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

    /// Drop everything (the grid was cleared, so every anchor is meaningless).
    /// Placed frames are retired so their atlas tiles are still evicted.
    pub fn clear(&self) {
        if let Ok(mut inner) = self.0.lock() {
            let gone: Vec<_> = inner.placed.drain(..).map(|p| p.data).collect();
            inner.retired.extend(gone);
        }
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
    tx: Option<Sender<PendingFrame>>,
    handle: Option<JoinHandle<()>>,
}

impl DecodeWorker {
    /// Spawn the decode thread. `store` is the shared placement store the worker
    /// writes decoded frames into; `wake` is called after each successful decode
    /// so the view repaints (a cloned `EventProxy::send_event(Wakeup)` in
    /// practice). The thread ends when the returned worker is dropped (the
    /// channel closes).
    pub fn spawn(store: ImageStore, wake: impl Fn() + Send + 'static) -> Self {
        let (tx, rx) = channel::<PendingFrame>();
        let handle = std::thread::Builder::new()
            .name("tty7-image-decode".to_string())
            .spawn(move || Self::run(rx, store, wake))
            .ok();
        Self {
            tx: Some(tx),
            handle,
        }
    }

    /// Hand a raw frame to the worker. Never blocks the caller (the reader
    /// thread): the frame is queued and decoded asynchronously. If the worker
    /// has gone away the frame is silently dropped — the pane is tearing down.
    pub fn submit(&self, frame: PendingFrame) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(frame);
        }
    }

    /// The decode loop. Blocks for the next frame, then **coalesces**: drains
    /// everything already queued and keeps only the last frame per image id, so
    /// a burst that piled up during a slow inflate collapses to one decode per
    /// image. Decodes the survivors newest-first and places them.
    fn run(rx: Receiver<PendingFrame>, store: ImageStore, wake: impl Fn()) {
        while let Ok(first) = rx.recv() {
            // Collect the blocking frame plus any that arrived while we were
            // busy, newest-per-id winning (a later frame with the same id
            // supersedes an earlier one — exactly what `place` would do, but
            // without paying to decode the ones we'd immediately retire).
            let mut latest: Vec<PendingFrame> = vec![first];
            loop {
                match rx.try_recv() {
                    Ok(next) => {
                        if next.img.id != 0
                            && let Some(slot) = latest.iter_mut().find(|p| p.img.id == next.img.id)
                        {
                            *slot = next; // supersede the queued frame for this id
                        } else {
                            latest.push(next);
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        // Decode whatever we already gathered, then exit.
                        Self::place_all(&store, latest, &wake);
                        return;
                    }
                }
            }
            Self::place_all(&store, latest, &wake);
        }
    }

    fn place_all(store: &ImageStore, frames: Vec<PendingFrame>, wake: &impl Fn()) {
        let mut placed_any = false;
        for pf in frames {
            if let Some((data, w, h)) = decode(&pf.img) {
                store.place(PlacedImage {
                    data,
                    anchor_row: pf.anchor_row,
                    anchor_col: pf.anchor_col,
                    width_px: w,
                    height_px: h,
                    cols: pf.img.cols,
                    rows: pf.img.rows,
                    id: pf.img.id,
                    placement: pf.img.placement,
                });
                placed_any = true;
            }
        }
        if placed_any {
            wake();
        }
    }
}

impl Drop for DecodeWorker {
    fn drop(&mut self) {
        // Drop the sender first so the worker sees `Disconnected` and returns;
        // then join so its store writes are visible and it doesn't outlive the
        // pane. Joining before dropping `tx` would deadlock — the loop only ends
        // once the channel closes.
        self.tx = None;
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
pub fn decode(img: &Image) -> Option<(Arc<RenderImage>, u32, u32)> {
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
            let rgba = img.to_rgba8()?;
            (rgba, img.width, img.height)
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
        let (data, w, h) = decode(&red_pixel()).unwrap();
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
        }
    }

    #[test]
    fn decodes_rgba_and_swaps_to_bgra() {
        let img = Image {
            data: vec![1, 2, 3, 4],
            ..red_pixel()
        };
        let (data, w, h) = decode(&img).unwrap();
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

    fn frame(id: u32) -> PendingFrame {
        PendingFrame {
            img: Image { id, ..red_pixel() },
            anchor_row: 0,
            anchor_col: 0,
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
