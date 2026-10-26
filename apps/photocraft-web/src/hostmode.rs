//! Pure host-mode rules, shared by the bridge ([`crate::host`]) and testable without a browser
//! (like [`crate::preset_bridge`]): what opts a page into hosting PhotoCraft in its own
//! document, which `photocraft:open` payloads may be copied for the app, and what a written file
//! tells the host in `photocraft:save`'s `saveAs`.

/// The largest `photocraft:open` payload the bridge copies for the app: 512 MiB. PhotoCraft puts
/// no size limit of its own on opens (the allocation guard catches what a decode really needs),
/// so the bridge refuses anything larger before copying and ignores it, like a malformed open.
pub const MAX_OPEN_BYTES: usize = 512 << 20;

/// Does this `data-host` value ask for page host mode? Only the exact value `page` counts;
/// anything else is ignored, so the page host mode stays opt-in. `?host=parent`, the parent
/// bridge's URL opt-in, is answered before this is asked.
pub fn page_host_requested(data_host: Option<&str>) -> bool {
    data_host == Some("page")
}

/// Whether an open payload of `len` bytes may be copied for the app.
pub fn open_allowed(len: usize) -> bool {
    len <= MAX_OPEN_BYTES
}

/// The last component of a path, or the whole string when it has none: the name a file opened
/// from the host carries, and the name a file written to the host reports.
pub fn file_name(path: &str) -> String {
    std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string())
}

/// The `saveAs` field of a `photocraft:save` for a file the app has written, decided once the
/// frame's work is done ([`crate::host::Bridge::sync`]), from what the file became:
///
/// - `bound_after`: the file is a document's file now. Only a save that becomes the document's
///   file gets here (a non-copy save, which `photocraft_engine::file_cmds::saved_to` binds);
///   copies and exports are written without touching the document's file, and so are writes of
///   other kinds of files (an .xmp sidecar, exported measurements), so they end `false`.
/// - `bound_before`: some document already had this file when the frame began, so the write
///   replaced an existing file rather than making a new one (a Save to the document's own file,
///   or a copy written over another document's file).
/// - `host_opened`: the host sent a file of this name (`photocraft:open`), so a save under it
///   writes the host's own file, as a Save does.
///
/// `saveAs` is `true` only when the write made the file a document's file that neither the host
/// nor another document already had: a Save As to a new name, or a Save of a document the host
/// never sent (an untitled document, or an import that changes format, like a .pdn suggested as
/// .pcraft or a .kra as .ora). The next Save of that document carries its new name with `false`.
pub fn save_as_flag(bound_before: bool, bound_after: bool, host_opened: bool) -> bool {
    bound_after && !bound_before && !host_opened
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_exact_page_value_opts_in() {
        assert!(page_host_requested(Some("page")));
        for off in [None, Some(""), Some("Page"), Some("PAGE"), Some(" page"), Some("page "), Some("parent")] {
            assert!(!page_host_requested(off), "{off:?} must keep the page host mode off");
        }
    }

    #[test]
    fn open_payloads_are_capped_before_they_are_copied() {
        assert!(open_allowed(0));
        assert!(open_allowed(MAX_OPEN_BYTES));
        assert!(!open_allowed(MAX_OPEN_BYTES + 1));
        assert!(!open_allowed(usize::MAX));
    }

    #[test]
    fn file_names_keep_the_last_component() {
        assert_eq!(file_name("photos/holiday/photo.psd"), "photo.psd");
        assert_eq!(file_name("photo.psd"), "photo.psd");
        assert_eq!(file_name(""), "");
    }

    #[test]
    fn save_as_flags_only_a_file_the_host_never_sent_that_becomes_a_documents_file() {
        // A Save As to a new name, or a Save of a document the host never sent.
        assert!(save_as_flag(false, true, false));
        // A Save of the file the host opened, under its own name.
        assert!(!save_as_flag(false, true, true));
        // A Save to the document's own file, and a copy written over another document's file.
        assert!(!save_as_flag(true, true, false));
        assert!(!save_as_flag(true, true, true));
        // Copies, exports and other written files never become a document's file.
        assert!(!save_as_flag(false, false, false));
        assert!(!save_as_flag(false, false, true));
    }
}
