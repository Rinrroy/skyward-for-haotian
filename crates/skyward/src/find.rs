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
//! A folder that cannot be read is skipped, not reported: a search that
//! stopped on the first folder it was not allowed in would be useless on
//! `/data`, where most folders belong to someone else. A link is never
//! followed into a folder, since a link can lead back up.

use anyhow::Result;
use skyward_ui::app::{FileKind, Found, SortKey};
use std::collections::VecDeque;
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
    while let Some((dir, depth)) = queue.pop_front() {
        folders += 1;
        if folders > MOST_FOLDERS {
            break;
        }
        let mut rows = match crate::ui::read_folder(&dir) {
            Ok(rows) => rows,
            // The folder the page shows is the one a reason is worth
            // giving for; one below it that will not open is passed over.
            Err(e) if dir != root => {
                let _ = e;
                continue;
            }
            Err(e) => return Err(e),
        };
        // The folder's own rows in the order the list would show them, so
        // that the box's answer moves with the button beside it.
        skyward_ui::sort::sort(&mut rows, sort);
        for row in rows {
            seen += 1;
            if seen > MOST_ENTRIES {
                return Ok(found);
            }
            let name = row.name.to_lowercase();
            if name.contains(&needle) {
                found.push(Found { folder: dir.to_string_lossy().into_owned(), entry: row.clone() });
                if found.len() >= MOST_FOUND {
                    return Ok(found);
                }
            }
            // Only a folder of the folder's own is gone into: a link to one
            // may point anywhere, including back up this tree.
            if matches!(row.kind, FileKind::Folder { .. }) && depth < MOST_DEPTH {
                queue.push_back((dir.join(&row.name), depth + 1));
            }
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
}
