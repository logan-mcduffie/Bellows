//! Detect checkout paths that a compile baked into its outputs.
//!
//! Every miss compiles with `--remap-path-prefix` for the workspace and target
//! roots, so a remapped build of path-independent code contains neither root.
//! Any remaining occurrence came from the program itself, such as
//! `env!("CARGO_MANIFEST_DIR")` used as a value or a proc macro that
//! read a path. Such outputs are only valid in the checkout that produced them.
use memchr::memmem::Finder;

/// Minimum root length that can be searched without matching unrelated bytes.
/// Shorter roots, such as `/w`, are treated as always present.
const MIN_NEEDLE: usize = 6;

pub struct Scanner {
    finders: Vec<Finder<'static>>,
    always: bool,
    case_insensitive: bool,
}

impl Scanner {
    pub fn new(roots: &[String]) -> Self {
        let case_insensitive = cfg!(windows);
        let mut needles = std::collections::BTreeSet::new();
        let mut always = false;
        for root in roots {
            let root = root.trim_end_matches(['/', '\\']);
            if root.len() < MIN_NEEDLE {
                always = true;
                continue;
            }
            let mut spellings = vec![root.to_owned()];
            if cfg!(windows) {
                spellings.push(root.replace('\\', "/"));
                spellings.push(root.replace('\\', "\\\\"));
            }
            for spelling in spellings {
                let spelling = if case_insensitive {
                    spelling.to_ascii_lowercase()
                } else {
                    spelling
                };
                if cfg!(windows) {
                    // Wide strings (PE resources, some PDB records).
                    let wide = spelling
                        .encode_utf16()
                        .flat_map(u16::to_le_bytes)
                        .collect::<Vec<_>>();
                    needles.insert(wide);
                }
                needles.insert(spelling.into_bytes());
            }
        }
        Self {
            finders: needles
                .into_iter()
                .map(|needle| Finder::new(&needle).into_owned())
                .collect(),
            always,
            case_insensitive,
        }
    }

    pub fn leaks(&self, bytes: &[u8]) -> bool {
        if self.always {
            return true;
        }
        if self.case_insensitive {
            let lowered = bytes.to_ascii_lowercase();
            return self.finders.iter().any(|f| f.find(&lowered).is_some());
        }
        self.finders
            .iter()
            .any(|finder| finder.find(bytes).is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_embedded_roots_and_ignores_remapped_paths() {
        let scanner = Scanner::new(&[
            "/home/dev/checkout-a".into(),
            "/home/dev/checkout-a/target".into(),
        ]);
        assert!(scanner.leaks(b"\0\x01/home/dev/checkout-a/crates/x/fixtures\0"));
        assert!(!scanner.leaks(b"/bellows/workspace/crates/x/src/lib.rs"));
        assert!(!scanner.leaks(b"/home/dev/checkout-b/crates/x"));
        assert!(Scanner::new(&["/w".into()]).leaks(b"anything"));
    }
}
