//! The leading field of a tuple key, which `prefix` reads by.

use std::borrow::Borrow;

/// A key whose leading field, borrowed as `Q`, groups its entries for
/// [`Map::prefix`](crate::Map::prefix), [`Set::prefix`](crate::Set::prefix) and
/// [`Table::prefix`](crate::Table::prefix).
///
/// Implemented for tuples of two to four fields whose first field borrows as `Q`, so a
/// `(String, u64)` key is read by `&str` as well as by `&String`. A key type of your own gets
/// `prefix` by implementing it for its leading field.
///
/// [`head`](Prefix::head) must agree with the key's [`Ord`]: every key with a given head must sort
/// together, after every key with a smaller head and before every key with a larger one, as a
/// tuple's first field does. Otherwise `prefix` misses entries.
///
/// # Examples
///
/// ```
/// use ferro::{Data, Map, Prefix};
///
/// #[derive(Data, Clone, PartialEq, Eq, PartialOrd, Ord)]
/// struct Posting {
///     account: u32,
///     day: u32,
/// }
///
/// impl Prefix<u32> for Posting {
///     fn head(&self) -> &u32 {
///         &self.account
///     }
/// }
///
/// let mut amounts: Map<Posting, i64> = Map::new();
/// amounts.insert(Posting { account: 1, day: 3 }, 50);
/// amounts.insert(Posting { account: 2, day: 1 }, -20);
/// amounts.insert(Posting { account: 1, day: 9 }, -5);
///
/// let balance: i64 = amounts.prefix(&1).map(|(_, amount)| *amount).sum();
/// assert_eq!(balance, 45);
/// ```
pub trait Prefix<Q: ?Sized> {
    /// The leading field of this key, borrowed as `Q`.
    fn head(&self) -> &Q;
}

impl<A: Borrow<Q>, B, Q: ?Sized> Prefix<Q> for (A, B) {
    #[inline]
    fn head(&self) -> &Q {
        self.0.borrow()
    }
}

impl<A: Borrow<Q>, B, C, Q: ?Sized> Prefix<Q> for (A, B, C) {
    #[inline]
    fn head(&self) -> &Q {
        self.0.borrow()
    }
}

impl<A: Borrow<Q>, B, C, D, Q: ?Sized> Prefix<Q> for (A, B, C, D) {
    #[inline]
    fn head(&self) -> &Q {
        self.0.borrow()
    }
}
