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

use std::fmt;

use rustc_hash::FxHashMap;

use crate::ast::{Definition, Document, FragmentDefinition};

/// Above this many fragments, lookup uses a hash map instead of a linear scan.
const INLINE_FRAGMENTS: usize = 8;

/// A fragment index computed once for a document that will be executed repeatedly.
///
/// Names borrow the document's source text and the positions index [`Document::definitions`], so an
/// index holds no reference to the document itself. Combining it with a borrow of the document at
/// execution time produces a [`Fragments`] lookup without scanning the definitions or allocating
/// again — the work a prepared document would otherwise repeat on every request and every
/// subscription event.
#[derive(Clone)]
pub(crate) struct FragmentIndex<'a> {
    /// Fragment names with the position of their definition, in source order.
    ///
    /// A `Vec` rather than an inline buffer: this is built once per prepared document, and keeping
    /// the type covariant lets a [`Fragments`] built from it shrink to the executing borrow.
    entries: Vec<(&'a str, usize)>,
    /// Name to position, when there are enough fragments for hashing to win.
    index: Option<FxHashMap<&'a str, usize>>,
}

impl<'a> FragmentIndex<'a> {
    /// Index the fragment definitions of `document`.
    ///
    /// The first definition of a name wins, matching [`Fragments::new`].
    ///
    /// The borrow of the document is only used to read it: every name points into the source text
    /// the document borrows, so the index itself is independent of the document's address.
    #[must_use]
    pub(crate) fn new(document: &Document<'a>) -> Self {
        let mut entries: Vec<(&'a str, usize)> = Vec::new();
        for (position, definition) in document.definitions.iter().enumerate() {
            let Definition::Fragment(fragment) = definition else {
                continue;
            };
            if entries.iter().any(|(name, _)| *name == fragment.name) {
                continue;
            }
            entries.push((fragment.name, position));
        }
        let index = (entries.len() > INLINE_FRAGMENTS).then(|| {
            entries
                .iter()
                .enumerate()
                .map(|(position, (name, _))| (*name, position))
                .collect()
        });
        Self { entries, index }
    }

    /// The position of the definition for `name`.
    #[inline]
    fn position(&self, name: &str) -> Option<usize> {
        match &self.index {
            Some(index) => index.get(name).map(|position| self.entries[*position].1),
            None => self
                .entries
                .iter()
                .find(|(known, _)| *known == name)
                .map(|(_, position)| *position),
        }
    }

    /// The number of indexed fragments.
    fn len(&self) -> usize {
        self.entries.len()
    }
}

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
    /// Many fragments: source order plus a name index.
    Large {
        entries: Vec<(&'a str, &'a FragmentDefinition<'a>)>,
        index: FxHashMap<&'a str, usize>,
    },
    /// A prepared document's lookup, resolved through its precomputed index.
    Prepared {
        document: &'a Document<'a>,
        index: &'a FragmentIndex<'a>,
    },
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
    ///
    /// Fragment definitions are kept in source order. When an invalid document defines the same
    /// name twice, the first definition wins in both representations, matching validation's
    /// "duplicate name" error and the inline lookup's behaviour.
    #[must_use]
    pub fn new(document: &'a Document<'a>) -> Self {
        let mut entries = [None; INLINE_FRAGMENTS];
        let mut len = 0usize;
        let mut fragments = document.fragments();
        for fragment in fragments.by_ref() {
            if len == INLINE_FRAGMENTS {
                let mut order: Vec<(&'a str, &'a FragmentDefinition<'a>)> =
                    entries.iter().flatten().copied().collect();
                let mut index = FxHashMap::with_capacity_and_hasher(
                    document.fragments().count(),
                    Default::default(),
                );
                for (position, (name, _)) in order.iter().enumerate() {
                    index.entry(*name).or_insert(position);
                }
                // The fragment at the loop head has already been taken from the iterator; add it
                // before draining the rest.
                if let std::collections::hash_map::Entry::Vacant(entry) = index.entry(fragment.name)
                {
                    entry.insert(order.len());
                    order.push((fragment.name, fragment));
                }
                for fragment in fragments {
                    if let std::collections::hash_map::Entry::Vacant(entry) =
                        index.entry(fragment.name)
                    {
                        entry.insert(order.len());
                        order.push((fragment.name, fragment));
                    }
                }
                return Self {
                    inner: FragmentsInner::Large {
                        entries: order,
                        index,
                    },
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

    /// Build a fragment lookup from a prepared document's precomputed index.
    ///
    /// This is [`Fragments::new`] without the scan or the allocation: the index was computed when
    /// the document was prepared, and this only pairs it with a borrow of the same document.
    #[must_use]
    pub(crate) fn prepared(document: &'a Document<'a>, index: &'a FragmentIndex<'a>) -> Self {
        Self {
            inner: FragmentsInner::Prepared { document, index },
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
            FragmentsInner::Large { entries, index } => {
                index.get(name).map(|position| entries[*position].1)
            }
            FragmentsInner::Prepared { document, index } => {
                let position = index.position(name)?;
                match document.definitions.get(position) {
                    Some(Definition::Fragment(fragment)) => Some(fragment),
                    _ => None,
                }
            }
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
            FragmentsInner::Large { entries, .. } => entries.len(),
            FragmentsInner::Prepared { index, .. } => index.len(),
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
            FragmentsInner::Large { entries, .. } => FragmentsIter::Large(entries.iter()),
            FragmentsInner::Prepared { document, index } => {
                FragmentsIter::Prepared(index.entries.iter(), document)
            }
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
    /// Iterating the ordered lookup.
    Large(std::slice::Iter<'b, (&'a str, &'a FragmentDefinition<'a>)>),
    /// Iterating a prepared document's index, resolving positions against the document.
    Prepared(std::slice::Iter<'a, (&'a str, usize)>, &'a Document<'a>),
}

impl<'a> Iterator for FragmentsIter<'_, 'a> {
    type Item = &'a FragmentDefinition<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Small(iter) => iter.find_map(|entry| entry.map(|(_, fragment)| fragment)),
            Self::Large(iter) => iter.next().map(|(_, fragment)| *fragment),
            Self::Prepared(entries, document) => loop {
                let (_, position) = entries.next()?;
                if let Some(Definition::Fragment(fragment)) = document.definitions.get(*position) {
                    return Some(fragment);
                }
            },
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            Self::Small(iter) => (0, iter.size_hint().1),
            Self::Large(iter) => iter.size_hint(),
            Self::Prepared(entries, _) => (0, entries.size_hint().1),
        }
    }
}

impl fmt::Debug for FragmentsIter<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FragmentsIter").finish_non_exhaustive()
    }
}
