mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::fast;
use ferro::{Data, Db, Map, Outcome, Record, Snapshot, Table};

struct Counted;

static ALLOCATED: AtomicUsize = AtomicUsize::new(0);

static ALONE: Mutex<()> = Mutex::new(());

// SAFETY: every method forwards to `System` with the caller's layout, pointer and size unchanged
// and returns what `System` returns, so `System`'s guarantees carry over. Besides that it only adds
// to the atomic `ALLOCATED`, which neither allocates nor panics.
unsafe impl GlobalAlloc for Counted {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        // SAFETY: the caller meets `GlobalAlloc::alloc`'s contract for `layout`, which is passed on
        // to `System` unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: the caller guarantees that `pointer` was allocated by this allocator with
        // `layout`, and every block this allocator hands out came from `System` with that layout.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        // SAFETY: the caller meets `GlobalAlloc::alloc_zeroed`'s contract for `layout`, which is
        // passed on to `System` unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOCATED.fetch_add(size, Ordering::Relaxed);
        // SAFETY: the caller guarantees that `pointer` was allocated by this allocator with
        // `layout` and that `size` is a valid new size for it. The block came from `System` with
        // that layout, and all three are passed on unchanged.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counted = Counted;

const MIB: usize = 1 << 20;

#[derive(Data, Record, Clone, Debug, PartialEq)]
struct Row {
    #[key]
    id: u64,
    #[index]
    n: u32,
    #[unique]
    email: u64,
    label: String,
}

fn row(id: u64) -> Row {
    Row {
        id,
        n: (id % 1000) as u32,
        email: id * 7 + 1,
        label: format!("row number {id:>12}"),
    }
}

#[derive(Data, Clone, Default)]
struct Nested {
    teams: Map<u64, Table<Row>>,
}

#[derive(Data, Clone, Default)]
struct Top {
    team: Table<Row>,
}

#[derive(Data, Clone, Default)]
struct AsideFirst {
    aside: Option<Table<Row>>,
    main: Table<Row>,
}

#[derive(Data, Clone, Default)]
struct AsideLast {
    main: Table<Row>,
    zaside: Option<Table<Row>>,
}

#[derive(Data, Clone, Default)]
struct Tenants {
    tenants: Map<u64, Map<u64, Table<Row>>>,
}

const TEAMS: u64 = 16;
const TEAM_ROWS: u64 = 2_000;
const EDITS: u64 = 10;

fn unmerged<T>(_: &mut T, _: &Snapshot<T>, _: &Snapshot<T>) -> (String, ()) {
    ("merge".to_string(), ())
}

fn allocated<R>(run: impl FnOnce() -> R) -> (R, usize) {
    let before = ALLOCATED.load(Ordering::Relaxed);
    let out = run();
    (out, ALLOCATED.load(Ordering::Relaxed) - before)
}

fn pulled<T: Data + Default>(
    dir: &std::path::Path,
    name: &str,
    fill: impl Fn(&mut T),
    edit: impl Fn(&mut T, u64),
) -> (usize, usize) {
    let sender: Db<T> = fast()
        .open(dir.join(format!("{name}-sender.ferro")))
        .unwrap();
    let receiver: Db<T> = fast()
        .open(dir.join(format!("{name}-receiver.ferro")))
        .unwrap();
    sender.update(|root| fill(root)).unwrap();
    let bundle = sender.bundle(&receiver.have().unwrap()).unwrap();
    receiver.receive("sender", &bundle, unmerged).unwrap();
    for at in 0..EDITS {
        sender.update(|root| edit(root, at)).unwrap();
    }
    let bundle = sender.bundle(&receiver.have().unwrap()).unwrap();
    let (pulled, pull) = allocated(|| receiver.receive("sender", &bundle, unmerged).unwrap());
    assert_eq!(pulled.outcome, Outcome::FastForward, "{name}");
    assert_eq!(receiver.head_hash(), sender.head_hash(), "{name}");
    let (_, verify) = allocated(|| receiver.verify_typed().unwrap());
    (pull, verify)
}

#[test]
fn a_table_is_checked_against_the_table_at_its_place_in_the_version_before() {
    let _alone = ALONE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let (top_pull, top_verify) = pulled::<Top>(
        dir.path(),
        "top",
        |root| {
            for id in 0..TEAMS * TEAM_ROWS {
                root.team.insert(row(id));
            }
        },
        |root, at| {
            root.team.update(&(at * 97 % (TEAMS * TEAM_ROWS)), |row| {
                row.label = format!("edit {at}")
            });
        },
    );
    let (nested_pull, nested_verify) = pulled::<Nested>(
        dir.path(),
        "nested",
        |root| {
            for team in 0..TEAMS {
                let mut table: Table<Row> = Table::new();
                for id in 0..TEAM_ROWS {
                    table.insert(row(team * TEAM_ROWS + id));
                }
                root.teams.insert(team, table);
            }
        },
        |root, at| {
            root.teams.update(&(at % TEAMS), |table| {
                table.update(&((at % TEAMS) * TEAM_ROWS + at), |row| {
                    row.label = format!("edit {at}")
                });
            });
        },
    );
    assert!(
        nested_pull < 2 * top_pull + 2 * MIB,
        "a pull of {EDITS} edits to tables nested in a map allocated {} KiB, at the root {} KiB",
        nested_pull >> 10,
        top_pull >> 10
    );
    assert!(
        nested_verify < top_verify + top_verify / 8 + 2 * MIB,
        "verify_typed of tables nested in a map allocated {} KiB, at the root {} KiB",
        nested_verify >> 10,
        top_verify >> 10
    );

    let toggled = |aside: &mut Option<Table<Row>>, at: u64| {
        *aside = at.is_multiple_of(2).then(|| {
            let mut table: Table<Row> = Table::new();
            table.insert(row(1_000_000 + at));
            table
        });
    };
    let fill = |main: &mut Table<Row>| {
        for id in 0..TEAMS * TEAM_ROWS {
            main.insert(row(id));
        }
    };
    let (first, _) = pulled::<AsideFirst>(
        dir.path(),
        "first",
        |root| fill(&mut root.main),
        |root, at| toggled(&mut root.aside, at),
    );
    let (last, _) = pulled::<AsideLast>(
        dir.path(),
        "last",
        |root| fill(&mut root.main),
        |root, at| toggled(&mut root.zaside, at),
    );
    assert!(
        first < 2 * last + 2 * MIB,
        "toggling a table placed before another allocated {} KiB, placed after it {} KiB",
        first >> 10,
        last >> 10
    );
}

#[test]
fn maps_cloned_from_one_template_pair_every_table_they_hold() {
    let _alone = ALONE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let tenants = 8;
    let fill = |apart: bool| {
        move |root: &mut Tenants| {
            let mut table: Table<Row> = Table::new();
            for id in 0..TEAMS * TEAM_ROWS {
                table.insert(row(id));
            }
            let mut template: Map<u64, Table<Row>> = Map::new();
            template.insert(1, table);
            for tenant in 0..tenants {
                let mut inner = template.clone();
                if apart {
                    inner.insert(1_000 + tenant, Table::new());
                }
                root.tenants.insert(tenant, inner);
            }
        }
    };
    let edit = |root: &mut Tenants, at: u64| {
        for tenant in 0..tenants {
            root.tenants.update(&tenant, |inner| {
                inner.update(&1, |table| {
                    table.update(&(tenant * 997 + at), |row| {
                        row.label = format!("tenant {tenant} edit {at}")
                    });
                });
            });
        }
    };
    let (template, template_verify) = pulled::<Tenants>(dir.path(), "template", fill(false), edit);
    let (apart, apart_verify) = pulled::<Tenants>(dir.path(), "apart", fill(true), edit);
    assert!(
        template < 2 * apart + 2 * MIB,
        "a pull of edits to {tenants} tables in maps cloned from one allocated {} KiB, in maps \
         apart {} KiB",
        template >> 10,
        apart >> 10
    );
    assert!(
        template_verify < apart_verify + apart_verify / 8 + 2 * MIB,
        "verify_typed of {tenants} tables in maps cloned from one allocated {} KiB, in maps \
         apart {} KiB",
        template_verify >> 10,
        apart_verify >> 10
    );
}

#[test]
fn maps_set_to_clones_of_another_map_open_its_nodes_a_bounded_number_of_times() {
    let _alone = ALONE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let (tenants, entries) = (64u64, 4_000u64);
    let one = |id: u64| {
        let mut table: Table<Row> = Table::new();
        table.insert(row(id));
        table
    };
    let first: Map<u64, Table<Row>> = (0..entries).map(|id| (id, one(id))).collect();
    let second: Map<u64, Table<Row>> = (0..entries).map(|id| (id, one(1_000_000 + id))).collect();
    let sender: Db<Tenants> = fast().open(dir.path().join("sender.ferro")).unwrap();
    let receiver: Db<Tenants> = fast().open(dir.path().join("receiver.ferro")).unwrap();
    sender
        .update(|root| {
            for tenant in 0..tenants {
                root.tenants.insert(tenant, first.clone());
            }
            root.tenants.insert(tenants, second.clone());
        })
        .unwrap();
    let bundle = sender.bundle(&receiver.have().unwrap()).unwrap();
    receiver.receive("sender", &bundle, unmerged).unwrap();
    sender
        .update(|root| {
            for tenant in 0..tenants {
                let mut inner = second.clone();
                inner.insert(entries + tenant, one(2_000_000 + tenant));
                root.tenants.insert(tenant, inner);
            }
        })
        .unwrap();
    let bundle = sender.bundle(&receiver.have().unwrap()).unwrap();
    let carried = bundle.as_bytes().len();
    let (pulled, pull) = allocated(|| receiver.receive("sender", &bundle, unmerged).unwrap());
    assert_eq!(pulled.outcome, Outcome::FastForward);
    assert_eq!(receiver.head_hash(), sender.head_hash());
    println!("{tenants} tenants set to clones of another map: {carried} bytes, {pull} allocated");
    assert!(
        pull < 3 * MIB * tenants as usize,
        "a pull of {carried} bytes that set {tenants} maps cloned from one to clones of another \
         allocated {} KiB",
        pull >> 10
    );
}
