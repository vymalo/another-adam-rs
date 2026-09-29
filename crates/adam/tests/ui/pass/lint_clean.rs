// What the macro generates must not trip the lints a strict crate turns on.
#![deny(warnings, missing_docs, unused_qualifications, trivial_casts, unreachable_pub)]
#![deny(clippy::all)]

//! A crate that denies everything.

use adam::prelude::*;

/// Shared.
pub struct Shared;

/// Documented tool.
#[tool]
pub async fn documented(
    _shared: State<Shared>,
    /// A value
    value: String,
) -> String {
    value
}

mod private {
    use adam::prelude::*;

    /// Private tools are fine too.
    #[tool]
    pub(crate) async fn hidden(
        /// A value
        value: Vec<u8>,
    ) -> Json<usize> {
        Json(value.len())
    }
}

fn main() {
    let _ = tools![Documented, private::Hidden];
}
