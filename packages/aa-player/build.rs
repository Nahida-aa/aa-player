// Windows 目标：把应用图标与版本信息嵌入 exe（资源管理器/任务栏/快捷方式用）。
// 其他平台 no-op——build.rs 总会被执行，但只有目标是 Windows 才编译资源。
//
// 图标由 `just icons`（scripts/gen-icons.mjs）生成并随仓库提交，
// 这里只引用，不重复生成。
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    // 相对路径以本包根（packages/aa-player）为基准。
    println!("cargo:rerun-if-changed=../../resources/icons/aa-player.ico");

    let mut res = winresource::WindowsResource::new();
    res.set_icon("../../resources/icons/aa-player.ico");
    res.set("FileDescription", "AA Player");
    res.set("ProductName", "AA Player");
    res.set("LegalCopyright", "Apache-2.0");
    res.set("FileVersion", &format!("{}.0.0.0", env!("CARGO_PKG_VERSION")));
    res.set("ProductVersion", &format!("{}.0.0.0", env!("CARGO_PKG_VERSION")));
    res.compile().expect("编译 Windows 资源失败（图标/版本信息）");
}
