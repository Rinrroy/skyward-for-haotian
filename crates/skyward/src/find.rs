//! What the file manager's search box looks for under the folder it shows.
//!
//! The box on a folder's page looks through that folder and every folder
//! below it, so a file three folders down is found from the top of a
//! partition without opening each folder on the way. The walk is bounded on
//! every side, because it runs inside the frame loop: a query that matches
//! nothing on a full partition must cost a moment, not a hang, and a tree
//! that answers a thousand times over must not build a list nothing can
//! scroll through.
//!
//! The walk is also cheap on purpose. It reads the names of a folder and
//! whether each entry is a folder, and asks [`crate::ui::read_row`] about
//! the entries it is going to show and no others. Listing a folder the way
//! the page does costs a stat for every entry and a count of the contents of
//! every folder in it; a walk that paid both on every entry of every folder
//! it passed is a walk that makes a keystroke wait, and the box takes a
//! keystroke at a time.
//!
//! A folder that cannot be read is skipped, not reported: a search that
//! stopped on the first folder it was not allowed in would be useless on
//! `/data`, where most folders belong to someone else. A link is never
//! followed into a folder, since a link can lead back up.

use anyhow::{Context, Result};
use skyward_ui::app::{FileRow, Found, SortKey};
use std::collections::VecDeque;
use std::fs;
use std::path::Path;

/// How many folders below the one shown are opened at the most.
const MOST_FOLDERS: usize = 2000;
/// How many entries are looked at at the most.
const MOST_ENTRIES: usize = 40000;
/// How many entries are reported at the most.
const MOST_FOUND: usize = 200;
/// How deep below the folder shown the search goes.
const MOST_DEPTH: usize = 12;

