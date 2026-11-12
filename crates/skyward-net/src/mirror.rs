//! Mirrors of GitHub, for the networks that reach neither its API nor where
//! it hands its files out: what a mirror is asked instead, in the order it is
//! asked, with GitHub's own address always the first of them.

use std::sync::atomic::{AtomicBool, Ordering};

/// Proxies that fetch any address they are given and hand it out as the rest
/// of their own, in the order they are tried: the first has answered most
/// often, and each is tried only once the one before it has failed.
///
/// The order is what answers from a phone network in the mainland, measured
/// rather than assumed: the first two answered the published index and the
/// 700 MB image with their real lengths, the last one answered nothing at all
/// from here and is kept only as somewhere else to ask. Every one of them is
/// tried, so a dead one costs a timeout and never the download — which is why
/// the dead ones are last.
const PROXIES: [&str; 3] = ["https://gh-proxy.com/", "https://ghproxy.net/", "https://ghfast.top/"];

/// GitHub's API, as the addresses asked of it are written.
const API: &str = "https://api.github.com/";

/// A mirror that answers GitHub's API in its place, at the rest of the
/// address: an asset's address or a release's link is not put on it, which
/// answers the API alone.
const API_MIRROR: &str = "https://api.kkgithub.com/";

/// Whether a mirror may answer when GitHub itself cannot. It may, until it is
/// told not to.
static ALLOWED: AtomicBool = AtomicBool::new(true);

/// Say whether a mirror may answer when GitHub itself cannot.
pub fn set(allowed: bool) {
    ALLOWED.store(allowed, Ordering::Relaxed);
}

/// Whether a mirror may answer when GitHub itself cannot.
pub fn allowed() -> bool {
    ALLOWED.load(Ordering::Relaxed)
}

/// `url` as it should be asked, GitHub's own first, then each mirror in the
/// order they are tried; the mirrors only while they are allowed.
///
/// The first is always `url` itself: a mirror is what is tried once GitHub's
/// own address has failed, never a replacement for it.
pub fn candidates(url: &str) -> Vec<String> {
    candidates_when(url, allowed())
}

/// [`candidates`], with the switch said outright rather than read.
fn candidates_when(url: &str, allowed: bool) -> Vec<String> {
    if !allowed {
        return vec![url.to_string()];
    }
    among(url, &PROXIES, API_MIRROR)
}

/// `url` and the mirrors of it, with `url` first, made by putting `url` on
/// each of `prefixes` and, where `url` is GitHub's API, on `api`.
fn among(url: &str, prefixes: &[&str], api: &str) -> Vec<String> {
    let mut out = vec![url.to_string()];
    // A mirror fetches what it is given over https, so an address that is not
    // https is not one a mirror can be given: it is asked of where it says,
    // and nowhere else.
    if !url.starts_with("https://") {
        return out;
    }
    if let Some(rest) = url.strip_prefix(API) {
        out.push(format!("{}/{}", api.trim_end_matches('/'), rest));
    }
    for prefix in prefixes {
        // A trailing slash on the prefix, or none, is the same mirror.
        let base = format!("{}/", prefix.trim_end_matches('/'));
        // An address that already goes through a mirror is not wrapped in
        // another: that would be this mirror asking itself for the whole
        // address of a mirror.
        if url.starts_with(&base) {
            continue;
        }
        out.push(format!("{base}{url}"));
    }
    dedupe(&mut out);
    out
}

/// `urls` without the entries that repeat, each kept where it was first.
fn dedupe(urls: &mut Vec<String>) {
    let mut at = 0;
    while at < urls.len() {
        if urls[..at].contains(&urls[at]) {
            urls.remove(at);
        } else {
            at += 1;
        }
    }
}

