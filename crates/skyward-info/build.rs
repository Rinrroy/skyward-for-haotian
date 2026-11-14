// Who built this image, from `SKYWARD_BUILDER` in the build's environment.
// The Recovery version card on the specs page prints it beside the version, so
// it has to be a single line: a line break in it would be a line break in the
// row the page draws.
//
// When the variable is unset or names nobody, nothing is emitted and
// `skyward_info::BUILDER` (`option_env!`) is `None`, which is what leaves the
// row off the page altogether.
fn main() {
    println!("cargo:rerun-if-env-changed=SKYWARD_BUILDER");
    let Some(raw) = std::env::var("SKYWARD_BUILDER").ok() else { return };
    let builder = raw.trim();
    if builder.is_empty() {
        return;
    }
    if builder.contains('\r') || builder.contains('\n') {
        panic!("SKYWARD_BUILDER is {builder:?}, which has a line break in it");
    }
    println!("cargo:rustc-env=SKYWARD_BUILDER={builder}");
}
