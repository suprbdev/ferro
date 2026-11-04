use std::borrow::Borrow;

pub trait Prefix<Q: ?Sized> {
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