/// The host of `url`, such as gh-proxy.com, as the mirror that answered is
/// named; none where `url` names none.
pub(crate) fn host(url: &str) -> Option<String> {
    let rest = match url.split_once("://") {
        Some((_, rest)) => rest,
        None => url,
    };
    let end = match rest.find(['/', '?', '#']) {
        Some(at) => at,
        None => rest.len(),
    };
    match &rest[..end] {
        "" => None,
        host => Some(host.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LATEST: &str = "https://api.github.com/repos/Rinrroy/skyward-for-haotian/releases/latest";
    const IMAGE: &str = "https://github.com/Rinrroy/skyward-for-haotian/releases/download/v0.1.3/skyward-v0.1.3-ishtar-OS3.0.308.0.WMACNXM.img";

    #[test]
    fn the_api_is_asked_of_github_first_then_of_its_mirror_then_of_each_proxy() {
        assert_eq!(
            candidates_when(LATEST, true),
            [
                "https://api.github.com/repos/Rinrroy/skyward-for-haotian/releases/latest",
                "https://api.kkgithub.com/repos/Rinrroy/skyward-for-haotian/releases/latest",
                "https://gh-proxy.com/https://api.github.com/repos/Rinrroy/skyward-for-haotian/releases/latest",
                "https://ghproxy.net/https://api.github.com/repos/Rinrroy/skyward-for-haotian/releases/latest",
                "https://ghfast.top/https://api.github.com/repos/Rinrroy/skyward-for-haotian/releases/latest",
            ],
            "GitHub itself, then a mirror of its API, then the proxies; the address unchanged inside each"
        );
    }

    #[test]
    fn the_api_mirror_answers_the_api_and_is_never_given_a_file_to_fetch() {
        let image = candidates_when(IMAGE, true);
        assert_eq!(image[0], IMAGE, "GitHub's own address first");
        assert!(image.iter().all(|url| !url.contains("kkgithub")), "a release's link is not the API: {image:?}");
        let asset = candidates_when("https://api.github.com/repos/Rinrroy/skyward-for-haotian/releases/assets/719751598", true);
        assert_eq!(asset[1], "https://api.kkgithub.com/repos/Rinrroy/skyward-for-haotian/releases/assets/719751598", "an asset's address is the API");
    }

    #[test]
    fn a_prefix_wraps_the_address_the_same_with_or_without_its_trailing_slash() {
        let without = among(LATEST, &["https://gh-proxy.com", "https://api.kkgithub.com"], "https://api.kkgithub.com");
        let with = among(LATEST, &["https://gh-proxy.com/", "https://api.kkgithub.com/"], "https://api.kkgithub.com/");
        assert_eq!(without, with);
        assert_eq!(without[1], "https://api.kkgithub.com/repos/Rinrroy/skyward-for-haotian/releases/latest");
        assert_eq!(without[2], format!("https://gh-proxy.com/{LATEST}"));
    }

    #[test]
    fn an_address_that_goes_through_a_mirror_already_is_not_wrapped_again() {
        let wrapped = format!("https://gh-proxy.com/{LATEST}");
        assert_eq!(candidates_when(&wrapped, true), [wrapped.clone(), format!("https://ghproxy.net/{wrapped}"), format!("https://ghfast.top/{wrapped}")]);
    }

    #[test]
    fn an_address_that_is_not_https_is_asked_of_where_it_says_alone() {
        let local = "http://127.0.0.1:8080/repos/Rinrroy/skyward-for-haotian/releases/assets/719751598";
        assert_eq!(candidates_when(local, true), [local], "no mirror fetches what is not https");
    }

    #[test]
    fn no_mirror_answers_once_they_are_not_allowed() {
        assert_eq!(candidates_when(LATEST, false), [LATEST], "GitHub's own address is still asked");
        assert!(allowed(), "the mirrors answer until they are told not to");
    }

    #[test]
    fn a_mirror_that_answered_is_named_by_its_host() {
        assert_eq!(host("https://gh-proxy.com/https://api.github.com/repos/a").as_deref(), Some("gh-proxy.com"));
        assert_eq!(host("https://api.kkgithub.com/repos/a?b=c").as_deref(), Some("api.kkgithub.com"));
        assert_eq!(host("http://127.0.0.1:8080/a").as_deref(), Some("127.0.0.1:8080"));
        assert_eq!(host("https://").as_deref(), None);
        assert_eq!(host(""), None);
    }
}