/// The entries named `query` in the folders below `root`, each with the
/// folder it is in, in the order [`sort`] puts a folder's own entries in and
/// the folders in the order the walk reached them.
///
/// [`sort`]: skyward_ui::sort::sort
pub fn find(root: &Path, query: &str, sort: SortKey) -> Result<Vec<Found>> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    let mut queue = VecDeque::from([(root.to_path_buf(), 0usize)]);
    let (mut folders, mut seen) = (0, 0);
    let mut spent = false;
    while let Some((dir, depth)) = queue.pop_front() {
        folders += 1;
        if folders > MOST_FOLDERS {
            break;
        }
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            // The folder the page shows is the one a reason is worth
            // giving for; one below it that will not open is passed over.
            Err(e) if dir != root => {
                let _ = e;
                continue;
            }
            Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
        };
        // The matches in this folder, in the order the list would show them.
        let mut here: Vec<FileRow> = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) if dir != root => {
                    let _ = e;
                    continue;
                }
                Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
            };
            seen += 1;
            if seen > MOST_ENTRIES {
                spent = true;
                break;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            // The kind comes with the name from the folder's own listing on
            // any filesystem that keeps it, which is the difference between
            // reading a tree and stating every entry in it.
            let folder = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
            if name.to_lowercase().contains(&needle) {
                // Only what is shown is looked at closely: its size, its
                // time and what kind of thing it is are what its row says.
                if let Some(row) = crate::ui::read_row(&entry.path())? {
                    here.push(row);
                }
            }
            // Only a folder of the folder's own is gone into: a link to one
            // may point anywhere, including back up this tree.
            if folder && depth < MOST_DEPTH {
                queue.push_back((entry.path(), depth + 1));
            }
        }
        skyward_ui::sort::sort(&mut here, sort);
        if !here.is_empty() {
            let at = dir.to_string_lossy().into_owned();
            for row in here {
                found.push(Found { folder: at.clone(), entry: row });
                if found.len() >= MOST_FOUND {
                    return Ok(found);
                }
            }
        }
        if spent {
            break;
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tree under a folder of its own, gone at the end of the test.
    struct Tree(std::path::PathBuf);

    impl Tree {
        fn made(name: &str) -> Tree {
            let root = std::env::temp_dir().join(format!("skyward-find-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).expect("a folder to search in");
            Tree(root)
        }

        fn file(&self, path: &str) {
            let full = self.0.join(path);
            fs::create_dir_all(full.parent().expect("a file has a folder")).expect("a folder for a file");
            fs::write(&full, b"x").expect("a file to find");
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_name_below_the_folder_shown_is_found_from_it() {
        let tree = Tree::made("deep");
        tree.file("DCIM/Camera/sea.jpg");
        tree.file("DCIM/land.jpg");
        tree.file("notes.txt");

        let found = find(&tree.0, "land", SortKey::Name).expect("a tree that reads");
        // The name is in the folder below the one searched, not the one
        // searched: that is the whole point of the walk.
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].entry.name, "land.jpg");
        assert!(found[0].folder.ends_with("DCIM"));
    }

    #[test]
    fn every_folder_the_name_is_in_is_given_and_no_other() {
        let tree = Tree::made("some");
        tree.file("a/sea.jpg");
        tree.file("a/b/sea.jpg");
        tree.file("c/SEA.jpg");
        tree.file("c/sand.jpg");

        let found = find(&tree.0, "sea", SortKey::Name).expect("a tree that reads");
        let mut names: Vec<String> = found.iter().map(|f| f.entry.name.clone()).collect();
        names.sort();
        // Matched without case, anywhere in the name, in every folder.
        assert_eq!(names, vec!["SEA.jpg".to_string(), "sea.jpg".to_string(), "sea.jpg".to_string()]);
    }

    #[test]
    fn a_folder_that_cannot_be_read_does_not_end_the_search() {
        let tree = Tree::made("closed");
        tree.file("open/sea.jpg");
        tree.file("shut/sand.jpg");
        let shut = tree.0.join("shut");
        let mut perms = fs::metadata(&shut).expect("the folder").permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o000);
        fs::set_permissions(&shut, perms).expect("a folder no one may read");

        // What is under the folder that cannot be read is not found, and
        // the rest of the tree still is.
        let found = find(&tree.0, "sea", SortKey::Name).expect("a tree that mostly reads");
        assert_eq!(found.len(), 1);

        let mut perms = fs::metadata(&shut).expect("the folder").permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        let _ = fs::set_permissions(&shut, perms);
    }

    #[test]
    fn a_query_with_nothing_in_it_finds_nothing() {
        let tree = Tree::made("empty");
        tree.file("sea.jpg");
        assert!(find(&tree.0, "  ", SortKey::Name).expect("a tree that reads").is_empty());
    }

    #[test]
    fn a_tree_that_answers_more_than_the_list_can_show_stops_at_the_cap() {
        let tree = Tree::made("many");
        for i in 0..MOST_FOUND + 20 {
            tree.file(&format!("sea{}.jpg", i));
        }
        let found = find(&tree.0, "sea", SortKey::Name).expect("a tree that reads");
        assert_eq!(found.len(), MOST_FOUND);
    }

    #[test]
    fn what_a_match_is_is_told_apart_from_what_a_name_says() {
        let tree = Tree::made("kinds");
        tree.file("sea.jpg");
        fs::create_dir_all(tree.0.join("sea-folder")).expect("a folder to find");
        std::os::unix::fs::symlink("sea.jpg", tree.0.join("sea-link")).expect("a link to find");

        let found = find(&tree.0, "sea", SortKey::Name).expect("a tree that reads");
        let mut kinds: Vec<(&str, bool)> = found
            .iter()
            .map(|f| {
                let folder = matches!(f.entry.kind, skyward_ui::app::FileKind::Folder { .. } | skyward_ui::app::FileKind::Link { folder: true, .. });
                (f.entry.name.as_str(), folder)
            })
            .collect();
        kinds.sort();
        // The folder is one, the link to a file is not, and the file is not:
        // the walk tells them apart without opening every folder it passes.
        assert_eq!(kinds, vec![("sea-folder", true), ("sea-link", false), ("sea.jpg", false)]);
    }
}
