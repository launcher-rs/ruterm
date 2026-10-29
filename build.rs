/// 构建脚本：Windows 下把品牌图标嵌入 exe 资源，作为文件 / 任务栏 / 标题栏图标。
/// rgpui 的 `WindowOptions.icon` 仅在 X11 生效，Windows 只能靠嵌入 PE 资源实现。
fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=assets/icons/ruterm.ico");
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icons/ruterm.ico");
        res.compile()?;
        // 调试构建的元素渲染帧很大，嵌套一深就会耗尽 Windows 默认的 1MB 主线程栈
        // （表现为「has overflowed its stack」）。给主线程 16MB，UI 仍留在主线程。
        println!("cargo:rustc-link-arg=/STACK:16777216");
    }

    Ok(())
}
