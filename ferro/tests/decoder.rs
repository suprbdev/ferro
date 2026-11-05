mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::sync::Arc;
use std::sync::mpsc::channel;
use std::thread;
use std::time::Duration;

use common::{cases, quiet_ferro_panics};
use ferro::codec::{Decoder, Encoder, HashSink, field_tag};
use ferro::{Data, List, Map, Ptr, Set};
use proptest::prelude::*;

struct Tracking;

thread_local! {
    static LARGEST: Cell<usize> = const { Cell::new(0) };
}

fn note(size: usize) {
    let _ = LARGEST.try_with(|largest| largest.set(largest.get().max(size)));
}

// SAFETY: every method forwards to `System` with the caller's layout, pointer and size unchanged
// and returns what `System` returns, so `System`'s guarantees carry over. Besides that it only
// updates the const-initialized thread-local cell `LARGEST`, through `try_with`, which neither
// allocates nor panics, even while the thread's locals are being destroyed.
unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        // SAFETY: the caller meets `GlobalAlloc::alloc`'s contract for `layout`, which is passed on
        // to `System` unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller guarantees that `ptr` was allocated by this allocator with `layout`,
        // and every block this allocator hands out came from `System` with that layout.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        // SAFETY: the caller meets `GlobalAlloc::alloc_zeroed`'s contract for `layout`, which is
        // passed on to `System` unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note(new_size);
        // SAFETY: the caller guarantees that `ptr` was allocated by this allocator with `layout`
        // and that `new_size` is a valid new size for it. The block came from `System` with that
        // layout, and all three are passed on unchanged.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Tracking = Tracking;

fn fake_links(count: usize) -> Vec<Ptr> {
    (0..count)
        .map(|n| Ptr {
            hash: [n as u8; ferro::codec::HASH_BYTES],
            offset: 8192 + n as u64,
        })
        .collect()
}

fn decode_any<T: Data>(bytes: &[u8], links: &[Ptr]) {
    let mut decoder = Decoder::new(bytes, links, None);
    let _ = T::decode(&mut decoder);
}

fn decode_fields(bytes: &[u8], links: &[Ptr]) {
    let mut decoder = Decoder::new(bytes, links, None);
    let _ = decoder.fields(|tag, field| match tag % 4 {
        0 => u64::decode(field).map(|_| ()),
        1 => String::decode(field).map(|_| ()),
        2 => Vec::<u8>::decode(field).map(|_| ()),
        _ => field
            .fields(|_, inner| Option::<u32>::decode(inner).map(|_| ()))
            .map(|_| ()),
    });
}

