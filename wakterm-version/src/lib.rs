pub fn wakterm_version() -> &'static str {
    // See build.rs
    env!("WAKTERM_CI_TAG")
}

pub fn wakterm_target_triple() -> &'static str {
    // See build.rs
    env!("WAKTERM_TARGET_TRIPLE")
}
