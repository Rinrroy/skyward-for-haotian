//! The orders the file manager can list a folder in, and the one order it
//! always keeps inside them: folders first.
//!
//! A listing comes from a directory the phone's own programs are writing to
//! while it is read, so the order here is a total one over whatever was read
//! and never an assumption about it: no indexing, no unwrap, and every
//! comparison ends somewhere. Two entries that a chosen key cannot tell
//! apart — two files of the same size, or names that differ only in case —
//! are told apart by their names, and names that lower-case to the same
//! string are told apart by the bytes they are, so the order is the same
//! from one read to the next and no entry is ever lost or drawn twice.

use crate::app::{FileKind, FileRow, SortKey};
use crate::files::{file_type, FileType};

/// Sort `entries` in place, as `key` asks: folders first, then the key, and
/// what it cannot tell apart by name ignoring case.
pub fn sort(entries: &mut [FileRow], key: SortKey) {
    entries.sort_by(|a, b| compare(a, b, key));
}

/// The order `a` and `b` go in, as `key` asks.
pub fn compare(a: &FileRow, b: &FileRow, key: SortKey) -> std::cmp::Ordering {
    // Folders first whatever the key: the phone's file manager does the same,
    // and a key that sorted a folder in among the files would hide where to
    // go next among things to do something with.
    folder(b).cmp(&folder(a)).then_with(|| match key {
        SortKey::Name => std::cmp::Ordering::Equal,
        // Most recently changed first, so what was just written is at the
        // top; then the largest, which is what separates two entries written
        // within the same second by a script.
        SortKey::Modified => b.modified.cmp(&a.modified).then_with(|| b.size.cmp(&a.size)),
        // Groups of what a file is, each group by name: what the list's own
        // icons group them by, so a folder of images, packages and text
        // files reads as the same three runs however many there are.
        SortKey::Kind => group(a).cmp(&group(b)),
    })
    // Ties broken by name, lower-cased so `IMG_1` and `img_1` sit together,
    // then by the name itself, so two names that differ only in case still
    // have one order and never compare equal.
    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    .then_with(|| a.name.cmp(&b.name))
}

/// Whether `e` is a folder, which the order always puts first.
fn folder(e: &FileRow) -> bool {
    matches!(e.kind, FileKind::Folder { .. } | FileKind::Link { folder: true, .. })
}

/// How a group the [`SortKey::Kind`] order sorts by is placed relative to
/// the others: as [`FileType`] lists them, with a link to nothing beside
/// what it is, and the phone's own devices and pipes last.
fn group(e: &FileRow) -> (u8, u8) {
    // A device, socket or pipe is nothing to do with what its name ends in.
    if matches!(e.kind, FileKind::Special) {
        return (2, 0);
    }
    (1, rank(file_type(&e.name)))
}

