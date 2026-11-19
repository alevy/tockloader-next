//! Board definitions in ordered tiers, and looking a name up among them.
//!
//! No I/O here, like the rest of this crate: a [`Tier`] is built by the
//! caller from whatever it scanned or bundled, and handed in as data.

use crate::Error;

/// One board's contents, however it was found.
#[derive(Debug, Clone)]
pub struct Definition {
    /// Where this came from: `bundled:<name>` for a bundled board, or a
    /// path otherwise. Self-exclusion in `extends` and relative-path
    /// resolution both compare this exactly, so it must stay the thing a
    /// file was actually reached by, not a re-derived label.
    pub origin: String,
    pub text: String,
}

/// What a name resolves to within one tier.
#[derive(Debug, Clone)]
pub enum Entry {
    Board(Definition),
    /// Two or more files in one tier claim this name. Resolving the name
    /// is an error -- see [`Environment::lookup`].
    Ambiguous(Vec<String>),
}

/// Names defined at one tier.
pub type Tier = std::collections::BTreeMap<String, Entry>;

/// Board definitions in ordered tiers, lowest (least specific) first.
///
/// A bare name resolves to the highest tier that defines it. `extends`
/// resolves among tiers at or below the extending file's own tier,
/// excluding that file itself -- the tier bound stops a definition reaching
/// upward, and the self-exclusion is what stops it finding itself.
#[derive(Debug)]
pub struct Environment {
    tiers: Vec<Tier>,
}

impl Environment {
    /// Tier 0: the bundled table, origins spelled `bundled:<name>`.
    pub fn bundled() -> Environment {
        let tier = crate::BUNDLED
            .iter()
            .map(|(name, text)| {
                (
                    (*name).to_string(),
                    Entry::Board(Definition {
                        origin: format!("bundled:{name}"),
                        text: (*text).to_string(),
                    }),
                )
            })
            .collect();
        Environment { tiers: vec![tier] }
    }

    /// Add a new highest tier.
    pub fn push(&mut self, tier: Tier) {
        self.tiers.push(tier);
    }

    pub fn tiers(&self) -> &[Tier] {
        &self.tiers
    }

    /// The highest tier defining `name` at index `at_or_below` or lower
    /// (`None` searches every tier). `exclude` skips a matching
    /// [`Entry::Board`]; any [`Entry::Ambiguous`] in range is an error,
    /// regardless of `exclude` -- the collision it names has no text to
    /// fall back to, so there is no board to return even when the
    /// extending file is one of the colliders. `Ok(None)` means no tier in
    /// range defines it.
    pub(crate) fn lookup(
        &self,
        name: &str,
        at_or_below: Option<usize>,
        exclude: Option<&str>,
    ) -> Result<Option<(usize, &Definition)>, Error> {
        if self.tiers.is_empty() {
            return Ok(None);
        }
        let highest = self.tiers.len() - 1;
        let top = at_or_below.unwrap_or(highest).min(highest);
        for i in (0..=top).rev() {
            match self.tiers[i].get(name) {
                None => continue,
                Some(Entry::Board(def)) => {
                    if exclude == Some(def.origin.as_str()) {
                        continue;
                    }
                    return Ok(Some((i, def)));
                }
                Some(Entry::Ambiguous(origins)) => {
                    return Err(Error::AmbiguousBoard {
                        name: name.to_string(),
                        origins: origins.clone(),
                    });
                }
            }
        }
        Ok(None)
    }

    /// Every name any tier defines, sorted and deduplicated, for an error
    /// that has to say what is available.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tiers.iter().flat_map(|t| t.keys().cloned()).collect();
        names.sort();
        names.dedup();
        names
    }
}
