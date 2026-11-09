mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::sync::Arc;
use std::sync::mpsc::channel;
use std::thread;
use std::time::Duration;

use common::{cases, fast, quiet_ferro_panics};
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
    let cases: [&[u8]; 9] = [
        &[0xFF; 10],
        &[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x02],
        &[
            0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01,
        ],
        &[0xFF; 3],
        &[],
        &[0x80, 0x00],
        &[0x85, 0x80, 0x00],
        &[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00],
        &[0x80, 0x80, 0x00],
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
    let mut decoder = Decoder::new(
        &[
            0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01, 0x80, 0x01, 0x00,
        ],
        &[],
        None,
    );
    assert_eq!(decoder.varint().unwrap(), 1 << 63);
    assert_eq!(decoder.varint().unwrap(), 128);
    assert_eq!(decoder.varint().unwrap(), 0);
}

fn varint_as_written(bytes: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (index, &byte) in bytes.iter().enumerate().take(10) {
        if index == 9 && byte > 1 {
            return None;
        }
        value |= u64::from(byte & 0x7f) << (7 * index);
        if byte & 0x80 == 0 {
            return (byte != 0 || index == 0).then_some((value, index + 1));
        }
    }
    None
}

fn agrees_with_written_form(bytes: &[u8]) {
    let mut decoder = Decoder::new(bytes, &[], None);
    match (decoder.varint(), varint_as_written(bytes)) {
        (Ok(value), Some((wanted, len))) => {
            assert_eq!(value, wanted, "{bytes:02x?}");
            assert_eq!(decoder.remaining(), bytes.len() - len, "{bytes:02x?}");
        }
        (Err(_), None) => {}
        (read, wanted) => panic!("{bytes:02x?}: read {read:?}, wanted {wanted:?}"),
    }
}

