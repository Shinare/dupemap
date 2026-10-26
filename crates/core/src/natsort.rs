//! Natural, case-insensitive name ordering: `IMG_2` < `IMG_10`, `apple` < `Banana`.

use std::cmp::Ordering;

/// Compare two names the way a file browser would: digit runs compare as numbers, case is
/// ignored, and accented Latin letters sort next to their base letter (`Ćma` right after `cma`).
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    cmp_level(a, b, primary)
        .then_with(|| cmp_level(a, b, secondary))
        // equal apart from case / leading zeros: fall back to a stable, exact order
        .then_with(|| a.cmp(b))
}

fn cmp_level(a: &str, b: &str, fold: fn(char) -> char) -> Ordering {
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let na = take_digits(&mut ai);
                let nb = take_digits(&mut bi);
                let ta = na.trim_start_matches('0');
                let tb = nb.trim_start_matches('0');
                let o = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if o != Ordering::Equal {
                    return o;
                }
            }
            (Some(x), Some(y)) => {
                let o = fold(x).cmp(&fold(y));
                if o != Ordering::Equal {
                    return o;
                }
                ai.next();
                bi.next();
            }
        }
    }
}

fn take_digits(it: &mut std::iter::Peekable<std::str::Chars>) -> String {
    let mut s = String::new();
    while let Some(&c) = it.peek() {
        if c.is_ascii_digit() {
            s.push(c);
            it.next();
        } else {
            break;
        }
    }
    s
}

/// Lower case, accents removed for Latin-1 and Latin Extended-A letters.
fn primary(c: char) -> char {
    let u = c as u32;
    const L1: &[u8; 64] = b"aaaaaaaceeeeiiiidnooooo*ouuuuytsaaaaaaaceeeeiiiidnooooo*ouuuuyty";
    const EXT_A: &[u8; 128] = b"aaaaaaccccccccddddeeeeeeeeeegggggggghhhhiiiiiiiiiiiijjkkkllllllllllnnnnnnnnnoooooooorrrrrrssssssssttttttuuuuuuuuuuuuwwyyyzzzzzzs";
    if (0xC0..=0xFF).contains(&u) {
        let m = L1[(u - 0xC0) as usize];
        if m != b'*' {
            return m as char;
        }
    } else if (0x100..=0x17F).contains(&u) {
        return EXT_A[(u - 0x100) as usize] as char;
    }
    secondary(c)
}

/// Lower case only.
fn secondary(c: char) -> char {
    if c.is_ascii() {
        c.to_ascii_lowercase()
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_like_a_file_browser() {
        let mut v = vec!["IMG_10.jpg", "img_2.jpg", "IMG_1.jpg", "Banana", "apple", "img_02.jpg", "Ćma", "cma", "a"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, vec!["a", "apple", "Banana", "cma", "Ćma", "IMG_1.jpg", "img_02.jpg", "img_2.jpg", "IMG_10.jpg"]);
    }

    #[test]
    fn is_a_total_order_on_distinct_strings() {
        assert_eq!(natural_cmp("x", "x"), Ordering::Equal);
        assert_ne!(natural_cmp("File", "file"), Ordering::Equal);
        assert_eq!(natural_cmp("File", "file"), natural_cmp("file", "File").reverse());
    }

    #[test]
    fn polish_letters_sort_with_their_base_letter() {
        let mut v = vec!["Żaba", "zebra", "łódka", "lody", "mama", "ósemka", "osa", "Śliwka", "sok", "ćma"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, vec!["ćma", "łódka", "lody", "mama", "osa", "ósemka", "Śliwka", "sok", "Żaba", "zebra"]);
    }
}
