use std::collections::HashSet;

use crate::codec::{Object, Ptr};
use crate::db::Refs;
use crate::error::Result;
use crate::log::Chain;
use crate::store::Store;

pub(crate) struct Roots {
    pub(crate) trees: Vec<Ptr>,
    pub(crate) chains: Vec<Chain>,
}

impl Roots {
    pub(crate) fn of(head: Ptr, refs: &Refs) -> Result<Roots> {
        let chains = refs.iter().map(|(_, branch)| branch.log).collect();
        Ok(Roots {
            trees: vec![head],
            chains,
        })
    }

    pub(crate) fn walk(
        &self,
        store: &Store,
        mut fresh: impl FnMut(Ptr) -> bool,
        mut visit: impl FnMut(Ptr, &Object, u32),
    ) -> Result<()> {
        let mut stack = Vec::new();
        let mut entries = HashSet::new();
        for chain in &self.chains {
            chain.visit(store, |ptr, entry, object, len| {
                if entries.insert(ptr.offset) && fresh(ptr) {
                    visit(ptr, object, len);
                }
                stack.extend(entry.commit.root);
                stack.extend(chain.retained_skip(entry));
                Ok(())
            })?;
        }
        stack.extend(&self.trees);
        while let Some(ptr) = stack.pop() {
            if entries.contains(&ptr.offset) || !fresh(ptr) {
                continue;
            }
            let (object, len) = store.read_record(ptr)?;
            visit(ptr, &object, len);
            stack.extend(object.links.iter().copied());
        }
        Ok(())
    }
}
