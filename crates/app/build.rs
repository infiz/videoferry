fn main() {
    slint_build::compile("ui/app.slint").expect("unable to compile the Slint interface");

    #[cfg(target_os = "windows")]
    {
        println!("cargo:rerun-if-changed=assets/windows/app-icon.rc");
        println!("cargo:rerun-if-changed=assets/app-icon.ico");
        embed_resource::compile_for(
            "assets/windows/app-icon.rc",
            ["videoferry"],
            embed_resource::NONE,
        )
        .manifest_optional()
        .expect("unable to embed the Windows application icon");
    }
}