#[test]
fn varints_of_nine_and_ten_bytes_read_only_their_shortest_form() {
    let prefixes: [[u8; 8]; 4] = [
        [0x80; 8],
        [0xff; 8],
        [0x81, 0xfe, 0x80, 0xc3, 0x9a, 0xff, 0x80, 0xb7],
        [0xff, 0x80, 0xff, 0x80, 0xff, 0x80, 0xff, 0x80],
    ];
    for prefix in prefixes {
        agrees_with_written_form(&prefix);
        for ninth in 0..=255u8 {
            let mut bytes = prefix.to_vec();
            bytes.push(ninth);
            agrees_with_written_form(&bytes);
            for tenth in 0..=255u8 {
                bytes.truncate(9);
                bytes.push(tenth);
                agrees_with_written_form(&bytes);
                bytes.extend_from_slice(&[0x01, 0x80]);
                agrees_with_written_form(&bytes);
            }
        }
    }
    let mut rng = common::Rng::new(19);
    for _ in 0..cases(200_000) {
        let len = rng.below(13) as usize;
        let bytes: Vec<u8> = (0..len)
            .map(|_| match rng.below(4) {
                0 => rng.below(256) as u8,
                1 => 0x80 | rng.below(128) as u8,
                2 => [0x00, 0x01, 0x7f, 0x80, 0x81, 0xff][rng.below(6) as usize],
                _ => 0x80,
            })
            .collect();
        agrees_with_written_form(&bytes);
    }
    for value in [
        1 << 56,
        (1 << 63) - 1,
        1 << 63,
        u64::MAX,
        0x0123_4567_89ab_cdef,
    ] {
        let bytes = varint_bytes(value);
        assert!(bytes.len() >= 9);
        let mut decoder = Decoder::new(&bytes, &[], None);
        assert_eq!(decoder.varint().unwrap(), value);
        assert_eq!(decoder.remaining(), 0);
    }
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
    assert!(field_tag("b") < field_tag("a"));
    encoder.varint(2);
    encoder
        .field(field_tag("b"), |w| {
            w.varint(9);
            w.link(links[0]);
            w.link(links[1]);
            Ok(())
        })
        .unwrap();
    encoder
        .field(field_tag("a"), |w| {
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
            if tag == field_tag("b") {
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

fn body_of<T: Data>(value: &T) -> Vec<u8> {
    let mut sink = HashSink;
    let mut encoder = Encoder::new(&mut sink);
    value.encode(&mut encoder).unwrap();
    encoder.into_object().body
}

fn refused<T: Data + Debug>(bytes: &[u8]) {
    let mut decoder = Decoder::new(bytes, &[], None);
    let decoded = T::decode(&mut decoder);
    assert!(
        matches!(decoded, Err(ferro::Error::Decode(_))),
        "{} decoded {bytes:02x?} as {decoded:?}",
        std::any::type_name::<T>()
    );
    let mut visitor = Decoder::new(bytes, &[], None);
    assert!(
        matches!(T::visit(&mut visitor), Err(ferro::Error::Decode(_))),
        "{} visited {bytes:02x?}",
        std::any::type_name::<T>()
    );
}

#[test]
fn a_value_is_read_only_from_the_bytes_its_encoder_writes() {
    refused::<u64>(&[0x80, 0x00]);
    refused::<u32>(&[0x85, 0x80, 0x00]);
    refused::<i64>(&[0x81, 0x00]);
    refused::<char>(&[0xC1, 0x00]);
    refused::<i128>(&[0x02, 0x80, 0x00]);
    refused::<String>(&[0x81, 0x00, b'a']);
    refused::<Vec<u8>>(&[0x81, 0x00, 7]);
    refused::<ferro::Bytes>(&[0x81, 0x00, 7]);
    refused::<BTreeMap<u8, u8>>(&[2, 1, 5, 1, 6]);
    refused::<BTreeMap<u8, u8>>(&[3, 1, 5, 2, 6, 1, 7]);
    refused::<BTreeSet<u8>>(&[2, 4, 4]);
    refused::<BTreeSet<u8>>(&[3, 4, 3, 4]);
    refused::<BTreeSet<String>>(&[2, 1, b'a', 1, b'a']);
    let repeated: Vec<u32> = (0..40).chain([17]).collect();
    refused::<BTreeSet<u32>>(&body_of(&repeated));
    assert_eq!(
        body_of(&BTreeMap::from([(1u8, 5u8), (2, 6)])),
        [2, 1, 5, 2, 6]
    );
}

fn reads_sorted<T: Data + Debug + PartialEq>(bytes: &[u8], expected: T) {
    let mut decoder = Decoder::new(bytes, &[], None);
    assert_eq!(T::decode(&mut decoder).unwrap(), expected);
    assert_eq!(decoder.remaining(), 0);
    let mut visitor = Decoder::new(bytes, &[], None);
    T::visit(&mut visitor).unwrap();
    assert_eq!(visitor.remaining(), 0);
    let canonical = body_of(&expected);
    assert_ne!(canonical, bytes);
    assert_eq!(
        T::decode(&mut Decoder::new(&canonical, &[], None)).unwrap(),
        expected
    );
}

#[test]
fn btree_keys_stored_in_any_order_read_sorted() {
    reads_sorted(&[2, 2, 0, 1, 0], BTreeMap::from([(1u8, 0u8), (2, 0)]));
    reads_sorted(&[2, 4, 3], BTreeSet::from([3u8, 4]));
    reads_sorted(
        &[2, 1, b'b', 1, b'a'],
        BTreeSet::from(["a".to_string(), "b".to_string()]),
    );
    let shuffled: Vec<(u32, String)> = (0..300u32)
        .map(|n| (n * 7 % 300, format!("v{n}")))
        .collect();
    reads_sorted(
        &body_of(&shuffled),
        shuffled.iter().cloned().collect::<BTreeMap<_, _>>(),
    );
    let late: Vec<u32> = (1..60).chain([0]).collect();
    reads_sorted(
        &body_of(&late),
        late.iter().copied().collect::<BTreeSet<_>>(),
    );
}

type Mixed = (
    Vec<Option<u32>>,
    BTreeMap<u16, String>,
    BTreeSet<i32>,
    [u16; 3],
    char,
    i128,
    (bool, u8, i16),
    ferro::Bytes,
);

type Stored = (
    Vec<Option<u32>>,
    Vec<(u16, String)>,
    Vec<i32>,
    [u16; 3],
    char,
    i128,
    (bool, u8, i16),
    ferro::Bytes,
);

fn mixed(rng: &mut common::Rng) -> Mixed {
    (
        (0..rng.below(6))
            .map(|_| rng.chance(70).then(|| (rng.next() >> rng.below(64)) as u32))
            .collect(),
        (0..rng.below(5))
            .map(|_| (rng.next() as u16, "é".repeat(rng.below(3) as usize)))
            .collect(),
        (0..rng.below(5))
            .map(|_| (rng.next() >> rng.below(64)) as i32)
            .collect(),
        [rng.next() as u16, (rng.next() >> 60) as u16, 0],
        char::from_u32(rng.below(0xD800) as u32).unwrap(),
        i128::from(rng.next() as i64) << rng.below(64),
        (rng.chance(50), rng.next() as u8, rng.next() as i16),
        ferro::Bytes::from(vec![rng.next() as u8; rng.below(4) as usize]),
    )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(4000), failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn what_decodes_whole_encodes_back_to_the_same_bytes_with_its_keys_sorted(seed in any::<u64>()) {
        let mut rng = common::Rng::new(seed);
        let mut bytes = body_of(&mixed(&mut rng));
        for _ in 0..1 + rng.below(3) {
            if bytes.is_empty() {
                break;
            }
            let at = rng.below(bytes.len() as u64) as usize;
            match rng.below(4) {
                0 => bytes[at] ^= 1 << rng.below(8),
                1 => bytes[at] = rng.next() as u8,
                2 => bytes.truncate(at),
                _ => bytes.insert(at, rng.next() as u8),
            }
        }
        let mut decoder = Decoder::new(&bytes, &[], None);
        let decoded = Mixed::decode(&mut decoder).map(|value| (value, decoder.remaining()));
        let mut visitor = Decoder::new(&bytes, &[], None);
        let visited = Mixed::visit(&mut visitor).map(|()| visitor.remaining());
        prop_assert_eq!(decoded.is_ok(), visited.is_ok());
        if let (Ok((value, rest)), Ok(left)) = (decoded, visited) {
            prop_assert_eq!(rest, left);
            if rest == 0 {
                let mut stored = Stored::decode(&mut Decoder::new(&bytes, &[], None)).unwrap();
                stored.1.sort_by_key(|(key, _)| *key);
                stored.2.sort_unstable();
                prop_assert_eq!(body_of(&value), body_of(&stored));
            }
        }
    }
}

#[derive(Clone, Default, Debug)]
struct Forged(Vec<u8>);

impl Data for Forged {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        w.raw(&self.0);
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        Ok(Forged(r.take(r.remaining())?.to_vec()))
    }
}

fn forged_file(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("forged.ferro");
    let db = ferro::Db::<Forged>::open(&path).unwrap();
    db.update(|root| *root = Forged(bytes.to_vec())).unwrap();
    drop(db);
    (dir, path)
}

fn forged_as<T: Data + Default + Debug>(bytes: &[u8]) {
    let (_dir, path) = forged_file(bytes);
    match ferro::Db::<T>::open(&path) {
        Err(ferro::Error::Decode(_)) => {}
        Err(other) => panic!("opening a forged root failed with {other}"),
        Ok(db) => {
            let value = db.read();
            panic!("a forged root {bytes:02x?} read as {:?}", *value);
        }
    }
}

#[test]
fn a_root_written_in_a_form_no_encoder_writes_fails_to_open() {
    forged_as::<u64>(&[0x80, 0x00]);
    forged_as::<BTreeMap<u8, u8>>(&[2, 1, 5, 1, 6]);
    forged_as::<BTreeSet<String>>(&[2, 1, b'a', 1, b'a']);
    forged_as::<Vec<String>>(&[1, 0x81, 0x00, b'a']);
}

#[test]
fn a_root_with_bytes_after_its_value_fails_a_typed_verify() {
    let (_dir, path) = forged_file(&[0x05, 0x07]);
    let db = ferro::Db::<u64>::open(&path).unwrap();
    assert_eq!(*db.read(), 5);
    assert!(matches!(
        db.verify_typed(),
        Err(ferro::Error::Corrupt(message)) if message.contains("left after the value")
    ));
}

fn forged_map(value: [u8; 2]) -> (tempfile::TempDir, ferro::Db<Map<u64, u64>>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("forged.ferro");
    let db = ferro::Db::<Map<u64, [u8; 2]>>::open(&path).unwrap();
    db.update(|map| {
        map.insert(3, value);
    })
    .unwrap();
    drop(db);
    let db = ferro::Db::<Map<u64, u64>>::open(&path).unwrap();
    (dir, db)
}

#[test]
fn a_map_node_holding_a_value_in_a_form_no_encoder_writes_fails_to_read() {
    let (_dir, db) = forged_map([0x80, 0x00]);
    assert!(matches!(
        db.read().try_get(&3),
        Err(ferro::Error::Decode(_))
    ));
    assert!(db.verify_typed().is_err());
}

#[derive(Clone, Default)]
struct Obj {
    body: Vec<u8>,
    links: Vec<usize>,
}

#[derive(Clone, Default)]
struct Objects(Arc<Vec<Obj>>);

fn write_obj(w: &mut Encoder<'_>, obj: &Obj, ptrs: &[Ptr]) {
    w.raw(&obj.body);
    for &link in &obj.links {
        w.link(ptrs[link]);
    }
}

impl Data for Objects {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        let Some((root, children)) = self.0.split_last() else {
            return Ok(());
        };
        let mut ptrs = Vec::new();
        for obj in children {
            let ptr = w.child(|inner| {
                write_obj(inner, obj, &ptrs);
                Ok(())
            })?;
            ptrs.push(ptr);
        }
        write_obj(w, root, &ptrs);
        Ok(())
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        r.take(r.remaining())?;
        while r.link().is_ok() {}
        Ok(Objects::default())
    }
}

#[derive(Default)]
struct Forge(Vec<Obj>);

impl Forge {
    fn add(&mut self, obj: Obj) -> usize {
        self.0.push(obj);
        self.0.len() - 1
    }

    fn file(mut self, root: Obj) -> (tempfile::TempDir, std::path::PathBuf) {
        self.0.push(root);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("forged.ferro");
        let db = ferro::Db::<Objects>::open(&path).unwrap();
        let objects = Objects(Arc::new(self.0));
        db.update(|root| *root = objects).unwrap();
        drop(db);
        (dir, path)
    }
}

fn varint_bytes(value: u64) -> Vec<u8> {
    let mut sink = HashSink;
    let mut encoder = Encoder::new(&mut sink);
    encoder.varint(value);
    encoder.bytes().to_vec()
}

enum Slot {
    Inline(Vec<u8>),
    Apart(usize, Vec<u8>),
}

struct Child {
    at: usize,
    count: u64,
    summary: Vec<u8>,
    obj: usize,
}

fn forged_node<K: Data>(level: u8, keys: &[K], slots: Vec<Slot>, children: Vec<Child>) -> Obj {
    let mut body = vec![level];
    body.extend(varint_bytes(keys.len() as u64));
    for key in keys {
        body.extend(body_of(key));
    }
    let mut links = Vec::new();
    for slot in slots {
        match slot {
            Slot::Inline(value) => {
                body.push(0);
                body.extend(value);
            }
            Slot::Apart(obj, summary) => {
                body.push(1);
                body.extend(summary);
                links.push(obj);
            }
        }
    }
    for position in 0..=keys.len() {
        match children.iter().find(|child| child.at == position) {
            Some(child) => {
                body.extend(varint_bytes(child.count));
                body.extend(child.summary.iter().copied());
                links.push(child.obj);
            }
            None => body.push(0),
        }
    }
    Obj { body, links }
}

fn forged_root(count: u64, summary: Vec<u8>, top: usize) -> Obj {
    let mut body = varint_bytes(count);
    body.extend(summary);
    Obj {
        body,
        links: vec![top],
    }
}

fn keys_at(level: u32, count: usize, from: u64) -> Vec<u64> {
    (from..)
        .filter(|key| common::level_of(key) == level)
        .take(count)
        .collect()
}

fn inline<D: Data>(value: &D) -> Slot {
    Slot::Inline(body_of(value))
}

#[derive(Clone, Debug, Default, PartialEq, Data)]
struct Total(u64);

impl ferro::Summary<u64, u64> for Total {
    fn of(_key: &u64, value: &u64) -> Self {
        Total(*value)
    }

    fn combine(&self, other: &Self) -> Self {
        Total(self.0 + other.0)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Data)]
struct Len(u64);

impl ferro::Summary<u64, ferro::Bytes> for Len {
    fn of(_key: &u64, value: &ferro::Bytes) -> Self {
        Len(value.len() as u64)
    }

    fn combine(&self, other: &Self) -> Self {
        Len(self.0 + other.0)
    }
}

fn one_entry(
    slot: impl FnOnce(&mut Forge) -> Slot,
    summary: Vec<u8>,
) -> (tempfile::TempDir, std::path::PathBuf) {
    let key = keys_at(0, 1, 1)[0];
    let mut forge = Forge::default();
    let slot = slot(&mut forge);
    let top = forge.add(forged_node(0, &[key], vec![slot], vec![]));
    forge.file(forged_root(1, summary, top))
}

fn apart(body: Vec<u8>, summary: Vec<u8>) -> impl FnOnce(&mut Forge) -> Slot {
    move |forge| {
        let obj = forge.add(Obj {
            body,
            links: Vec::new(),
        });
        Slot::Apart(obj, summary)
    }
}

fn typed_verdict<T: Data + Default>(path: &std::path::Path) -> ferro::Result<ferro::Verified> {
    ferro::admin::verify(path).unwrap();
    ferro::Db::<T>::open(path).unwrap().verify_typed()
}

fn received<T: Data + Default>(path: &std::path::Path) -> ferro::Result<()> {
    let sender = ferro::Db::<T>::open(path).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let receiver = ferro::Db::<T>::open(dir.path().join("receiver.ferro")).unwrap();
    let bundle = sender.bundle(&receiver.have().unwrap()).unwrap();
    let received = receiver.receive("peer", &bundle, |_, _, _| ("merge", ()));
    assert_eq!(receiver.version(), 0);
    received.map(drop)
}

fn refused_as(outcome: ferro::Result<ferro::Verified>, ending: &str) {
    assert!(
        matches!(&outcome, Err(ferro::Error::Corrupt(message)) if message.ends_with(ending)),
        "{outcome:?}"
    );
}

#[test]
fn verify_typed_and_receive_refuse_a_small_value_stored_apart_from_its_node() {
    let small = |len: usize| {
        format!(
            "a value of {len} bytes is stored apart from its node, where one under 512 stays in it"
        )
    };
    let (_dir, path) = one_entry(apart(body_of(&5u64), Vec::new()), Vec::new());
    refused_as(typed_verdict::<Map<u64, u64>>(&path), &small(1));
    assert!(matches!(
        received::<Map<u64, u64>>(&path),
        Err(ferro::Error::Bundle(_))
    ));

    let total = body_of(&Total(5));
    let (_dir, path) = one_entry(apart(body_of(&5u64), total.clone()), total);
    refused_as(typed_verdict::<Summed>(&path), &small(1));
    assert!(matches!(
        received::<Summed>(&path),
        Err(ferro::Error::Bundle(_))
    ));

    let value = ferro::Bytes::from(vec![7; 509]);
    let (_dir, path) = one_entry(apart(body_of(&value), Vec::new()), Vec::new());
    refused_as(typed_verdict::<Map<u64, ferro::Bytes>>(&path), &small(511));
}

#[test]
fn verify_typed_and_receive_refuse_a_large_value_stored_in_its_node() {
    let large = "a value of 512 bytes sits in its node, where one of 512 or more is stored apart";
    let value = ferro::Bytes::from(vec![7; 510]);
    let (_dir, path) = one_entry(|_| inline(&value), Vec::new());
    refused_as(typed_verdict::<Map<u64, ferro::Bytes>>(&path), large);
    assert!(matches!(
        received::<Map<u64, ferro::Bytes>>(&path),
        Err(ferro::Error::Bundle(_))
    ));

    let (_dir, path) = one_entry(|_| inline(&value), body_of(&Len(510)));
    refused_as(typed_verdict::<Map<u64, ferro::Bytes, Len>>(&path), large);
    assert!(matches!(
        received::<Map<u64, ferro::Bytes, Len>>(&path),
        Err(ferro::Error::Bundle(_))
    ));
}

type Summed = Map<u64, u64, Total>;

fn two_level(
    parent_key: u64,
    child_keys: &[u64],
    child_count: u64,
    child_summary: u64,
    root_count: u64,
    root_summary: u64,
) -> (tempfile::TempDir, std::path::PathBuf) {
    let mut forge = Forge::default();
    let parent_level = common::level_of(&parent_key) as u8;
    let child_level = child_keys
        .first()
        .map_or(0, |key| common::level_of(key) as u8);
    let child = forge.add(forged_node(
        child_level,
        child_keys,
        child_keys.iter().map(inline).collect(),
        vec![],
    ));
    let top = forge.add(forged_node(
        parent_level,
        &[parent_key],
        vec![inline(&parent_key)],
        vec![Child {
            at: 0,
            count: child_count,
            summary: body_of(&Total(child_summary)),
            obj: child,
        }],
    ));
    forge.file(forged_root(root_count, body_of(&Total(root_summary)), top))
}

#[test]
fn verify_typed_reports_each_way_a_forged_map_node_disagrees_with_its_parent() {
    let high = keys_at(1, 1, 1000)[0];
    let low: Vec<u64> = keys_at(0, 3, 1)
        .into_iter()
        .filter(|key| *key < high)
        .collect();
    assert_eq!(low.len(), 3);
    let sum = |keys: &[u64]| keys.iter().sum::<u64>();
    let all = sum(&low) + high;

    let (_dir, good) = two_level(high, &low, 3, sum(&low), 4, all);
    let db = ferro::Db::<Summed>::open(&good).unwrap();
    db.verify_typed().unwrap();
    assert_eq!(db.read().summary(), Some(Total(all)));
    drop(db);

    let fits = "not what its parent records";
    let fold = "a map node's link records a summary that is not the fold of its entries";
    let level = "a key sits at a level it does not hash to";
    let swapped = [low[1], low[0], low[2]];
    let escaped = keys_at(0, 1, high + 1)[0];
    let outside = [low[0], low[1], escaped];
    let twice = [low[0], low[1], high];
    let level_ones: Vec<u64> = keys_at(1, 2, 1)
        .into_iter()
        .filter(|key| *key < high)
        .collect();
    assert_eq!(level_ones.len(), 2);
    let cases = [
        (two_level(high, &low, 2, sum(&low), 3, all), fits),
        (two_level(high, &low, 3, sum(&low) + 1, 4, all + 1), fold),
        (two_level(high, &low, 3, sum(&low), 4, all + 1), fold),
        (two_level(high, &low, 3, sum(&low), 5, all), fits),
        (
            two_level(high, &swapped, 3, sum(&low), 4, all),
            "node keys are out of order",
        ),
        (
            two_level(high, &outside, 3, sum(&outside), 4, sum(&outside) + high),
            "node keys fall outside the range their parent gives them",
        ),
        (
            two_level(high, &twice, 3, sum(&twice), 4, sum(&twice) + high),
            level,
        ),
        (
            two_level(
                high,
                &level_ones,
                2,
                sum(&level_ones),
                3,
                sum(&level_ones) + high,
            ),
            fits,
        ),
        (
            one_entry(
                apart(body_of(&5u64), body_of(&Total(6))),
                body_of(&Total(6)),
            ),
            "stays in it",
        ),
        (
            one_entry(
                apart([body_of(&5u64), vec![0; 600]].concat(), body_of(&Total(6))),
                body_of(&Total(6)),
            ),
            "a spilled value's entry records a summary that is not the fold of its entries",
        ),
        (
            one_entry(
                apart([body_of(&5u64), vec![0; 600]].concat(), body_of(&Total(5))),
                body_of(&Total(5)),
            ),
            "600 bytes and 0 links are left after the value",
        ),
    ];
    for ((_dir, path), ending) in cases {
        refused_as(typed_verdict::<Summed>(&path), ending);
    }
}

#[derive(ferro::Record, Data, Clone, Debug, PartialEq)]
struct Blob {
    #[key]
    id: u64,
    #[index]
    tag: u32,
    body: ferro::Bytes,
}

#[derive(Data, Clone, Debug, Default, PartialEq)]
struct Sized {
    plain: Map<u64, ferro::Bytes>,
    summed: Map<u64, ferro::Bytes, Len>,
    nested: Map<u64, (ferro::Bytes, Map<u64, ferro::Bytes>)>,
    rows: ferro::Table<Blob>,
}

fn sized(len: u64) -> ferro::Bytes {
    ferro::Bytes::from(vec![len as u8; len as usize])
}

fn nested_value(len: u64) -> (ferro::Bytes, Map<u64, ferro::Bytes>) {
    let inner = (500..=520).map(|inner| (inner, sized(inner))).collect();
    (sized(len), inner)
}

fn blob(id: u64, len: u64) -> Blob {
    Blob {
        id,
        tag: (id % 7) as u32,
        body: sized(len),
    }
}

#[test]
fn values_at_either_side_of_the_spill_size_pass_verify_typed_and_receive() {
    let lens = 480..=530;
    let sizes = |encoded: Vec<usize>| encoded.contains(&511) && encoded.contains(&512);
    assert!(sizes(
        lens.clone().map(|len| body_of(&sized(len)).len()).collect()
    ));
    assert!(sizes(
        lens.clone()
            .map(|len| body_of(&nested_value(len)).len())
            .collect()
    ));
    assert!(sizes(
        lens.clone()
            .map(|len| body_of(&blob(len, len)).len())
            .collect()
    ));

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sized.ferro");
    let db = ferro::Db::<Sized>::open(&path).unwrap();
    let edits: [&dyn Fn(&mut Sized); 4] = [
        &|root| {
            for len in lens.clone() {
                root.plain.insert(len, sized(len));
                root.summed.insert(len, sized(len));
                root.nested.insert(len, nested_value(len));
                root.rows.insert(blob(len, len));
            }
        },
        &|root| {
            for len in lens.clone() {
                root.plain.insert(len * 1000, sized(3));
                root.summed.insert(len * 1000, sized(3));
                root.rows.insert(blob(len * 1000, 3));
            }
        },
        &|root| {
            root.plain.rewrite();
            root.nested.rewrite();
            root.rows.rewrite();
        },
        &|root| {
            for len in lens.clone().step_by(3) {
                root.plain.remove(&len);
                root.summed.remove(&len);
                root.nested.remove(&len);
                root.rows.remove(&len);
            }
        },
    ];
    for edit in edits {
        db.update(edit).unwrap();
        db.verify_typed().unwrap();
    }
    let receiver = ferro::Db::<Sized>::open(dir.path().join("receiver.ferro")).unwrap();
    let bundle = db.bundle(&receiver.have().unwrap()).unwrap();
    receiver
        .receive("peer", &bundle, |root, _, theirs| {
            *root = (**theirs).clone();
            ("take", ())
        })
        .unwrap();
    assert!(*receiver.read() == *db.read());
    receiver.verify_typed().unwrap();
    drop(db);
    let db = ferro::Db::<Sized>::open(&path).unwrap();
    db.verify_typed().unwrap();
}

#[test]
fn a_map_node_with_bytes_after_its_entries_fails_a_typed_verify() {
    let (_dir, db) = forged_map([0x05, 0x00]);
    assert_eq!(db.read().get(&3).as_deref(), Some(&5));
    assert!(matches!(
        db.verify_typed(),
        Err(ferro::Error::Corrupt(message)) if message.contains("left after the value")
    ));
}

fn verified_whole<T: Data + Default>(db: &ferro::Db<T>) {
    db.verify_typed().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let other: ferro::Db<T> = fast().open(dir.path().join("other.ferro")).unwrap();
    let bundle = db.bundle(&other.have().unwrap()).unwrap();
    other
        .receive("sender", &bundle, |_, _, _| ("merge".to_string(), ()))
        .unwrap();
    other.verify_typed().unwrap();
    db.compact(ferro::Keep::Last(1)).unwrap();
    db.verify_typed().unwrap();
}

mod spill_v1 {
    #[derive(ferro::Data, Clone, Debug, Default)]
    pub struct Blob {
        pub data: Vec<u8>,
    }

    #[derive(ferro::Data, Clone, Default)]
    pub struct Root {
        pub blobs: ferro::Map<u32, Blob>,
    }
}

mod spill_v2 {
    #[derive(ferro::Data, Clone, Debug, Default)]
    pub struct Blob {
        pub data: Vec<u8>,
        #[ferro(default)]
        pub extra: u64,
    }

    #[derive(ferro::Data, Clone, Default)]
    pub struct Root {
        pub blobs: ferro::Map<u32, Blob>,
    }
}

mod spill_v3 {
    #[derive(ferro::Data, Clone, Debug, Default)]
    #[ferro(removed("extra"))]
    pub struct Blob {
        pub data: Vec<u8>,
    }

    #[derive(ferro::Data, Clone, Default)]
    pub struct Root {
        pub blobs: ferro::Map<u32, Blob>,
    }
}

#[test]
fn values_near_the_spill_size_hold_their_placement_across_schema_changes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("spill.ferro");
    {
        let db: ferro::Db<spill_v1::Root> = fast().open(&path).unwrap();
        db.commit("fill", |root| {
            for key in 0..400u32 {
                let data = vec![key as u8; 490 + (key % 30) as usize];
                root.blobs.insert(key, spill_v1::Blob { data });
            }
        })
        .unwrap();
        db.verify_typed().unwrap();
    }
    {
        let db: ferro::Db<spill_v2::Root> = fast().open(&path).unwrap();
        db.verify_typed().unwrap();
        db.commit("touch", |root| {
            for key in (0..400u32).step_by(7) {
                let data = vec![1; 500 + (key % 20) as usize];
                let extra = u64::from(key) * 1_000_000;
                root.blobs
                    .insert(key + 1000, spill_v2::Blob { data, extra });
            }
            for key in (3..400u32).step_by(11) {
                if let Some(blob) = root.blobs.get_mut(&key) {
                    blob.extra = 1 << 40;
                }
            }
        })
        .unwrap();
        db.verify_typed().unwrap();
    }
    let db: ferro::Db<spill_v3::Root> = fast().open(&path).unwrap();
    assert_eq!(db.read().blobs.len(), 458);
    verified_whole(&db);
    db.commit("rewrite", |root| root.blobs.rewrite()).unwrap();
    verified_whole(&db);
}

#[derive(Data, Clone, Default)]
struct Lists {
    list: List<String>,
    lists: Map<u32, List<u64>>,
    sets: Set<String>,
}

#[test]
fn lists_hold_their_positions_after_every_operation() {
    let dir = tempfile::tempdir().unwrap();
    let db: ferro::Db<Lists> = fast().open(dir.path().join("lists.ferro")).unwrap();
    db.commit("extend", |root| {
        root.list.extend((0..600).map(|n| format!("item {n}")));
        for key in 0..20u32 {
            let list = (0..u64::from(key) * 40).collect::<List<u64>>();
            root.lists.insert(key, list);
        }
        root.sets.extend((0..300).map(|n| format!("s{n}")));
    })
    .unwrap();
    db.commit("pop", |root| {
        root.list.pop();
        root.list.pop();
    })
    .unwrap();
    db.commit("set", |root| {
        root.list.set(5, "five".into());
        root.list.set(root.list.len() - 1, "last".into());
    })
    .unwrap();
    db.commit("truncate", |root| root.list.truncate(333))
        .unwrap();
    db.commit("push", |root| {
        for n in 0..50 {
            root.list.push(format!("again {n}"));
        }
    })
    .unwrap();
    db.commit("inner", |root| {
        for key in 0..20u32 {
            let Some(list) = root.lists.get_mut(&key) else {
                continue;
            };
            match key % 4 {
                0 => list.clear(),
                1 => list.truncate(key as usize),
                2 => {
                    list.pop();
                    list.push(9);
                }
                _ => list.rewrite(),
            }
        }
        root.sets.remove("s7");
    })
    .unwrap();
    verified_whole(&db);
    db.commit("empty", |root| {
        root.list.truncate(0);
        root.list.push("one".into());
    })
    .unwrap();
    verified_whole(&db);
    assert_eq!(db.read().list.len(), 1);
}

thread_local! {
    static KEY_READS: Cell<u64> = const { Cell::new(0) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct CountedKey(u8);

impl Data for CountedKey {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        self.0.encode(w)
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        KEY_READS.set(KEY_READS.get() + 1);
        Ok(CountedKey(u8::decode(r)?))
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Descending(BTreeMap<CountedKey, Descending>);

impl Data for Descending {
    fn encode(&self, w: &mut Encoder<'_>) -> ferro::Result<()> {
        w.nested(|w| {
            w.varint(self.0.len() as u64);
            for (key, value) in self.0.iter().rev() {
                w.item(|w| {
                    key.encode(w)?;
                    value.encode(w)
                })?;
            }
            Ok(())
        })
    }

    fn decode(r: &mut Decoder<'_>) -> ferro::Result<Self> {
        Ok(Descending(BTreeMap::decode(r)?))
    }

    fn visit(r: &mut Decoder<'_>) -> ferro::Result<()> {
        BTreeMap::<CountedKey, Descending>::visit(r)
    }
}

fn descending(depth: u32) -> Descending {
    if depth == 0 {
        return Descending::default();
    }
    Descending(BTreeMap::from([
        (CountedKey(0), descending(depth - 1)),
        (CountedKey(1), descending(depth - 1)),
    ]))
}

fn key_reads(read: impl FnOnce(&mut Decoder<'_>), bytes: &[u8]) -> u64 {
    KEY_READS.set(0);
    let mut decoder = Decoder::new(bytes, &[], None);
    read(&mut decoder);
    decoder.end().unwrap();
    KEY_READS.get()
}

#[test]
fn a_visit_reads_each_key_at_most_twice_however_deep_the_keys_are_out_of_order() {
    for depth in [1u32, 2, 6, 12] {
        let entries = (1u64 << (depth + 1)) - 2;
        let bytes = body_of(&descending(depth));
        let decoded = key_reads(
            |r| assert_eq!(Descending::decode(r).unwrap(), descending(depth)),
            &bytes,
        );
        let visited = key_reads(|r| Descending::visit(r).unwrap(), &bytes);
        assert_eq!(decoded, entries, "depth {depth}");
        assert!(
            visited <= 2 * entries,
            "depth {depth}: a visit read {visited} keys of {entries} entries"
        );
    }
}

fn leb(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn scrambled<K: Data, V>(
    rng: &mut common::Rng,
    mut entries: Vec<(K, V)>,
    value: impl Fn(&mut common::Rng, &V, &mut Vec<u8>),
) -> Vec<u8> {
    if rng.chance(60) {
        rng.shuffle(&mut entries);
    }
    let mut order: Vec<usize> = (0..entries.len()).collect();
    if rng.chance(10) && !entries.is_empty() {
        let at = rng.below(order.len() as u64 + 1) as usize;
        order.insert(at, rng.below(entries.len() as u64) as usize);
    }
    let mut out = Vec::new();
    leb(&mut out, order.len() as u64);
    for at in order {
        let (key, held) = &entries[at];
        out.extend(body_of(key));
        value(rng, held, &mut out);
    }
    out
}

type Inner = BTreeMap<u16, BTreeSet<u8>>;
type Outer = BTreeMap<u8, Inner>;

fn scrambled_set(rng: &mut common::Rng, set: &BTreeSet<u8>, out: &mut Vec<u8>) {
    let entries = set.iter().map(|&item| (item, ())).collect();
    out.extend(scrambled(rng, entries, |_, _, _| {}));
}

fn scrambled_inner(rng: &mut common::Rng, inner: &Inner, out: &mut Vec<u8>) {
    out.extend(scrambled(
        rng,
        inner.clone().into_iter().collect(),
        scrambled_set,
    ));
}

fn scrambled_outer(rng: &mut common::Rng, outer: &Outer) -> Vec<u8> {
    scrambled(rng, outer.clone().into_iter().collect(), scrambled_inner)
}

fn random_outer(rng: &mut common::Rng) -> Outer {
    (0..rng.below(5))
        .map(|_| {
            let inner = (0..rng.below(4))
                .map(|_| {
                    let set = (0..rng.below(4)).map(|_| rng.below(6) as u8).collect();
                    (rng.below(300) as u16, set)
                })
                .collect();
            (rng.below(8) as u8, inner)
        })
        .collect()
}

fn read_both_ways<T: Data>(bytes: &[u8]) -> Result<Option<T>, String> {
    let mut decoder = Decoder::new(bytes, &[], None);
    let decoded = T::decode(&mut decoder);
    let mut visitor = Decoder::new(bytes, &[], None);
    let visited = T::visit(&mut visitor);
    match (decoded, visited) {
        (Ok(value), Ok(())) if decoder.remaining() == visitor.remaining() => Ok(Some(value)),
        (Err(_), Err(_)) => Ok(None),
        (decoded, visited) => Err(format!(
            "decode {:?} with {} left, visit {visited:?} with {} left, of {bytes:02x?}",
            decoded.map(drop),
            decoder.remaining(),
            visitor.remaining()
        )),
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(2000), failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn decode_and_visit_agree_on_nested_btrees_stored_in_any_order(seed in any::<u64>(), damage in 0u32..4) {
        let mut rng = common::Rng::new(seed);
        let outer = random_outer(&mut rng);
        let mut bytes = scrambled_outer(&mut rng, &outer);
        for _ in 0..damage {
            if bytes.is_empty() {
                break;
            }
            let at = rng.below(bytes.len() as u64) as usize;
            match rng.below(4) {
                0 => bytes[at] ^= 1 << rng.below(8),
                1 => bytes.truncate(at),
                2 => bytes.insert(at, rng.next() as u8),
                _ => {
                    bytes.remove(at);
                }
            }
        }
        let read = read_both_ways::<Outer>(&bytes);
        prop_assert!(read.is_ok(), "{}", read.unwrap_err());
        if damage == 0 && let Ok(Some(read)) = &read {
            prop_assert_eq!(read, &outer);
        }
        let mut twice = Vec::new();
        leb(&mut twice, 2);
        twice.extend(&bytes);
        twice.extend(&bytes);
        let read = read_both_ways::<Vec<Outer>>(&twice);
        prop_assert!(read.is_ok(), "{}", read.unwrap_err());
    }
}

#[test]
fn zero_width_items_of_btrees_out_of_order_count_once() {
    const LIMIT: u64 = 1 << 24;
    for spare in [0u64, 1] {
        let mut bytes = Vec::new();
        leb(&mut bytes, LIMIT - 7000 + spare);
        leb(&mut bytes, 2);
        for (key, units) in [(5u8, 1000u64), (3, 1000)] {
            bytes.push(key);
            leb(&mut bytes, units);
        }
        leb(&mut bytes, 2);
        for units in [3000u64, 2000] {
            leb(&mut bytes, units);
        }
        type Units = (Vec<()>, BTreeMap<u8, Vec<()>>, BTreeSet<Vec<()>>);
        let read = read_both_ways::<Units>(&bytes).unwrap();
        assert_eq!(read.is_some(), spare == 0, "spare {spare}");
    }
}
