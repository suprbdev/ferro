//! What the code that [`#[derive(Data)]`](derive@crate::Data) and
//! [`#[derive(Record)]`](derive@crate::Record) generate calls, and nothing else.
//!
//! This module is not part of ferro's API, and semantic versioning does not cover it: anything in
//! it may change or go in any release, together with the derives that call it. The derives name
//! it as `::ferro::__derive`, or under the path `#[ferro(crate = "...")]` gives. Code written by
//! hand that names it, such as a `Data` impl that overrides `tables` or `pair`, may need to change
//! with any release.
//!
//! The derives also implement or call hidden items of public traits, which stay on those traits
//! and are outside semantic versioning in the same way:
//!
//! - [`Data`]: `OPTIONAL`, `absent`, `tables` and `pair`.
//! - [`TableData`](crate::TableData): `write`, `add`, `decode_stored`, `indexing_reads`,
//!   `diff_slots` and `built_slots`.
//! - [`IndexEach`](crate::table::IndexEach): `READS` and `delta`, and `try_values` and
//!   `read_values`, which [`each_values`] calls.
//!
//! `Data::LOCATED` is hidden too, for ferro's own types; the derives neither set nor read it.

use crate::codec::{Data, Decoder};
use crate::error::Result;

pub use crate::codec::pairing::{Pairing, Tabled};
pub use crate::map::edits::Edits;
pub use crate::table::derive::{
    Delta, Differ, EveryRow, Fault, Fingerprints, Fresh, IndexSink, Indexed, Project, RowChange,
    Rows, Stage, Started, Stored, Verifying, each_values, encode_index, index_after,
    index_after_some, index_counts, index_eq, index_eq_some, index_fingerprint, index_range,
    index_where, match_after, match_after_some, match_eq, match_eq_some, noted, plain, present,
    same_stored, stored_rows, unique_eq, unique_rows, unique_rows_covered, unique_some,
    unique_some_range,
};

/// Decodes a value of `T` from `r`, the decoder of one field, into `slot`, as the derive reads
/// each stored field.
///
/// # Errors
///
/// As for [`Data::decode`], and [`Error::Decode`](crate::Error::Decode) if the value does not
/// read the field to its end.
pub fn decode_into<T: Data>(slot: &mut Option<T>, r: &mut Decoder<'_>) -> Result<()> {
    *slot = Some(T::decode(r)?);
    r.end()
}

/// Visits a value of `T` in `r`, the decoder of one field, as the derive checks each stored
/// field.
///
/// # Errors
///
/// As for [`decode_into`].
pub fn visit_field<T: Data>(r: &mut Decoder<'_>) -> Result<()> {
    T::visit(r)?;
    r.end()
}

/// Whether a table that `r` decodes rebuilds the indexes it does not store, as [`Data::decode`]
/// does unless the decoder belongs to the check that a table's indexes agree with its rows.
#[inline]
#[must_use]
pub fn rebuilds(r: &Decoder<'_>) -> bool {
    r.rebuilds()
}

/// Reads the fields of `r` as [`Decoder::fields`] does, but lets several fields in a row share a
/// tag, as a table stores each of its indexes under one tag.
///
/// # Errors
///
/// As for [`Decoder::fields`], refusing a tag lower than the one before it.
#[inline]
pub fn fields_sharing_tags<'a>(
    r: &mut Decoder<'a>,
    f: impl FnMut(u16, &mut Decoder<'a>) -> Result<()>,
) -> Result<()> {
    r.fields_sharing_tags(f)
}
