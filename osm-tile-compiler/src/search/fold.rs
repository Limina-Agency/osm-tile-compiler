//! The text folding names and queries share: the index stores folded words,
//! so changing this changes which queries match and needs a new format
//! version.
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

/// `text` as the search index compares it: lowercase words separated by
/// single spaces, without diacritics. Letters that do not decompose into a
/// base letter and a mark are mapped by hand (`ł` to `l`, `ß` to `ss`, `ø`
/// to `o`, `æ` to `ae`), and anything that is not a letter or digit
/// separates words, so `Bielsko-Biała`, `bielsko biala` and `BIELSKO/BIAŁA`
/// fold the same. Apps can fold text the same way for highlighting.
pub fn fold(text: &str) -> String {
    let mut folded = String::with_capacity(text.len());
    let mut space = false;
    for c in text.chars().flat_map(char::to_lowercase) {
        let mapped: &str = match c {
            'ł' => "l",
            'đ' | 'ð' => "d",
            'ø' => "o",
            'ß' => "ss",
            'æ' => "ae",
            'œ' => "oe",
            'þ' => "th",
            'ı' => "i",
            'ħ' => "h",
            'ŧ' => "t",
            'ŋ' => "n",
            _ => "",
        };
        if !mapped.is_empty() {
            if space && !folded.is_empty() {
                folded.push(' ');
            }
            space = false;
            folded.push_str(mapped);
            continue;
        }
        for d in c.nfkd() {
            if is_combining_mark(d) {
                continue;
            }
            if d.is_alphanumeric() {
                if space && !folded.is_empty() {
                    folded.push(' ');
                }
                space = false;
                // NFKD can give capitals back (ligatures, Roman numerals).
                folded.extend(d.to_lowercase());
            } else {
                space = true;
            }
        }
    }
    folded
}

/// The words of `text`, folded, each once, in order.
pub(super) fn words(text: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for word in fold(text).split(' ').filter(|w| !w.is_empty()) {
        if !words.iter().any(|w| w == word) {
            words.push(word.to_string());
        }
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_polish_and_other_letters() {
        assert_eq!(fold("Łódź"), "lodz");
        assert_eq!(fold("ŻÓŁĆ gęślą jaźń"), "zolc gesla jazn");
        assert_eq!(fold("Bielsko-Biała"), "bielsko biala");
        assert_eq!(fold("  Hala   Gąsienicowa! "), "hala gasienicowa");
        assert_eq!(fold("Straße"), "strasse");
        assert_eq!(fold("Søndre Ærø"), "sondre aero");
        assert_eq!(fold("Św. Jana"), "sw jana");
        assert_eq!(fold("İstanbul"), "istanbul");
        assert_eq!(fold("ﬁord"), "fiord");
        assert_eq!(fold("Москва"), "москва");
        assert_eq!(fold("Ⅻ"), "xii");
        assert_eq!(fold(""), "");
        assert_eq!(fold("--"), "");
    }

    #[test]
    fn words_are_unique() {
        assert_eq!(words("Nowy Nowy Targ"), ["nowy", "targ"]);
    }
}