fn decode_everything(bytes: &[u8], links: &[Ptr]) {
    decode_any::<()>(bytes, links);
    decode_any::<bool>(bytes, links);
    decode_any::<u8>(bytes, links);
    decode_any::<u16>(bytes, links);
    decode_any::<u32>(bytes, links);
    decode_any::<u64>(bytes, links);
    decode_any::<usize>(bytes, links);
    decode_any::<u128>(bytes, links);
    decode_any::<i8>(bytes, links);
    decode_any::<i16>(bytes, links);
    decode_any::<i32>(bytes, links);
    decode_any::<i64>(bytes, links);
    decode_any::<isize>(bytes, links);
    decode_any::<i128>(bytes, links);
    decode_any::<f32>(bytes, links);
    decode_any::<f64>(bytes, links);
    decode_any::<char>(bytes, links);
    decode_any::<String>(bytes, links);
    decode_any::<Ptr>(bytes, links);
    decode_any::<Option<u64>>(bytes, links);
    decode_any::<Option<Ptr>>(bytes, links);
    decode_any::<Option<Option<String>>>(bytes, links);
    decode_any::<Vec<u8>>(bytes, links);
    decode_any::<Vec<u64>>(bytes, links);
    decode_any::<Vec<String>>(bytes, links);
    decode_any::<Vec<Vec<u16>>>(bytes, links);
    decode_any::<Vec<Option<bool>>>(bytes, links);
    decode_any::<Vec<Ptr>>(bytes, links);
    decode_any::<[u32; 4]>(bytes, links);
    decode_any::<[String; 3]>(bytes, links);
    decode_any::<Box<u64>>(bytes, links);
    decode_any::<Arc<String>>(bytes, links);
    decode_any::<BTreeMap<u32, String>>(bytes, links);
    decode_any::<BTreeMap<String, Vec<u8>>>(bytes, links);
    decode_any::<BTreeSet<u64>>(bytes, links);
    decode_any::<BTreeSet<String>>(bytes, links);
    decode_any::<(u8,)>(bytes, links);
    decode_any::<(u8, String)>(bytes, links);
    decode_any::<(u8, String, u64)>(bytes, links);
    decode_any::<(u8, String, u64, bool)>(bytes, links);
    decode_any::<(u8, String, u64, bool, char)>(bytes, links);
    decode_any::<(u8, String, u64, bool, char, f32)>(bytes, links);
    decode_any::<Map<u64, String>>(bytes, links);
    decode_any::<Set<String>>(bytes, links);
    decode_any::<List<u64>>(bytes, links);
    decode_any::<Vec<Map<u64, String>>>(bytes, links);
    decode_any::<ferro::Commit>(bytes, links);
    decode_fields(bytes, links);
}

fn bytes_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        prop::collection::vec(any::<u8>(), 0..64),
        prop::collection::vec(
            prop_oneof![Just(0xFFu8), Just(0x80), Just(0x00), Just(0x01), Just(0x7F)],
            0..64
        ),
        prop::collection::vec(any::<u8>(), 0..4096),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(4000), failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn arbitrary_bytes_never_panic(bytes in bytes_strategy(), links in 0..4usize) {
        decode_everything(&bytes, &fake_links(links));
    }
}

