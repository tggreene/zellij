/// Given a list of tab names in tab-position order, return one hint character per tab.
/// Algorithm:
///   1. First pass: prefer the first ASCII alphabetic char of the tab name (lowercased) if it's
///      not already taken by an earlier tab.
///   2. Second pass for tabs that didn't get a hint: try each subsequent ASCII alphabetic char
///      of the tab name (lowercased) until an unused one is found.
///   3. Third pass: fall back to digits 1..=9, 0, then unused letters of the alphabet a..=z.
///   4. If everything is taken, return None for that tab.
pub fn assign_tab_hints(tab_names: &[&str]) -> Vec<Option<char>> {
    let mut used: std::collections::HashSet<char> = std::collections::HashSet::new();
    let mut hints: Vec<Option<char>> = vec![None; tab_names.len()];

    // First pass: try first alphabetic char of each name
    for (i, name) in tab_names.iter().enumerate() {
        if let Some(c) = name.chars().find(|c| c.is_ascii_alphabetic()) {
            let c = c.to_ascii_lowercase();
            if !used.contains(&c) {
                used.insert(c);
                hints[i] = Some(c);
            }
        }
    }

    // Second pass: for tabs without a hint, try all subsequent alphabetic chars in name
    for (i, name) in tab_names.iter().enumerate() {
        if hints[i].is_some() {
            continue;
        }
        let mut found = false;
        // skip first char (already tried in pass 1), try rest
        let mut chars = name.chars().filter(|c| c.is_ascii_alphabetic());
        chars.next(); // skip first
        for c in chars {
            let c = c.to_ascii_lowercase();
            if !used.contains(&c) {
                used.insert(c);
                hints[i] = Some(c);
                found = true;
                break;
            }
        }
        let _ = found;
    }

    // Third pass: fallback pool - digits 1..=9, 0, then a..=z
    let fallback: Vec<char> = "123456789"
        .chars()
        .chain(std::iter::once('0'))
        .chain('a'..='z')
        .collect();

    for hint in hints.iter_mut() {
        if hint.is_none() {
            if let Some(&c) = fallback.iter().find(|c| !used.contains(c)) {
                used.insert(c);
                *hint = Some(c);
            }
        }
    }

    hints
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_typical_tab_names() {
        let names = ["fish", "nvim", "htop"];
        let hints = assign_tab_hints(&names);
        assert_eq!(hints[0], Some('f'));
        assert_eq!(hints[1], Some('n'));
        assert_eq!(hints[2], Some('h'));
        // all distinct
        let non_none: Vec<char> = hints.into_iter().flatten().collect();
        assert_eq!(non_none.len(), 3);
        let set: std::collections::HashSet<char> = non_none.into_iter().collect();
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn test_all_same_first_letter() {
        // All start with 'f' - second pass should find other chars
        let names = ["fish", "foo", "far"];
        let hints = assign_tab_hints(&names);
        // first gets 'f', second and third need fallback chars from name or pool
        assert_eq!(hints[0], Some('f'));
        // second tab "foo" -> 'o' (second char)
        assert_eq!(hints[1], Some('o'));
        // third tab "far" -> 'a'
        assert_eq!(hints[2], Some('a'));
        let non_none: Vec<char> = hints.into_iter().flatten().collect();
        let set: std::collections::HashSet<char> = non_none.into_iter().collect();
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn test_empty_names_fall_back_to_pool() {
        let names = ["", "", ""];
        let hints = assign_tab_hints(&names);
        // No alpha chars in name -> falls through to digit/letter pool
        assert_eq!(hints[0], Some('1'));
        assert_eq!(hints[1], Some('2'));
        assert_eq!(hints[2], Some('3'));
    }

    #[test]
    fn test_more_tabs_than_unique_hints() {
        // 37 tabs all named "" -> pool exhausted after 36 (9 digits + 26 letters + '0')
        // Actually pool has 36 chars (1-9=9, 0=1, a-z=26 -> 36 total)
        let names: Vec<&str> = std::iter::repeat("").take(37).collect();
        let hints = assign_tab_hints(&names);
        let non_none_count = hints.iter().filter(|h| h.is_some()).count();
        // 36 unique hints available (digits 1-9, 0, a-z)
        assert_eq!(non_none_count, 36);
        assert_eq!(hints[36], None);
    }
}
