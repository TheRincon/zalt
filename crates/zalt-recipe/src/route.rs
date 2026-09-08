//! Content-type routing: several recipes, and picking one.
//!
//! ## Why ambiguity is an error rather than a precedence rule
//!
//! Two recipes claiming one content type could be resolved by declaration
//! order, by filename, by specificity — every one of those is defensible and
//! none of them is guessable from reading the manifests. A host that loaded a
//! directory would get whichever recipe the filesystem happened to enumerate
//! first, which is not a rule so much as an outcome.
//!
//! So [`RecipeSet::insert`] refuses the second claim. The cost is that adding a
//! recipe can fail; the benefit is that a routing decision can always be
//! explained by pointing at exactly one manifest.
//!
//! Within a single recipe, `[[rule]]` ordering *is* the tie-break — see
//! [`Recipe::encode_for`]. The difference is that one manifest's rules were
//! written together by one author who can see all of them at once, and two
//! manifests in a directory were not.

use alloc::vec::Vec;

use zalt_codec::dict::Dictionary;

use crate::error::{Error, ErrorKind, Result};
use crate::recipe::{Recipe, extension_of, normalise_content_type};

/// A recipe and the dictionary it resolved to.
///
/// One type rather than two parallel collections. A host that has just routed a
/// resource wants both halves — which parameters, and which dictionary — and
/// making it join them by name afterwards would be an invitation to pair a
/// recipe with the wrong dictionary, which is the one mistake §7 says produces
/// wrong bytes instead of an error.
#[derive(Debug, Clone)]
pub struct Entry {
    /// The manifest.
    pub recipe: Recipe,
    /// The dictionary, where one was named *and* resolvable locally.
    ///
    /// `None` also covers a recipe naming only a `Dictionary_ID` for both peers
    /// to resolve out of band: nothing on disk to read, and not an error.
    pub dictionary: Option<Dictionary>,
}

/// A collection of recipes, with a deterministic route from content to one.
#[derive(Debug, Clone, Default)]
pub struct RecipeSet {
    entries: Vec<Entry>,
}

impl RecipeSet {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a recipe with no dictionary.
    pub fn insert(&mut self, recipe: Recipe) -> Result<()> {
        self.insert_entry(Entry {
            recipe,
            dictionary: None,
        })
    }

    /// Add a recipe and the dictionary it resolved to.
    ///
    /// A recipe claiming *nothing* is accepted: it is reachable by name, which
    /// is how a `--recipe path.toml` flag will use one.
    pub fn insert_entry(&mut self, entry: Entry) -> Result<()> {
        for existing in &self.entries {
            let collides = entry
                .recipe
                .match_
                .content_types
                .iter()
                .any(|c| existing.recipe.match_.content_types.contains(c))
                || entry
                    .recipe
                    .match_
                    .extensions
                    .iter()
                    .any(|e| existing.recipe.match_.extensions.contains(e));
            if collides {
                return Err(Error::whole_file(ErrorKind::AmbiguousRoute));
            }
        }
        self.entries.push(entry);
        Ok(())
    }

    /// The entry claiming this content type, if any.
    pub fn for_content_type(&self, content_type: &str) -> Option<&Entry> {
        let wanted = normalise_content_type(content_type);
        self.entries
            .iter()
            .find(|e| e.recipe.match_.content_types.contains(&wanted))
    }

    /// The entry claiming this filename's extension, if any.
    pub fn for_filename(&self, filename: &str) -> Option<&Entry> {
        let wanted = extension_of(filename)?;
        self.entries
            .iter()
            .find(|e| e.recipe.match_.extensions.contains(&wanted))
    }

    /// The entry for a resource, preferring what the content type says.
    ///
    /// Content type wins over extension because it is what the producer
    /// declared, where an extension is what a filename happens to end with. A
    /// resource served as `application/octet-stream` is being described to us
    /// as opaque bytes, and the name on disk does not overrule that.
    pub fn route(&self, content_type: Option<&str>, filename: Option<&str>) -> Option<&Entry> {
        content_type
            .and_then(|ct| self.for_content_type(ct))
            .or_else(|| filename.and_then(|f| self.for_filename(f)))
    }

