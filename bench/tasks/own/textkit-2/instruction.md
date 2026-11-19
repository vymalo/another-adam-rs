# Slugs have repeated and dangling dashes

`slugify` is meant to produce clean URL slugs: lowercase letters and digits, with words joined by a single dash. It
works for plain input such as `Hello World`, but punctuation and extra spaces produce broken slugs:

| Input | Expected | Today |
|---|---|---|
| `Hello,   World!` | `hello-world` | `hello----world-` |
| `  Hello World  ` | `hello-world` | `--hello-world--` |
| `Rock & Roll` | `rock-roll` | `rock---roll` |

Fix it so that any run of characters that are not letters or digits becomes exactly one dash, and the slug never
starts or ends with a dash. Input with no letters or digits at all (for example `!!!`) gives an empty slug.
Everything that works today (accents removed, digits kept, lowercase) must keep working, and `npm test` must pass.
