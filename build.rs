fn main() {
    let config = slint_build::CompilerConfiguration::new()
        .with_style("fluent-light".into())
        .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles);
    slint_build::compile_with_config("ui/app.slint", config).expect("Unable to compile desktop UI");

    // Windows 的图标必须编进可执行文件的资源段，运行时设置的窗口图标管不到
    // 资源管理器和任务栏。交叉编译时由 mingw 的 windres 处理。
    println!("cargo:rerun-if-changed=assets/app-icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("assets/app-icon.ico")
            .compile()
            .expect("Unable to embed the Windows icon resource");
    }
}
