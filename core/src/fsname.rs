//! Turning Drive names into names the local filesystem accepts, on every OS.
//!
//! Drive allows almost anything in a file name (`Q3: plan?.docx`, `CON`, trailing
//! dots, 500-character names). Linux and macOS accept most of that; Windows does
//! not. The rules are pure functions of the name so they can be unit-tested on
//! any platform – only [`local_name`] looks at the OS it runs on.

/// Longest local name we create, in UTF-8 bytes. The real limit is 255, but
/// callers append `" (n)"` for duplicates and `".cloudy-part"` while
/// downloading, so we leave headroom. (UTF-16 units never exceed UTF-8 bytes,
/// so the same limit is safe for Windows' 255-unit rule.)
pub const MAX_NAME_BYTES: usize = 200;

/// A name that is safe to create on the OS Cloudy is running on.
pub fn local_name(name: &str) -> String {
    sanitize(name, cfg!(windows))
}

/// `windows_rules == false`: only `/` and NUL are replaced (all Unix needs).
/// `windows_rules == true`: also `\ : * ? " < > |` and control characters,
/// trailing dots/spaces, and reserved device names (`CON`, `NUL`, `COM1`, …).
pub fn sanitize(name: &str, windows_rules: bool) -> String {
    let replaced: String = name
        .chars()
        .map(|c| {
            let bad = c == '/'
                || c == '\0'
                || (windows_rules
                    && (c.is_control() || matches!(c, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')));
            if bad { '_' } else { c }
        })
        .collect();

    let mut s = truncate_keep_ext(&replaced, MAX_NAME_BYTES);

    if windows_rules {
        // Windows silently drops trailing dots and spaces, which makes two
        // different Drive names collide (and confuses Explorer).
        s = s.trim_end_matches(['.', ' ']).to_string();
        // Reserved device names are reserved even with an extension: `con.txt`.
        let stem = s.split('.').next().unwrap_or("").trim_end();
        if is_windows_reserved(stem) {
            s.insert(0, '_');
        }
    }

    if s.is_empty() || s == "." || s == ".." {
        "_".to_string()
    } else {
        s
    }
}

fn is_windows_reserved(stem: &str) -> bool {
    let up = stem.to_ascii_uppercase();
    matches!(up.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (up.len() == 4
            && (up.starts_with("COM") || up.starts_with("LPT"))
            && matches!(up.as_bytes()[3], b'1'..=b'9'))
}

/// Shorten to `max` bytes without cutting a character in half, keeping a
/// short extension (`.jpg`, `.tar.gz`'s last part) so the file stays openable.
fn truncate_keep_ext(name: &str, max: usize) -> String {
    if name.len() <= max {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 && name.len() - i <= 16 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let mut end = max.saturating_sub(ext.len()).min(stem.len());
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &stem[..end], ext)
}

/// Files and folders the OS creates on its own. Uploading them is just clutter
/// (and a `Thumbs.db` in your Drive helps nobody), so folder uploads skip them.
pub fn is_os_junk(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        ".ds_store"
            | ".spotlight-v100"
            | ".trashes"
            | ".fseventsd"
            | "thumbs.db"
            | "ehthumbs.db"
            | "desktop.ini"
            | "$recycle.bin"
            | "system volume information"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_rules_only_touch_slash_and_nul() {
        assert_eq!(sanitize("a/b", false), "a_b");
        assert_eq!(sanitize("x\0y", false), "x_y");
        assert_eq!(sanitize("Q3: plan?.docx", false), "Q3: plan?.docx");
        assert_eq!(sanitize("CON", false), "CON");
        assert_eq!(sanitize("trailing. ", false), "trailing. ");
        for degenerate in ["", ".", ".."] {
            assert_eq!(sanitize(degenerate, false), "_");
        }
    }

    #[test]
    fn windows_rules_replace_forbidden_characters() {
        assert_eq!(sanitize("report: Q3 <final>?.docx", true), "report_ Q3 _final__.docx");
        assert_eq!(sanitize(r#"a\b|c*d"e"#, true), "a_b_c_d_e");
        assert_eq!(sanitize("tab\there", true), "tab_here");
        assert_eq!(sanitize("fine name (1).jpg", true), "fine name (1).jpg");
    }

    #[test]
    fn windows_rules_strip_trailing_dots_and_spaces() {
        assert_eq!(sanitize("name. ", true), "name");
        assert_eq!(sanitize("name...", true), "name");
        assert_eq!(sanitize("a .txt", true), "a .txt", "only a trailing dot/space is a problem");
        for degenerate in ["...", " ", ". ."] {
            assert_eq!(sanitize(degenerate, true), "_", "{degenerate:?}");
        }
    }

    #[test]
    fn windows_reserved_device_names_are_prefixed() {
        for n in ["CON", "con", "Nul.txt", "AUX.tar.gz", "PRN", "COM1", "lpt9.log", "CON.", "con .txt"] {
            let out = sanitize(n, true);
            assert!(out.starts_with('_'), "{n:?} -> {out:?}");
            assert_eq!(out.len(), n.trim_end_matches(['.', ' ']).len() + 1, "only the prefix is added: {n:?} -> {out:?}");
        }
        for n in ["COM0", "COM10", "Console.txt", "Conference", "auxiliary.txt", "LPT"] {
            assert_eq!(sanitize(n, true), n, "{n:?} is a legal name");
        }
    }

    #[test]
    fn long_names_keep_their_extension_and_stay_valid_utf8() {
        let long = format!("{}.jpg", "a".repeat(500));
        let out = sanitize(&long, false);
        assert!(out.len() <= MAX_NAME_BYTES && out.ends_with(".jpg"), "{}", out.len());

        // 'é' is 2 bytes: cutting at an odd byte offset must not split it.
        let multi = format!("{}.txt", "é".repeat(300));
        let out = sanitize(&multi, true);
        assert!(out.len() <= MAX_NAME_BYTES && out.ends_with(".txt"));
        assert!(out.starts_with('é'));

        // A "dot" far from the end is not an extension; plain truncation.
        let dotty = format!("a.{}", "b".repeat(300));
        assert_eq!(sanitize(&dotty, false).len(), MAX_NAME_BYTES);

        // Room is left for the " (n)" and ".cloudy-part" suffixes added elsewhere.
        assert!(MAX_NAME_BYTES + " (9999)".len() + ".cloudy-part".len() <= 255);
    }

    #[test]
    fn sanitizing_twice_changes_nothing() {
        let long = "é".repeat(300);
        let samples = ["plain.txt", "a/b:c", "CON", "x. ", long.as_str(), "", "..", "COM1.txt", "☃ snow?.png"];
        for s in samples {
            for win in [false, true] {
                let once = sanitize(s, win);
                assert_eq!(sanitize(&once, win), once, "{s:?} (windows={win})");
            }
        }
    }

    #[test]
    fn os_junk_is_recognised_case_insensitively() {
        for n in [".DS_Store", "Thumbs.db", "THUMBS.DB", "desktop.ini", ".Trashes", "$RECYCLE.BIN"] {
            assert!(is_os_junk(n), "{n}");
        }
        for n in ["photo.jpg", "desktop.ini.bak", "._notes", ".gitignore", "Thumbs.db.txt"] {
            assert!(!is_os_junk(n), "{n} must be kept");
        }
    }
}
