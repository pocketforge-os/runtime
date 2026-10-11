//! tsp-ihne1.12 evidence: measuring text must never perform font-sized allocations, and on-disk
//! fonts must reach the shaper as file-backed memory maps shared across systems and clones.
//!
//! Both controls run inside one invocation of each test: a negative control (owned-bytes
//! `FontSet::new` must be seen allocating the whole font) proves the counter works, and the
//! positive control (disk-backed set measures without a single font-sized allocation) is the
//! acceptance claim.

#![cfg(target_os = "linux")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Anything at or above this size counts as "font-sized": far above any shaping buffer this test
/// produces, far below the 19.5 MB CJK collection it must catch being copied.
const FONT_SIZED: usize = 1024 * 1024;

static LARGE_ALLOCS: AtomicUsize = AtomicUsize::new(0);
static LARGE_BYTES: AtomicUsize = AtomicUsize::new(0);

struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() >= FONT_SIZED {
            LARGE_ALLOCS.fetch_add(1, Ordering::SeqCst);
            LARGE_BYTES.fetch_add(layout.size(), Ordering::SeqCst);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size >= FONT_SIZED {
            LARGE_ALLOCS.fetch_add(1, Ordering::SeqCst);
            LARGE_BYTES.fetch_add(new_size, Ordering::SeqCst);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// The counting globals are process-wide; serialize the tests that snapshot them.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn snapshot() -> (usize, usize) {
    (
        LARGE_ALLOCS.load(Ordering::SeqCst),
        LARGE_BYTES.load(Ordering::SeqCst),
    )
}

fn fonts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fonts")
}

fn style(family: &str) -> pf_text_raster::ResolvedTextStyle {
    pf_text_raster::ResolvedTextStyle {
        family: family.to_owned(),
        size_px: 16.0,
        weight: 400,
        line_height: 1.5,
        tracking_em: 0.0,
    }
}

fn measure_corpus(set: &pf_text_raster::FontSet, iterations: usize) {
    let latin = style("Manrope");
    let cjk = style("Manrope");
    for iteration in 0..iterations {
        let (active, text) = if iteration % 2 == 0 {
            (
                &latin,
                "Measured body text wraps into additional lines as its accessible text scale grows",
            )
        } else {
            (&cjk, "再開 日本語")
        };
        let (width, height) = pf_text_raster::measure(set, active, text, 1.0, Some(240.0));
        assert!(
            width > 0.0 && height > 0.0,
            "measurement degenerated: {width}x{height}"
        );
    }
}

fn assert_file_backed(paths: &[PathBuf]) {
    let maps = std::fs::read_to_string("/proc/self/maps").expect("/proc/self/maps readable");
    for path in paths {
        let canonical = std::fs::canonicalize(path)
            .unwrap_or_else(|error| panic!("font path {}: {error}", path.display()));
        assert!(
            maps.lines()
                .any(|line| line.contains(canonical.to_str().expect("utf-8 font path"))),
            "no file-backed mapping for {} in /proc/self/maps",
            path.display()
        );
    }
}

#[test]
fn disk_font_measurement_never_copies_font_bytes() {
    let _guard = lock();
    let dir = fonts_dir();
    let manrope = dir.join("Manrope[wght].ttf");
    let fraunces = dir.join("Fraunces[SOFT,WONK,opsz,wght].ttf");
    let cjk = dir.join("NotoSansCJK-Regular.ttc");
    let cjk_bytes = std::fs::read(&cjk).expect("CJK font readable");

    // Negative control: the owned-bytes constructor must be caught copying the whole font, which
    // proves this allocator counter observes font-sized allocations when they happen.
    let (allocs_before, bytes_before) = snapshot();
    let bytes_set = pf_text_raster::FontSet::new(&[&cjk_bytes], "en-US");
    let system = bytes_set.font_system();
    assert!(
        system.db().faces().count() > 0,
        "bytes-backed system parsed no faces"
    );
    let (allocs_after, bytes_after) = snapshot();
    assert!(
        allocs_after - allocs_before >= 1 && bytes_after - bytes_before >= cjk_bytes.len(),
        "negative control failed: owned-bytes FontSet recorded only {} font-sized allocations \
         ({} bytes) for a {} byte font; the counter cannot see font copies",
        allocs_after - allocs_before,
        bytes_after - bytes_before,
        cjk_bytes.len()
    );
    drop(system);
    drop(bytes_set);

    // Positive control: disk-backed set, measurement loop, and clones must never allocate at
    // font scale because the mapping is shared, not copied.
    let paths = [manrope.clone(), fraunces.clone(), cjk.clone()];
    let disk_set =
        pf_text_raster::FontSet::from_files(&paths, "en-US").expect("disk-backed FontSet builds");
    measure_corpus(&disk_set, 3);
    let (allocs_before, bytes_before) = snapshot();
    measure_corpus(&disk_set, 25);
    let cloned = disk_set.clone();
    measure_corpus(&cloned, 25);
    let (allocs_after, bytes_after) = snapshot();
    assert_eq!(
        (allocs_after - allocs_before, bytes_after - bytes_before),
        (0, 0),
        "measuring disk-backed fonts copied font-sized bytes: {} allocations, {} bytes",
        allocs_after - allocs_before,
        bytes_after - bytes_before
    );

    assert_eq!(
        disk_set.source_paths(),
        vec![
            Some(manrope.clone()),
            Some(fraunces.clone()),
            Some(cjk.clone())
        ],
        "disk-backed set must report its file sources in load order"
    );
    assert_file_backed(&paths);
}

