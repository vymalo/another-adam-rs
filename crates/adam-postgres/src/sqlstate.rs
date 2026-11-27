//! adam-rs's own SQLSTATEs: the refusals the store's database functions raise
//! ([ADR 0082](../../docs/decisions/0082-the-sql-adapters-keep-their-rules-in-the-database.md),
//! decision 4).
//!
//! A function of the store's schema refuses with `RAISE .. USING ERRCODE = 'AD00n'`. This table is
//! the one place that says what each code means to a caller: the [`ErrorClass`] it stands for
//! here, and, in `adam-store-postgres`, the `StoreError` variant the store builds from it.
//!
//! | SQLSTATE | Raised when | `StoreError` | Class |
//! |---|---|---|---|
//! | `AD001` | a body row's index is not its position, an append starts past the rows, a head has no body | `InvalidInput` | `Invalid` |
//! | `AD002` | the journal holds another step at this sequence number; the recorded name is the error's `DETAIL` | `NonDeterminism` | `Corrupt` |
//! | `AD003` | stored data breaks an invariant (a collected body slot is still occupied) | `Corrupt` | `Corrupt` |
//! | `AD004` | a live attempt wrote a different body row; raised after rows were written, so the commit rolls back | `Conflict` | `Conflict` |
//!
//! The codes use the class `AD`, which PostgreSQL does not use (Appendix A of its manual), and none
//! ends in `000`, which marks a category code. The enum is closed on purpose: a new code is a new
//! variant, and every `match` over it, the store's included, fails to compile until it says what the
//! code means.

use adam_error::ErrorClass;

/// The SQLSTATE class every code of [`AdamSqlState`] belongs to.
pub const ADAM_SQLSTATE_CLASS: &str = "AD";

/// One of adam-rs's own SQLSTATEs (class `AD`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AdamSqlState {
    /// `AD001`: the caller's input cannot be stored (a body row out of place, an append past the
    /// rows, a head without a body). `StoreError::InvalidInput`, class `Invalid`.
    InvalidInput,
    /// `AD002`: the journal holds another step at this sequence number; the error's `DETAIL` is the
    /// recorded step's name. `StoreError::NonDeterminism`, class `Corrupt`.
    NonDeterminism,
    /// `AD003`: stored data breaks an invariant. `StoreError::Corrupt`, class `Corrupt`.
    Corrupt,
    /// `AD004`: a live attempt wrote a different body row. Raised after rows were written, so the
    /// abort rolls them back. `StoreError::Conflict`, class `Conflict`.
    Conflict,
}

impl AdamSqlState {
    /// Every code, in the order of the table.
    pub const ALL: [Self; 4] = [
        Self::InvalidInput,
        Self::NonDeterminism,
        Self::Corrupt,
        Self::Conflict,
    ];

    /// The five-character SQLSTATE a function raises.
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidInput => "AD001",
            Self::NonDeterminism => "AD002",
            Self::Corrupt => "AD003",
            Self::Conflict => "AD004",
        }
    }

    /// The class a caller decides from (`adam_error::Classify`).
    pub const fn class(self) -> ErrorClass {
        match self {
            Self::InvalidInput => ErrorClass::Invalid,
            Self::NonDeterminism | Self::Corrupt => ErrorClass::Corrupt,
            Self::Conflict => ErrorClass::Conflict,
        }
    }

    /// The code a SQLSTATE stands for, or `None` for any other SQLSTATE (PostgreSQL's own, or an
    /// `AD` code this release does not know).
    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.code() == code)
    }

    /// The code a driver error carries, if it is one of these.
    pub fn of(err: &tokio_postgres::Error) -> Option<Self> {
        err.code().and_then(|code| Self::from_code(code.code()))
    }
}

impl std::fmt::Display for AdamSqlState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table of ADR 0082 decision 4, row by row: the code and the class.
    #[test]
    fn each_code_has_the_class_of_the_table() {
        let table = [
            ("AD001", ErrorClass::Invalid),
            ("AD002", ErrorClass::Corrupt),
            ("AD003", ErrorClass::Corrupt),
            ("AD004", ErrorClass::Conflict),
        ];
        assert_eq!(AdamSqlState::ALL.len(), table.len());
        for (state, (code, class)) in AdamSqlState::ALL.into_iter().zip(table) {
            assert_eq!(state.code(), code);
            assert_eq!(state.class(), class, "{code}");
            assert_eq!(state.to_string(), code);
        }
    }

    #[test]
    fn a_code_reads_back_as_itself_and_nothing_else_reads_as_one() {
        for state in AdamSqlState::ALL {
            assert_eq!(AdamSqlState::from_code(state.code()), Some(state));
        }
        // PostgreSQL's own codes, the category code of the class, an unknown code of the class,
        // and the lower-case spelling are none of ours.
        for other in [
            "23505", "40001", "P0001", "AD000", "AD005", "ad001", "AD01", "",
        ] {
            assert_eq!(AdamSqlState::from_code(other), None, "{other:?}");
        }
    }

    /// The rules of a custom SQLSTATE: five digits or upper-case letters, the class `AD`, unique,
    /// and never a category code (`..000`).
    #[test]
    fn the_codes_are_well_formed_and_distinct() {
        let mut seen = std::collections::HashSet::new();
        for state in AdamSqlState::ALL {
            let code = state.code();
            assert_eq!(code.len(), 5, "{code}");
            assert!(
                code.chars()
                    .all(|c| c.is_ascii_digit() || c.is_ascii_uppercase()),
                "{code}"
            );
            assert!(code.starts_with(ADAM_SQLSTATE_CLASS), "{code}");
            assert!(!code.ends_with("000"), "{code} is a category code");
            assert!(seen.insert(code), "{code} twice");
        }
    }
}
