//! Names: what a host may be called, and the allow-list that says which hosts the proxy tunnels to.

use std::collections::BTreeSet;
use std::fmt;

use adam_error::{Classify, ErrorClass};

/// The longest a name may be, in bytes, without a trailing dot (RFC 1035).
const MAX_NAME: usize = 253;
/// The longest a label may be, in bytes (RFC 1035).
const MAX_LABEL: usize = 63;

/// A DNS name as the proxy accepts it: ASCII letters, digits and hyphens in dot-separated labels,
/// lowercased, with at most one trailing dot taken off. A name that is not exactly this is not a
/// host the proxy tunnels to, so nothing downstream has to wonder what it looks at.
///
/// The last label is never all digits (RFC 3696): `127.0.0.1`, `2130706433` and `127.1` are
/// addresses that a resolver would read, not names.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HostName(String);

/// Why a text is not a [`HostName`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum NameError {
    /// Nothing, or only a dot.
    #[error("it is empty")]
    Empty,
    /// More than 253 bytes.
    #[error("it is longer than {MAX_NAME} bytes")]
    TooLong,
    /// Two dots in a row, or a leading dot.
    #[error("it has an empty label")]
    EmptyLabel,
    /// A label of more than 63 bytes.
    #[error("it has a label longer than {MAX_LABEL} bytes")]
    LabelTooLong,
    /// A byte that is no letter, digit or hyphen (a name outside ASCII has to be its `xn--` form).
    #[error("it has a character that is not a letter, a digit, a hyphen or a dot")]
    Character,
    /// A label that starts or ends with a hyphen.
    #[error("it has a label that starts or ends with a hyphen")]
    Hyphen,
    /// A last label of digits only: an address, not a name.
    #[error("its last label is all digits, so it reads as an address")]
    Numeric,
}

impl Classify for NameError {
    fn class(&self) -> ErrorClass {
        ErrorClass::Invalid
    }
}

impl HostName {
    /// Read `text` as a name.
    ///
    /// # Errors
    ///
    /// [`NameError`] saying what is wrong with it.
    pub fn parse(text: &str) -> Result<Self, NameError> {
        let name = text.strip_suffix('.').unwrap_or(text);
        if name.is_empty() {
            return Err(NameError::Empty);
        }
        if name.len() > MAX_NAME {
            return Err(NameError::TooLong);
        }
        let mut last = "";
        for label in name.split('.') {
            if label.is_empty() {
                return Err(NameError::EmptyLabel);
            }
            if label.len() > MAX_LABEL {
                return Err(NameError::LabelTooLong);
            }
            if !label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            {
                return Err(NameError::Character);
            }
            if label.starts_with('-') || label.ends_with('-') {
                return Err(NameError::Hyphen);
            }
            last = label;
        }
        if last.bytes().all(|b| b.is_ascii_digit()) {
            return Err(NameError::Numeric);
        }
        Ok(Self(name.to_ascii_lowercase()))
    }

    /// The name, lowercase, without a trailing dot.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HostName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An entry of the allow-list that is not a name or a `.name`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AllowError {
    /// The entry is not a valid name. A `*.` prefix is the usual mistake and gets its hint.
    #[error(
        "allow-list entry {entry:?} is not a host name ({reason}); write `example.com` for one host \
         and `.example.com` for every subdomain of it"
    )]
    Name {
        /// The entry as written.
        entry: String,
        /// What is wrong with it.
        reason: NameError,
    },
}

impl Classify for AllowError {
    fn class(&self) -> ErrorClass {
        ErrorClass::Invalid
    }
}

/// Which hosts the proxy tunnels to: exact names, and suffixes that cover every subdomain.
///
/// * `example.com` matches `example.com` and nothing else.
/// * `.example.com` matches `a.example.com` and `a.b.example.com`, and **not** `example.com`
///   itself: list the apex as well when it is wanted. Mind what a suffix covers: everything below
///   it, so name the registrable domain (`.example.com`, not `.com`).
///
/// An empty list matches nothing and is refused where the configuration is read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AllowList {
    exact: BTreeSet<HostName>,
    /// Kept with the leading dot (`.example.com`).
    suffixes: BTreeSet<String>,
}

impl AllowList {
    /// An empty list.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one entry: `example.com` or `.example.com`, in any case, with or without a trailing dot.
    ///
    /// # Errors
    ///
    /// [`AllowError`] when the entry is neither.
    pub fn add(&mut self, entry: &str) -> Result<(), AllowError> {
        let entry = entry.trim();
        let (suffix, name) = match entry.strip_prefix('.') {
            Some(rest) => (true, rest),
            None => (false, entry),
        };
        let host = HostName::parse(name).map_err(|reason| AllowError::Name {
            entry: entry.to_owned(),
            reason,
        })?;
        if suffix {
            self.suffixes.insert(format!(".{host}"));
        } else {
            self.exact.insert(host);
        }
        Ok(())
    }

    /// Whether `host` is on the list.
    pub fn allows(&self, host: &HostName) -> bool {
        if self.exact.contains(host) {
            return true;
        }
        // Every proper suffix of the name that starts at a dot: `a.b.c` has `.b.c` and `.c`.
        host.as_str()
            .match_indices('.')
            .any(|(at, _)| self.suffixes.contains(&host.as_str()[at..]))
    }

