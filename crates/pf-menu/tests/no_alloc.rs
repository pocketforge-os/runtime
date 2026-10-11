use pf_menu::{
    Bind, EngineOptions, Item, ItemKind, Menu, MenuEngine, MenuInput, MenuValue, Msg, Page, PageId,
    PrefsSchemas, Section, FORMAT_V1,
};
use pf_ports::MonotonicTime;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct CountingAllocator;
static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn menu() -> Menu {
    let toggle = Item::new(
        "toggle",
        Msg::text("Toggle"),
        ItemKind::Toggle {
            bind: Bind::Host("toggle".into()),
        },
    );
    Menu {
        format: FORMAT_V1.into(),
        id: "allocation".into(),
        root: PageId("root".into()),
        pages: vec![Page {
            id: PageId("root".into()),
            title: Msg::text("Root"),
            tabs: vec![],
            primary: None,
            sections: vec![Section {
                header: None,
                footer: None,
                items: vec![toggle],
            }],
        }],
    }
}

#[test]
fn reducer_input_path_does_not_allocate() {
    let mut engine = MenuEngine::new(menu(), &PrefsSchemas, EngineOptions::default()).unwrap();
    let toggle = engine.focus().unwrap();
    engine.set_value(toggle, MenuValue::Bool(false)).unwrap();
    let inputs = [
        MenuInput::Activate,
        MenuInput::Down,
        MenuInput::Up,
        MenuInput::Left,
        MenuInput::Right,
        MenuInput::Back,
        MenuInput::Tick,
    ];

    ALLOCS.store(0, Ordering::SeqCst);
    COUNTING.store(true, Ordering::SeqCst);
    for (step, input) in inputs.into_iter().cycle().take(10_000).enumerate() {
        std::hint::black_box(
            engine.input(input, MonotonicTime::from_nanos(step as u64 * 10_000_000)),
        );
    }
    COUNTING.store(false, Ordering::SeqCst);
    assert_eq!(ALLOCS.load(Ordering::SeqCst), 0);
}