    /// The entry with this recipe name, if any.
    pub fn by_name(&self, name: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.recipe.name == name)
    }

    /// Every entry, in insertion order.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// How many recipes are held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe::Match;
    use alloc::string::ToString;

    fn recipe(name: &str, content_types: &[&str], extensions: &[&str]) -> Recipe {
        let mut r = Recipe::new(name);
        r.match_ = Match {
            content_types: content_types.iter().map(|s| s.to_string()).collect(),
            extensions: extensions.iter().map(|s| s.to_string()).collect(),
        };
        r
    }

    fn name_of(entry: Option<&Entry>) -> Option<&str> {
        entry.map(|e| e.recipe.name.as_str())
    }

    #[test]
    fn routes_by_content_type_then_by_extension() {
        let mut set = RecipeSet::new();
        set.insert(recipe("docs", &["application/x-doc"], &[".doc"]))
            .unwrap();
        set.insert(recipe("media", &["image/webp"], &[".webp"]))
            .unwrap();

        assert_eq!(name_of(set.route(Some("image/webp"), None)), Some("media"));
        assert_eq!(name_of(set.route(None, Some("a.doc"))), Some("docs"));
        // Parameters and casing must not defeat the route.
        assert_eq!(
            name_of(set.route(Some("Application/X-Doc; v=1"), None)),
            Some("docs")
        );
        assert_eq!(name_of(set.route(Some("text/plain"), Some("a.txt"))), None);
    }

    #[test]
    fn a_known_content_type_beats_a_filename_that_says_otherwise() {
        let mut set = RecipeSet::new();
        set.insert(recipe("docs", &["application/x-doc"], &[]))
            .unwrap();
        set.insert(recipe("media", &[], &[".webp"])).unwrap();
        assert_eq!(
            name_of(set.route(Some("application/x-doc"), Some("thing.webp"))),
            Some("docs")
        );
    }

    #[test]
    fn an_unclaimed_content_type_falls_through_to_the_extension() {
        let mut set = RecipeSet::new();
        set.insert(recipe("media", &[], &[".webp"])).unwrap();
        assert_eq!(
            name_of(set.route(Some("application/octet-stream"), Some("thing.webp"))),
            Some("media")
        );
    }

    #[test]
    fn a_second_claim_on_a_content_type_is_refused() {
        let mut set = RecipeSet::new();
        set.insert(recipe("first", &["application/x-doc"], &[]))
            .unwrap();
        let err = set
            .insert(recipe("second", &["application/x-doc"], &[]))
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::AmbiguousRoute);
        // And the set is unchanged, rather than half-updated.
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn a_second_claim_on_an_extension_is_refused() {
        let mut set = RecipeSet::new();
        set.insert(recipe("first", &[], &[".doc"])).unwrap();
        assert_eq!(
            set.insert(recipe("second", &[], &[".doc"]))
                .unwrap_err()
                .kind,
            ErrorKind::AmbiguousRoute
        );
    }

    #[test]
    fn recipes_claiming_nothing_are_reachable_only_by_name() {
        let mut set = RecipeSet::new();
        set.insert(Recipe::new("first")).unwrap();
        // Two of them do not collide: neither claims a route.
        set.insert(Recipe::new("second")).unwrap();
        assert_eq!(set.len(), 2);
        assert_eq!(name_of(set.route(Some("text/plain"), Some("a.txt"))), None);
        assert_eq!(name_of(set.by_name("second")), Some("second"));
        assert!(set.by_name("third").is_none());
    }

    #[test]
    fn routing_is_stable_across_insertion_order() {
        // Same two recipes, inserted both ways; the route must not depend on it.
        let a = recipe("docs", &["application/x-doc"], &[]);
        let b = recipe("media", &["image/webp"], &[]);
        let mut forward = RecipeSet::new();
        forward.insert(a.clone()).unwrap();
        forward.insert(b.clone()).unwrap();
        let mut backward = RecipeSet::new();
        backward.insert(b).unwrap();
        backward.insert(a).unwrap();

        for ct in ["application/x-doc", "image/webp"] {
            assert_eq!(
                name_of(forward.route(Some(ct), None)),
                name_of(backward.route(Some(ct), None)),
            );
        }
    }

    #[test]
    fn an_empty_set_routes_nowhere() {
        let set = RecipeSet::new();
        assert!(set.is_empty());
        assert!(set.route(Some("text/plain"), Some("a.txt")).is_none());
        assert!(set.entries().is_empty());
    }
}