    /// How many entries the list holds.
    pub fn len(&self) -> usize {
        self.exact.len() + self.suffixes.len()
    }

    /// Whether the list holds nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The entries as they are written, sorted: names first, then the suffixes.
    pub fn entries(&self) -> impl Iterator<Item = String> + '_ {
        self.exact
            .iter()
            .map(ToString::to_string)
            .chain(self.suffixes.iter().cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(text: &str) -> HostName {
        HostName::parse(text).unwrap_or_else(|e| panic!("{text:?}: {e}"))
    }

    fn list(entries: &[&str]) -> AllowList {
        let mut list = AllowList::new();
        for entry in entries {
            list.add(entry).unwrap_or_else(|e| panic!("{entry:?}: {e}"));
        }
        list
    }

    #[test]
    fn a_name_is_lowercased_and_loses_one_trailing_dot() {
        assert_eq!(name("Registry.NPMjs.org.").as_str(), "registry.npmjs.org");
        assert_eq!(name("localhost").as_str(), "localhost");
        assert_eq!(
            name("xn--bcher-kva.example").as_str(),
            "xn--bcher-kva.example"
        );
        assert_eq!(name("a-b.c1.io").as_str(), "a-b.c1.io");
        assert_eq!(name("1password.com").as_str(), "1password.com");
    }

    #[test]
    fn what_is_not_a_name_is_refused_for_the_reason_it_is_not() {
        let long_label = "a".repeat(64);
        let long_name = format!("{}.com", vec!["a".repeat(60); 5].join("."));
        let cases: &[(&str, NameError)] = &[
            ("", NameError::Empty),
            (".", NameError::Empty),
            ("a..b", NameError::EmptyLabel),
            (".a.b", NameError::EmptyLabel),
            ("a.b..", NameError::EmptyLabel),
            (&long_label, NameError::LabelTooLong),
            (&long_name, NameError::TooLong),
            ("exa_mple.com", NameError::Character),
            ("exa mple.com", NameError::Character),
            ("example.com/", NameError::Character),
            ("user@example.com", NameError::Character),
            ("example.com:443", NameError::Character),
            ("*.example.com", NameError::Character),
            ("bücher.example", NameError::Character),
            ("[::1]", NameError::Character),
            ("-a.com", NameError::Hyphen),
            ("a-.com", NameError::Hyphen),
            ("127.0.0.1", NameError::Numeric),
            ("1.2.3", NameError::Numeric),
            ("2130706433", NameError::Numeric),
            ("example.123", NameError::Numeric),
        ];
        for (text, want) in cases {
            assert_eq!(HostName::parse(text), Err(*want), "{text:?}");
        }
    }

    #[test]
    fn an_exact_entry_matches_that_host_and_no_other() {
        let list = list(&["registry.npmjs.org"]);
        assert!(list.allows(&name("registry.npmjs.org")));
        assert!(list.allows(&name("REGISTRY.npmjs.org.")));
        assert!(!list.allows(&name("npmjs.org")));
        assert!(!list.allows(&name("evil.registry.npmjs.org")));
        assert!(!list.allows(&name("registry.npmjs.org.evil.com")));
        assert!(!list.allows(&name("xregistry.npmjs.org")));
    }

    #[test]
    fn a_suffix_entry_matches_every_subdomain_and_not_the_apex() {
        let list = list(&[".githubusercontent.com"]);
        assert!(list.allows(&name("objects.githubusercontent.com")));
        assert!(list.allows(&name("a.b.githubusercontent.com")));
        assert!(!list.allows(&name("githubusercontent.com")));
        assert!(!list.allows(&name("evilgithubusercontent.com")));
        assert!(!list.allows(&name("githubusercontent.com.evil.org")));
        assert!(!list.allows(&name("x.githubusercontent.org")));
    }

    #[test]
    fn the_apex_and_its_subdomains_are_two_entries() {
        let list = list(&["example.com", ".example.com"]);
        assert!(list.allows(&name("example.com")));
        assert!(list.allows(&name("www.example.com")));
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn entries_are_normalised_trimmed_and_counted_once() {
        let list = list(&[
            " Example.COM. ",
            "example.com",
            ".Example.com",
            ".example.com.",
        ]);
        assert_eq!(list.len(), 2);
        assert_eq!(
            list.entries().collect::<Vec<_>>(),
            ["example.com", ".example.com"]
        );
        assert!(!list.is_empty());
        assert!(AllowList::new().is_empty());
        assert!(!AllowList::new().allows(&name("example.com")));
    }

    #[test]
    fn an_entry_that_is_no_name_is_refused_and_named() {
        for entry in [
            "",
            ".",
            "..example.com",
            "*.example.com",
            "https://example.com",
            "example.com:443",
            "example.com/path",
            "10.0.0.1",
            "bad_name.com",
        ] {
            let e = AllowList::new().add(entry).unwrap_err();
            assert!(e.to_string().contains(&format!("{entry:?}")), "{e}");
            assert_eq!(e.class(), ErrorClass::Invalid);
        }
        let hint = AllowList::new()
            .add("*.example.com")
            .unwrap_err()
            .to_string();
        assert!(hint.contains(".example.com"), "{hint}");
    }
}