fn roundtrip<T: Data + PartialEq + Debug>(value: T) {
    let mut sink = HashSink;
    let mut encoder = Encoder::new(&mut sink);
    value.encode(&mut encoder).unwrap();
    let object = encoder.into_object();
    let mut decoder = Decoder::new(&object.body, &object.links, None);
    assert_eq!(T::decode(&mut decoder).unwrap(), value);
    assert_eq!(decoder.remaining(), 0);
    for cut in 0..object.body.len() {
        let mut short = Decoder::new(&object.body[..cut], &object.links, None);
        let _ = T::decode(&mut short);
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(600), failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn integers_round_trip(a in any::<u64>(), b in any::<i64>(), c in any::<i8>(), d in any::<u16>(), e in any::<i32>(), f in any::<u128>(), g in any::<i128>()) {
        roundtrip(a);
        roundtrip(b);
        roundtrip(c);
        roundtrip(d);
        roundtrip(e);
        roundtrip(f);
        roundtrip(g);
        roundtrip(a as usize);
        roundtrip(b as isize);
        roundtrip((a, b, c, d, e, f));
    }

    #[test]
    fn composites_round_trip(
        text in ".{0,40}",
        c in any::<char>(),
        items in prop::collection::vec(any::<Option<u32>>(), 0..20),
        map in prop::collection::btree_map(any::<u16>(), ".{0,8}", 0..12),
        set in prop::collection::btree_set(any::<i32>(), 0..12),
        array in any::<[u16; 5]>(),
        boolean in any::<bool>(),
    ) {
        roundtrip(text.clone());
        roundtrip(c);
        roundtrip(items.clone());
        roundtrip(map.clone());
        roundtrip(set.clone());
        roundtrip(array);
        roundtrip(boolean);
        roundtrip(Box::new(text.clone()));
        roundtrip(Arc::new(items));
        roundtrip((text, map, set));
    }
}

#[test]
fn small_i128_values_stay_small() {
    let mut sink = HashSink;
    let mut encoder = Encoder::new(&mut sink);
    (-1_000_000i128).encode(&mut encoder).unwrap();
    assert_eq!(encoder.bytes().len(), 4);
}

#[test]
fn edge_integers_round_trip() {
    for value in [0u64, 1, 127, 128, 16383, 16384, u64::MAX - 1, u64::MAX] {
        roundtrip(value);
    }
    for value in [0i64, -1, 1, i64::MIN, i64::MAX, -64, 63, -65, 64] {
        roundtrip(value);
    }
    roundtrip(i8::MIN);
    roundtrip(i16::MIN);
    roundtrip(i32::MIN);
    roundtrip(u128::MAX);
    for value in [
        0i128,
        1,
        -1,
        i128::from(i64::MIN) - 1,
        i128::from(u64::MAX) + 1,
        i128::MAX,
        i128::MIN,
    ] {
        roundtrip(value);
    }
    roundtrip(char::MAX);
    roundtrip(f64::MIN_POSITIVE.to_bits());
    roundtrip(f32::INFINITY);
    roundtrip(f64::NEG_INFINITY);
}

#[test]
fn overlong_varints_are_rejected() {
    let cases: [&[u8]; 5] = [
        &[0xFF; 10],
        &[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x02],
        &[
            0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01,
        ],
        &[0xFF; 3],
        &[],
    ];
    for bytes in cases {
        let mut decoder = Decoder::new(bytes, &[], None);
        assert!(decoder.varint().is_err(), "{bytes:?}");
    }
    let mut decoder = Decoder::new(
        &[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01],
        &[],
        None,
    );
    assert_eq!(decoder.varint().unwrap(), u64::MAX);
}

#[test]
fn out_of_range_narrow_integers_are_rejected() {
    let mut sink = HashSink;
    let mut encoder = Encoder::new(&mut sink);
    70_000u32.encode(&mut encoder).unwrap();
    let bytes = encoder.bytes().to_vec();
    assert!(u16::decode(&mut Decoder::new(&bytes, &[], None)).is_err());
    let mut sink = HashSink;
    let mut encoder = Encoder::new(&mut sink);
    70_000i32.encode(&mut encoder).unwrap();
    let bytes = encoder.bytes().to_vec();
    assert!(i16::decode(&mut Decoder::new(&bytes, &[], None)).is_err());
    assert!(i8::decode(&mut Decoder::new(&bytes, &[], None)).is_err());
    assert!(char::decode(&mut Decoder::new(&[0x80, 0x80, 0x80, 0x10], &[], None)).is_err());
    assert!(bool::decode(&mut Decoder::new(&[2], &[], None)).is_err());
    assert!(Option::<u8>::decode(&mut Decoder::new(&[2], &[], None)).is_err());
    assert!(String::decode(&mut Decoder::new(&[2, 0xFF, 0xFF], &[], None)).is_err());
}

fn huge_length() -> Vec<u8> {
    vec![0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01]
}

fn finishes_quickly(label: &str, work: impl FnOnce() + Send + 'static) {
    let (done, wait) = channel();
    thread::spawn(move || {
        work();
        let _ = done.send(());
    });
    assert!(
        wait.recv_timeout(Duration::from_secs(10)).is_ok(),
        "{label} did not finish"
    );
}

#[test]
fn zero_width_items_cannot_stall_the_decoder() {
    finishes_quickly("Vec<()>", || {
        assert!(Vec::<()>::decode(&mut Decoder::new(&huge_length(), &[], None)).is_err());
    });
    finishes_quickly("Vec<[u8; 0]>", || {
        assert!(Vec::<[u8; 0]>::decode(&mut Decoder::new(&huge_length(), &[], None)).is_err());
    });
    finishes_quickly("Vec<Box<()>>", || {
        assert!(Vec::<Box<()>>::decode(&mut Decoder::new(&huge_length(), &[], None)).is_err());
    });
    finishes_quickly("BTreeSet<()>", || {
        assert!(BTreeSet::<()>::decode(&mut Decoder::new(&huge_length(), &[], None)).is_err());
    });
    finishes_quickly("BTreeMap<(), ()>", || {
        assert!(BTreeMap::<(), ()>::decode(&mut Decoder::new(&huge_length(), &[], None)).is_err());
    });
}

#[test]
fn legitimate_zero_width_collections_still_round_trip() {
    roundtrip(vec![(); 1000]);
    roundtrip(vec![Box::new(()); 1000]);
    roundtrip(BTreeSet::from([()]));
    roundtrip(BTreeMap::from([((), ())]));
}

#[test]
fn large_declared_lengths_do_not_allocate_ahead_of_input() {
    let mut bytes = vec![0xFF, 0xFF, 0xFF, 0xFF, 0x7F];
    bytes.resize(4096, 0);
    let limit = 1 << 22;
    LARGEST.set(0);
    let mut decoder = Decoder::new(&bytes, &[], None);
    let result = Vec::<[u64; 512]>::decode(&mut decoder);
    assert!(result.is_err());
    let peak = LARGEST.get();
    assert!(
        peak < limit,
        "single allocation of {peak} bytes for 4 KiB of input"
    );
}

#[test]
fn field_records_with_bad_lengths_are_rejected() {
    let cases: [&[u8]; 4] = [
        &[1, 1, 0, 0xFF, 0x01],
        &[1, 1, 0, 3, 0xFF, 0x7F],
        &[0xFF, 0xFF, 0xFF, 0xFF, 0x0F],
        &[2, 1, 0, 0],
    ];
    for bytes in cases {
        let mut decoder = Decoder::new(bytes, &[], None);
        assert!(decoder.fields(|_, _| Ok(())).is_err(), "{bytes:?}");
    }
}

#[test]
fn field_links_stay_within_their_field() {
    let mut sink = HashSink;
    let mut encoder = Encoder::new(&mut sink);
    let links = fake_links(3);
    encoder.varint(2);
    encoder
        .field(field_tag("a"), |w| {
            w.varint(9);
            w.link(links[0]);
            w.link(links[1]);
            Ok(())
        })
        .unwrap();
    encoder
        .field(field_tag("b"), |w| {
            w.link(links[2]);
            Ok(())
        })
        .unwrap();
    let object = encoder.into_object();
    assert_eq!(object.links.len(), 3);
    let mut decoder = Decoder::new(&object.body, &object.links, None);
    let mut seen = Vec::new();
    decoder
        .fields(|tag, field| {
            if tag == field_tag("a") {
                assert_eq!(field.varint()?, 9);
                seen.push(field.link()?.offset);
                seen.push(field.link()?.offset);
                assert!(field.link().is_err());
            } else {
                seen.push(field.link()?.offset);
                assert!(field.link().is_err());
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(seen, [8192, 8193, 8194]);
    assert_eq!(decoder.remaining(), 0);
}

#[test]
fn map_decoded_without_a_store_fails_with_a_ferro_panic() {
    quiet_ferro_panics();
    let links = fake_links(1);
    let mut decoder = Decoder::new(&[5], &links, None);
    let map = Map::<u64, String>::decode(&mut decoder).unwrap();
    assert_eq!(map.len(), 5);
    assert!(matches!(map.try_get(&1), Err(ferro::Error::Detached)));
    match common::guarded(|| map.get(&1)) {
        common::Outcome::FerroPanic(_) => {}
        common::Outcome::Value(_) => panic!("detached read succeeded"),
    }
}

#[test]
fn field_tag_is_stable_and_spreads() {
    assert_eq!(field_tag("name"), field_tag("name"));
    assert_ne!(field_tag("name"), field_tag("nane"));
    let names: BTreeSet<u16> = (0..1000)
        .map(|n| field_tag(&format!("field_{n}")))
        .collect();
    assert!(names.len() > 980);
}
