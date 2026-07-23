fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile_for(
            "windows/harness-hat.rc",
            ["hht", "hht-daemon"],
            embed_resource::NONE,
        )
        .manifest_required()
        .unwrap();
    }
}
