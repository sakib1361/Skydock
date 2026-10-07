//! Rules about file names that belong to no provider in particular.

/// A provider name as it appears locally. Providers allow names Linux
/// cannot hold (Google Drive permits `/`), so those are respelled.
pub(crate) fn local_name(name: &str) -> String {
    match name {
        "" | "." | ".." => "_".to_owned(),
        _ => name.replace(['/', '\0'], "_"),
    }
}

/// Scratch files that applications keep beside a document while it is open
/// (lock files, swap files, backups). They work like any other file here
/// but are never uploaded; renaming one to an ordinary name uploads it.
pub(crate) fn is_transient(name: &str) -> bool {
    const PREFIXES: [&str; 4] = [".~lock.", "~$", ".goutputstream-", ".#"];
    const SUFFIXES: [&str; 8] = [
        "~",
        ".swp",
        ".swo",
        ".swx",
        ".tmp",
        ".part",
        ".kate-swp",
        ".crdownload",
    ];
    PREFIXES.iter().any(|prefix| name.starts_with(prefix))
        || SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
}

/// Folders a file manager creates at the top of a volume to hold its own
/// wastebasket. Refused, so that "move to wastebasket" does not turn into
/// uploading a hidden folder; deleting uses the provider's recycle bin.
pub(crate) fn is_trash_folder(name: &str) -> bool {
    name == ".Trash" || name.starts_with(".Trash-")
}

/// Name for the copy that keeps this device's content when a file was also
/// changed elsewhere: `report (conflicted copy from laptop).docx`.
pub(crate) fn conflict_name(name: &str, device: &str) -> String {
    let note = format!(" (conflicted copy from {device})");
    match name.rfind('.') {
        Some(dot) if dot > 0 => format!("{}{note}{}", &name[..dot], &name[dot..]),
        _ => format!("{name}{note}"),
    }
}

/// This device's name, for [`conflict_name`].
pub(crate) fn device_name() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "this device".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_linux_cannot_hold_are_respelled() {
        assert_eq!(local_name("report.pdf"), "report.pdf");
        assert_eq!(local_name("a/b"), "a_b");
        assert_eq!(local_name(".."), "_");
    }

    #[test]
    fn editor_scratch_files_are_transient() {
        for name in [
            ".~lock.report.odt#",
            "~$report.docx",
            ".notes.txt.swp",
            "notes.txt~",
            ".goutputstream-AB12CD",
            "download.part",
        ] {
            assert!(is_transient(name), "{name}");
        }
        for name in ["report.odt", ".bashrc", "swp", "template.tmpl"] {
            assert!(!is_transient(name), "{name}");
        }
    }

    #[test]
    fn wastebasket_folders_are_recognised() {
        assert!(is_trash_folder(".Trash-1000") && is_trash_folder(".Trash"));
        assert!(!is_trash_folder(".Trashcan") && !is_trash_folder("Trash"));
    }

    #[test]
    fn conflict_copy_keeps_the_extension() {
        assert_eq!(
            conflict_name("report.docx", "laptop"),
            "report (conflicted copy from laptop).docx"
        );
        assert_eq!(
            conflict_name("Makefile", "laptop"),
            "Makefile (conflicted copy from laptop)"
        );
        assert_eq!(
            conflict_name(".profile", "laptop"),
            ".profile (conflicted copy from laptop)"
        );
    }
}
