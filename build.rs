fn main() {
    println!("cargo:rerun-if-changed=assets/meeting-notes.rc");
    println!("cargo:rerun-if-changed=assets/meeting-notes.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile_for(
            "assets/meeting-notes.rc",
            ["meeting-notes"],
            embed_resource::NONE,
        )
        .manifest_required()
        .expect("embed the Meeting Notes Windows icon");
    }
}
