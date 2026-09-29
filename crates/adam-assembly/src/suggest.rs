//! "Did you mean": the closest name to a mistake, for error messages.

/// The candidate closest to `name`, when one is close enough to be a plausible typo.
///
/// Close means: equal ignoring case (`Read` for `read`), one contains the other (`read` for
/// `read_diff`, when the shorter has at least three characters), or a small edit distance
/// (a third of the length, at least one and at most three). The first of equally close
/// candidates wins, so the answer follows the order the caller lists them in. A change that
/// rewrites every character is never suggested.
pub(crate) fn closest<'a>(
    name: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<&'a str> {
    let wanted = name.to_lowercase();
    let len = wanted.chars().count();
    let limit = (len / 3).clamp(1, 3);
    let mut best: Option<(usize, &'a str)> = None;
    for candidate in candidates {
        let other = candidate.to_lowercase();
        let other_len = other.chars().count();
        let distance = edit_distance(&wanted, &other);
        // An edit that rewrites every character is not a typo.
        let score = if other == wanted {
            Some(0)
        } else if distance <= limit && distance < len.min(other_len) {
            Some(distance)
        } else if len.min(other_len) >= 3 && (other.contains(&wanted) || wanted.contains(&other)) {
            // One name inside the other: as good as the worst typo, and worse than a real one.
            Some(limit)
        } else {
            None
        };
        if let Some(score) = score
            && best.is_none_or(|(b, _)| score < b)
        {
            best = Some((score, candidate));
        }
    }
    best.map(|(_, candidate)| candidate)
}

/// Levenshtein distance over characters.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = if ca == *cb {
                diagonal
            } else {
                1 + diagonal.min(above).min(row[j])
            };
            diagonal = above;
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_distances() {
        assert_eq!(edit_distance("", ""), 0);
        assert_eq!(edit_distance("abc", ""), 3);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("run_check", "run_checks"), 1);
        assert_eq!(edit_distance("é", "e"), 1);
    }

    #[test]
    fn typos_are_suggested() {
        let tools = ["prepare_workspace", "run_checks", "ask_user"];
        assert_eq!(closest("run_check", tools), Some("run_checks"));
        assert_eq!(closest("ask_usr", tools), Some("ask_user"));
        assert_eq!(
            closest("prepare_workspce", tools),
            Some("prepare_workspace")
        );
    }

    #[test]
    fn case_and_containment_count() {
        assert_eq!(closest("Read", ["read", "write"]), Some("read"));
        assert_eq!(
            closest("read", ["read_diff", "list_files"]),
            Some("read_diff")
        );
        // Two characters are too few to call one a prefix of the other.
        assert_eq!(closest("re", ["read_diff"]), None);
    }

    #[test]
    fn the_nearest_wins_and_the_first_breaks_a_tie() {
        assert_eq!(closest("abcd", ["abxd", "abcd_"]), Some("abxd"));
        assert_eq!(closest("abcd", ["abce", "abcf"]), Some("abce"));
        // An exact (case-insensitive) match beats a near one that comes first.
        assert_eq!(closest("abcd", ["abcde", "ABCD"]), Some("ABCD"));
    }

    #[test]
    fn nothing_far_is_suggested() {
        assert_eq!(closest("zzz", ["prepare_workspace", "ask_user"]), None);
        assert_eq!(closest("x", Vec::<&str>::new()), None);
        assert_eq!(closest("", ["a"]), None);
    }
}
