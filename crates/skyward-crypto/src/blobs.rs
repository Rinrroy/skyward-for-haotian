//! Which of the blobs a phone keeps for a user is the one to open.
//!
//! A phone keeps a blob for each token some part of it was given to reach a
//! user's secret with, and writes down which one the user's current
//! credential opens. Some of them are guarded by a weaver: a slot in a secure
//! element that checks the credential itself. A recovery reaches a weaver
//! only where the phone's weaver HAL talks to the secure element directly;
//! one whose weaver is a Thales secure element goes through Android's own
//! OMAPI service, which a recovery does not run, so such a blob cannot be
//! opened here at all.
//!
//! HyperOS therefore keeps a second blob beside it, bound instead to bytes
//! kept next to it and guarded by the credential service of the older kind,
//! which a recovery does run. It is the same secret: what changes is what
//! guards it. This module chooses between them, in code that reads the files
//! as they are, so that the choice can be tested on a computer.

// The choice serves the phone's recovery alone, and the host tests.
#![cfg_attr(not(any(target_os = "android", test)), allow(dead_code))]
use anyhow::Result;
use std::path::Path;

/// The files a phone keeps beside a blob, each with what it is: the blob
/// itself, the number of the weaver's slot it is bound to, the bytes it is
/// bound to instead, and what the phone recorded of the user's credential.
const BESIDE: [(&str, &str); 4] = [("spblob", "blob"), ("weaver", "weaver slot"), ("secdis", "secdis"), ("pwd", "pwd")];

/// Whether `dir` keeps `id`'s `suffix` file.
fn has(dir: &Path, id: &str, suffix: &str) -> bool {
    dir.join(format!("{id}.{suffix}")).is_file()
}

/// Whether `id` is bound to a weaver's slot.
fn bound_to_a_weaver(dir: &Path, id: &str) -> bool {
    has(dir, id, "weaver")
}

/// What came of choosing between a user's blobs: the one to open, and what
/// the phone kept beside them where the choice was not plain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chosen {
    /// The blob to open.
    pub id: String,
    /// What is worth saying of the choice, where the phone records a weaver
    /// binding for a blob: the recovery writes it to the kernel log, where a
    /// phone's own recovery leaves what it did.
    pub said: Option<String>,
}

/// The blob to open, which is `id` unless that one is bound to a weaver and
/// another of the user's blobs is not.
///
/// The blob taken instead has to be bound to bytes a recovery can read and
/// carry the credential the phone recorded with it, or there would be
/// nothing to check a credential against and nothing to unwrap the secret
/// with.
pub fn beside_the_weaver(dir: &Path, id: &str, ids: Vec<String>) -> Result<Chosen> {
    if !bound_to_a_weaver(dir, id) {
        return Ok(Chosen { id: id.to_string(), said: None });
    }
    // Every blob is written down as it stands, so that a phone which kept
    // none for the older credential can be told from one whose second blob
    // is there but does not carry what the way round the weaver needs.
    let seen = format!("the blob the phone records is bound to a weaver; the blobs beside it are {}", describe(dir, &ids));
    let usable = ids.into_iter().find(|other| !bound_to_a_weaver(dir, other) && has(dir, other, "secdis") && has(dir, other, "pwd"));
    match usable {
        Some(other) => {
            // Said as it happens, so a recovery that opens the second blob
            // rather than the recorded one can be told apart from one that
            // opens what the phone recorded.
            let said = format!("{seen}; opening the one bound beside it: {other}");
            Ok(Chosen { id: other, said: Some(said) })
        }
        None => {
            // Nothing can open the recorded blob here, and there is no second
            // one to fall back on. Said before the weaver is asked, so that
            // the failure that follows is read with its reason.
            let said = format!(
                "{seen}; no blob beside it is bound to anything a recovery can reach, \
                 so the files of the user it belongs to cannot be opened in a recovery"
            );
            Ok(Chosen { id: id.to_string(), said: Some(said) })
        }
    }
}