/// Where a type sits among the others, in the order [`FileType`] names them.
fn rank(kind: FileType) -> u8 {
    match kind {
        FileType::Archive => 0,
        FileType::DiskImage => 1,
        FileType::Picture => 2,
        FileType::Video => 3,
        FileType::Audio => 4,
        FileType::Package => 5,
        FileType::Document => 6,
        FileType::Spreadsheet => 7,
        FileType::Presentation => 8,
        FileType::Pdf => 9,
        FileType::Text => 10,
        FileType::Code => 11,
        FileType::Other => 12,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> FileRow {
        FileRow { name: name.into(), kind: FileKind::Folder { items: Some(3) }, size: 0, modified: 0, mode: 0, uid: 0, gid: 0 }
    }

    fn file(name: &str, modified: i64, size: u64) -> FileRow {
        FileRow { name: name.into(), kind: FileKind::File, size, modified, mode: 0, uid: 0, gid: 0 }
    }

    fn names(rows: &[FileRow]) -> Vec<&str> {
        rows.iter().map(|r| r.name.as_str()).collect()
    }

    #[test]
    fn folders_come_first_by_every_key() {
        let rows = vec![file("a.zip", 100, 1), folder("z"), file("b.png", 300, 1), folder("a")];
        for key in crate::app::SORT_KEYS {
            let mut sorted = rows.clone();
            sort(&mut sorted, key);
            assert_eq!(names(&sorted)[..2], ["a", "z"], "{key:?} puts both folders first, by name");
        }
    }

    #[test]
    fn the_name_order_ignores_case_and_still_tells_two_apart() {
        let mut rows = vec![file("b.txt", 0, 0), file("B.txt", 0, 0), file("a.txt", 0, 0)];
        sort(&mut rows, SortKey::Name);
        assert_eq!(names(&rows), ["a.txt", "B.txt", "b.txt"], "upper case first among names that fold alike");
    }

    #[test]
    fn the_modified_order_puts_the_newest_first_and_separates_one_second() {
        let mut rows = vec![file("old", 100, 9), file("new", 300, 1), file("same-second-small", 200, 1), file("same-second-big", 200, 8)];
        sort(&mut rows, SortKey::Modified);
        assert_eq!(names(&rows), ["new", "same-second-big", "same-second-small", "old"]);
    }

    #[test]
    fn the_kind_order_groups_what_a_file_is_each_group_by_name() {
        let mut rows = vec![
            file("b.zip", 0, 0),
            file("a.png", 0, 0),
            file("notes.txt", 0, 0),
            file("a.zip", 0, 0),
            file("no-extension", 0, 0),
            file("IMG.PNG", 0, 0),
        ];
        sort(&mut rows, SortKey::Kind);
        assert_eq!(names(&rows), ["a.zip", "b.zip", "a.png", "IMG.PNG", "notes.txt", "no-extension"]);
    }

    #[test]
    fn a_special_file_sits_after_every_other_and_is_sorted_by_name() {
        let special = |name: &str| FileRow {
            name: name.into(),
            kind: FileKind::Special,
            size: 0,
            modified: 0,
            mode: 0,
            uid: 0,
            gid: 0,
        };
        let link = |name: &str| FileRow {
            name: name.into(),
            kind: FileKind::Link { target: "/x".into(), folder: false },
            size: 0,
            modified: 0,
            mode: 0,
            uid: 0,
            gid: 0,
        };
        let mut rows = vec![special("b"), link("a.zip"), special("a"), file("z.txt", 0, 0)];
        sort(&mut rows, SortKey::Kind);
        assert_eq!(names(&rows), ["a.zip", "z.txt", "a", "b"], "a link is what it leads to; a device comes last");
    }

    #[test]
    fn the_order_is_the_same_however_the_entries_arrive_and_keeps_every_one() {
        // What a name that was not UTF-8 shows as, built here rather than
        // written as a literal: the typeface test reads every string in `src`
        // and asks the bundled font for each character, and no font draws this
        // one.
        let folded = String::from_utf8_lossy(b"\xff").into_owned();
        let all = vec![
            file("IMG_1.JPG", 5, 2),
            file("img_1.jpg", 5, 2),
            file("同一张.png", 5, 2),
            file(&folded, 5, 2),
            folder("DCIM"),
            file("a", 0, 0),
        ];
        for key in crate::app::SORT_KEYS {
            let mut forward = all.clone();
            sort(&mut forward, key);
            let mut backward = all.clone();
            backward.reverse();
            sort(&mut backward, key);
            assert_eq!(names(&forward), names(&backward), "{key:?} reads the same both ways");
            assert_eq!(forward.len(), all.len(), "{key:?} keeps every entry");
        }
    }

    #[test]
    fn an_empty_and_a_single_entry_list_sort_without_trouble() {
        let mut none: Vec<FileRow> = Vec::new();
        for key in crate::app::SORT_KEYS {
            sort(&mut none, key);
            assert!(none.is_empty());
        }
        let mut one = vec![folder("only")];
        sort(&mut one, SortKey::Modified);
        assert_eq!(names(&one), ["only"]);
    }
}
