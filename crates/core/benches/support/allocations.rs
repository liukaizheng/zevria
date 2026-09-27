//! This allocator exists only in the separately compiled benchmark executable.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
};

struct Counting;
static ACTIVE: AtomicBool = AtomicBool::new(false);
static CALLS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
#[global_allocator]
static ALLOCATOR: Counting = Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ACTIVE.load(Relaxed) {
            CALLS.fetch_add(1, Relaxed);
            BYTES.fetch_add(layout.size() as u64, Relaxed);
        }
        // SAFETY: forward the caller's layout unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: all pointers returned by this allocator originate in System.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if ACTIVE.load(Relaxed) {
            CALLS.fetch_add(1, Relaxed);
            BYTES.fetch_add(size as u64, Relaxed);
        }
        // SAFETY: forward the caller's allocation and requested size unchanged.
        unsafe { System.realloc(ptr, layout, size) }
    }
}
fn measure(label: &str, run: impl FnOnce()) {
    CALLS.store(0, Relaxed);
    BYTES.store(0, Relaxed);
    ACTIVE.store(true, Relaxed);
    run();
    ACTIVE.store(false, Relaxed);
    println!(
        "allocation {label}: calls={}, requested_bytes={}",
        CALLS.load(Relaxed),
        BYTES.load(Relaxed)
    );
}
pub fn run() {
    use zevria_core::transcript_bench::Workload;
    use zevria_core::transcript_bench::fixture;
    use zevria_core::transcript_bench::owned_snapshot;
    use zevria_transcript::transcript::TranscriptItem;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    for count in [10, 1000] {
        for bytes in [128, 8192] {
            for (kind, activated, skill) in [
                ("message", false, false),
                ("activation", false, true),
                ("reapplication", true, true),
            ] {
                let session =
                    zevria_core::benchmark::AdmissionFixture::prompt(count, bytes, activated);
                for (anchor, edit) in [("append", false), ("edit", true)] {
                    measure(
                        &format!("admit_prompt/{anchor}/{kind}/{count}/{bytes}"),
                        || session.admit(edit, skill),
                    );
                }
            }
        }
    }
    for workload in [Workload::Plain, Workload::Replay, Workload::Mixed] {
        for count in [10, 100, 1000] {
            let f = fixture(workload, count, 8192);
            let label = format!("{}/{count}/8192", workload.name());
            measure(&format!("{label}/snapshot"), || {
                std::hint::black_box(owned_snapshot(&f.items));
            });
            measure(&format!("{label}/encode"), || {
                for item in &f.items {
                    std::hint::black_box(serde_json::to_vec(item).unwrap());
                }
            });
            measure(&format!("{label}/decode"), || {
                for line in &f.lines {
                    std::hint::black_box(serde_json::from_slice::<TranscriptItem>(line).unwrap());
                }
            });
            measure(&format!("{label}/validate"), || {
                zevria_transcript::SessionReplayError::validate(&f.items).unwrap()
            });
            measure(&format!("{label}/restore"), || {
                std::hint::black_box(zevria_transcript::reconstruct_transcript(&f.items));
            });
        }
    }
}
