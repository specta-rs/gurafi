//! Fragment lookup for executable documents.
//!
//! Validation, field collection and execution all need to find a fragment definition by name.
//! [`Fragments`] is that lookup, built once per document and borrowed for the lifetime of the
//! request. It is public so a resolver receiving [`ResolveInfo`](crate::ResolveInfo) can inspect
//! the fragments of the operation being executed without exposing executor internals.
//!
//! Documents usually define only a handful of fragments, and execution looks them up once per
//! selection set, so an inline array is cheaper than a hash map. Documents that define many
//! fragments fall back to hashing to keep lookup cost independent of their number.

use std::{collections::hash_map, fmt};

use rustc_hash::FxHashMap;

use crate::ast::{Document, FragmentDefinition};

/// Above this many fragments, lookup uses a hash map instead of a linear scan.
const INLINE_FRAGMENTS: usize = 8;

/// A map of fragment definitions keyed by name, borrowed from a parsed document.
///
/// Build one with [`Fragments::new`] or `Fragments::from(&document)`. Lookup is by fragment name,
/// exactly as a `...FragmentName` spread names it (without the leading `...`).
///
/// ```
/// use gql::{Fragments, parser::parse_query};
///
/// let document = parse_query("query Q { ...Fields } fragment Fields on Query { hello }").unwrap();
/// let fragments = Fragments::new(&document);
/// assert!(fragments.get("Fields").is_some());
/// ```
#[derive(Clone)]
pub struct Fragments<'a> {
    inner: FragmentsInner<'a>,
}

#[derive(Clone)]
enum FragmentsInner<'a> {
    /// Few enough fragments that a linear scan wins.
    Small {
        entries: [Option<(&'a str, &'a FragmentDefinition<'a>)>; INLINE_FRAGMENTS],
        len: u8,
    },
    /// Many fragments, indexed by name.
    Large(FxHashMap<&'a str, &'a FragmentDefinition<'a>>),
}

impl<'a> Fragments<'a> {
    /// An empty lookup, useful when no document is available.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            inner: FragmentsInner::Small {
                entries: [None; INLINE_FRAGMENTS],
                len: 0,
            },
        }
    }

    /// Build a fragment lookup for a parsed document.
    #[must_use]
    pub fn new(document: &'a Document<'a>) -> Self {
        let mut entries = [None; INLINE_FRAGMENTS];
        let mut len = 0usize;
        let mut fragments = document.fragments();
        for fragment in fragments.by_ref() {
            if len == INLINE_FRAGMENTS {
                let mut large: FxHashMap<_, _> = entries.iter().flatten().copied().collect();
                large.insert(fragment.name, fragment);
                for fragment in fragments {
                    large.insert(fragment.name, fragment);
                }
                return Self {
                    inner: FragmentsInner::Large(large),
                };
            }
            entries[len] = Some((fragment.name, fragment));
            len += 1;
        }
        Self {
            inner: FragmentsInner::Small {
                entries,
                len: len as u8,
            },
        }
    }

    /// Look up a fragment definition by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&'a FragmentDefinition<'a>> {
        match &self.inner {
            FragmentsInner::Small { entries, len } => entries[..usize::from(*len)]
                .iter()
                .flatten()
                .find(|(known, _)| *known == name)
                .map(|(_, fragment)| *fragment),
            FragmentsInner::Large(fragments) => fragments.get(name).copied(),
        }
    }

    /// Whether a fragment with the given name is defined.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// The number of fragment definitions.
    #[must_use]
    pub fn len(&self) -> usize {
        match &self.inner {
            FragmentsInner::Small { len, .. } => usize::from(*len),
            FragmentsInner::Large(fragments) => fragments.len(),
        }
    }

    /// Whether the document defines no fragments.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Iterate over the fragment definitions, in source order.
    pub fn iter(&self) -> FragmentsIter<'_, 'a> {
        match &self.inner {
            FragmentsInner::Small { entries, len } => {
                FragmentsIter::Small(entries[..usize::from(*len)].iter())
            }
            FragmentsInner::Large(fragments) => FragmentsIter::Large(fragments.iter()),
        }
    }
}

impl<'a> From<&'a Document<'a>> for Fragments<'a> {
    fn from(document: &'a Document<'a>) -> Self {
        Self::new(document)
    }
}

impl<'b, 'a> IntoIterator for &'b Fragments<'a> {
    type Item = &'a FragmentDefinition<'a>;
    type IntoIter = FragmentsIter<'b, 'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl fmt::Debug for Fragments<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.iter().map(|fragment| (fragment.name, fragment)))
            .finish()
    }
}

/// An iterator over the fragment definitions of a [`Fragments`] lookup.
pub enum FragmentsIter<'b, 'a> {
    /// Iterating the inline lookup.
    Small(std::slice::Iter<'b, Option<(&'a str, &'a FragmentDefinition<'a>)>>),
    /// Iterating the hashed lookup.
    Large(hash_map::Iter<'b, &'a str, &'a FragmentDefinition<'a>>),
}

impl<'a> Iterator for FragmentsIter<'_, 'a> {
    type Item = &'a FragmentDefinition<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Small(iter) => iter.find_map(|entry| entry.map(|(_, fragment)| fragment)),
            Self::Large(iter) => iter.next().map(|(_, fragment)| *fragment),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            Self::Small(iter) => (0, iter.size_hint().1),
            Self::Large(iter) => iter.size_hint(),
        }
    }
}

impl fmt::Debug for FragmentsIter<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FragmentsIter").finish_non_exhaustive()
    }
}