/// Every blob in `dir`, named with the files the phone keeps beside it.
/// Written without assuming any of them is there, so that what a phone kept
/// is read off the phone rather than guessed.
pub fn describe(dir: &Path, ids: &[String]) -> String {
    let mut said = Vec::new();
    for id in ids {
        let kept: Vec<&str> = BESIDE.iter().filter(|(suffix, _)| has(dir, id, suffix)).map(|(_, name)| *name).collect();
        said.push(format!("{id} ({})", kept.join(", ")));
    }
    said.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A directory of blobs, each written with the files named for it.
    fn blobs(test: &str, files: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("skyward-blobs-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in files {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        dir
    }

    #[test]
    fn a_blob_is_described_by_the_files_the_phone_kept_beside_it() {
        let dir = blobs("describe", &["abc.spblob", "abc.weaver", "abc.pwd", "def.spblob", "def.secdis"]);
        assert_eq!(
            describe(&dir, &["abc".to_string(), "def".to_string()]),
            "abc (blob, weaver slot, pwd); def (blob, secdis)",
            "only the files the phone kept are named"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_blob_bound_to_no_weaver_is_opened_as_it_is() {
        let dir = blobs("plain", &["abc.spblob", "abc.secdis", "abc.pwd"]);
        assert_eq!(beside_the_weaver(&dir, "abc", vec!["abc".into()]).unwrap().id, "abc");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_blob_bound_to_a_weaver_stays_itself_when_none_beside_it_can_be_opened() {
        // What a phone kept that has no second blob: the recorded one is
        // still what is handed back, so that the weaver is asked and says
        // why it cannot answer, rather than a wrong blob being opened.
        let dir = blobs("weaver-only", &["abc.spblob", "abc.weaver", "abc.pwd"]);
        assert_eq!(beside_the_weaver(&dir, "abc", vec!["abc".into()]).unwrap().id, "abc");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_blob_bound_beside_the_weaver_is_taken_when_there_is_one() {
        let dir = blobs("weaver-beside", &["abc.spblob", "abc.weaver", "abc.pwd", "def.spblob", "def.secdis", "def.pwd"]);
        assert_eq!(beside_the_weaver(&dir, "abc", vec!["abc".into(), "def".into()]).unwrap().id, "def");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_second_blob_without_both_of_the_files_the_old_way_needs_is_not_taken() {
        let dir = blobs("weaver-short", &["abc.spblob", "abc.weaver", "def.spblob", "def.secdis"]);
        assert_eq!(
            beside_the_weaver(&dir, "abc", vec!["abc".into(), "def".into()]).unwrap().id,
            "abc",
            "no recorded credential means the credential service has none to check"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_second_blob_bound_to_a_weaver_too_is_not_taken() {
        let dir = blobs("weaver-both", &["abc.spblob", "abc.weaver", "def.spblob", "def.weaver", "def.secdis", "def.pwd"]);
        assert_eq!(
            beside_the_weaver(&dir, "abc", vec!["abc".into(), "def".into()]).unwrap().id,
            "abc",
            "a weaver the recovery cannot reach guards the second blob just as it does the first"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_phone_that_kept_no_way_round_the_weaver_says_so_before_the_weaver_is_asked() {
        // The phone this came from: the owner's blob is bound to a weaver,
        // and nothing beside it can be opened in a recovery. What is said
        // goes to the phone's log, so that the failure that follows reads
        // with its reason rather than as a wrong credential.
        let dir = blobs("weaver-nofallback", &["abc.spblob", "abc.weaver", "abc.pwd"]);
        let chosen = beside_the_weaver(&dir, "abc", vec!["abc".into()]).unwrap();
        assert_eq!(chosen.id, "abc", "the recorded blob is still what is asked for");
        let said = chosen.said.expect("the phone's log is told what was kept");
        assert!(said.contains("bound to a weaver"), "{said}");
        assert!(said.contains("abc (blob, weaver slot, pwd)"), "the blobs the phone kept, as they are: {said}");
        assert!(said.contains("cannot be opened in a recovery"), "{said}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_blob_opened_without_a_weaver_says_nothing() {
        let dir = blobs("plain-quiet", &["abc.spblob", "abc.secdis", "abc.pwd"]);
        assert_eq!(beside_the_weaver(&dir, "abc", vec!["abc".into()]).unwrap().said, None, "nothing worth a line");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