#[cfg(feature = "default-fonts")]
#[test]
fn default_fonts_are_disk_backed_and_measure_without_copies() {
    let _guard = lock();
    let set = pf_text_raster::pocketforge_default_fonts();
    let paths: Vec<PathBuf> = set
        .source_paths()
        .into_iter()
        .enumerate()
        .map(|(index, path)| {
            path.unwrap_or_else(|| panic!("default font {index} is embedded, not disk-backed"))
        })
        .collect();
    assert!(!paths.is_empty(), "no default fonts resolved");

    measure_corpus(&set, 3);
    let (allocs_before, bytes_before) = snapshot();
    measure_corpus(&set, 25);
    let (allocs_after, bytes_after) = snapshot();
    assert_eq!(
        (allocs_after - allocs_before, bytes_after - bytes_before),
        (0, 0),
        "measuring default fonts copied font-sized bytes: {} allocations, {} bytes",
        allocs_after - allocs_before,
        bytes_after - bytes_before
    );
    assert_file_backed(&paths);
}

#[cfg(feature = "default-fonts")]
#[test]
fn default_fonts_partial_override_dir_fails_closed() {
    let _guard = lock();
    // End-to-end complement to the resolver unit test: a partial $PF_FONTS_DIR
    // override (any canonical face missing) must fail closed at the public entry
    // point FOR THE CANONICAL-MISSING REASON, never silently substitute another
    // face and shift metrics. The latin-only case is the tsp-ihne1.12 reopen
    // regression: dropping NotoSansCJK-Regular.ttc removes the guaranteed CJK
    // fallback and must be refused by name, not merely trip over empty stubs.
    for (tag, stubs, expected_named) in [
        (
            "manrope-only",
            &["Manrope[wght].ttf"][..],
            &[
                "Fraunces[SOFT,WONK,opsz,wght].ttf",
                "NotoSansCJK-Regular.ttc",
            ][..],
        ),
        (
            "latin-only",
            &["Manrope[wght].ttf", "Fraunces[SOFT,WONK,opsz,wght].ttf"][..],
            &["NotoSansCJK-Regular.ttc"][..],
        ),
    ] {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "pf-default-fonts-partial-{tag}-{}-{nanos}",
            std::process::id(),
        ));
        std::fs::create_dir_all(&dir).expect("create partial font dir");
        for stub in stubs {
            std::fs::write(dir.join(stub), b"").expect("write font stub");
        }

        std::env::set_var("PF_FONTS_DIR", &dir);
        let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let captured_hook = captured.clone();
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let text = info
                .payload()
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| {
                    info.payload()
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_owned())
                })
                .unwrap_or_default();
            *captured_hook.lock().expect("capture lock") = text;
        }));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = pf_text_raster::pocketforge_default_fonts();
        }));
        std::panic::set_hook(prev);
        std::env::remove_var("PF_FONTS_DIR");

        // Cleanup: remove each stub then the dir (no recursive delete, no rm -rf).
        for stub in stubs {
            let _ = std::fs::remove_file(dir.join(stub));
        }
        let _ = std::fs::remove_dir(&dir);

        assert!(
            outcome.is_err(),
            "case {tag}: a partial PF_FONTS_DIR override must fail closed, not substitute another face"
        );
        let message = captured.lock().expect("capture lock").clone();
        assert!(
            message.contains("missing canonical font"),
            "case {tag}: rejection must state the missing-canonical reason; got: {message}"
        );
        for name in expected_named {
            assert!(
                message.contains(name),
                "case {tag}: rejection must name the missing face {name}; got: {message}"
            );
        }
    }
}
