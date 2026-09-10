#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod server;
mod store;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use i_slint_backend_winit::{EventResult, WinitWindowAccessor, winit::event::WindowEvent};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::{
    cell::RefCell,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    rc::Rc,
    sync::mpsc,
    time::{Duration, UNIX_EPOCH},
};
use store::Store;
slint::include_modules!();

const GITHUB_URL: &str = "https://github.com/xusenlin/lan-drop";

enum Update {
    Status(String),
    Files(Vec<store::Entry>),
    TextSaved(String),
    ServerStopped(String),
}

fn main() {
    if let Err(error) = run() {
        eprintln!("LAN Drop: {error:#}");
        if !std::env::args().any(|a| a == "--headless") {
            rfd::MessageDialog::new()
                .set_title("LAN Drop failed to start")
                .set_description(format!("{error:#}"))
                .set_level(rfd::MessageLevel::Error)
                .show();
        }
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut port = 8765;
    let mut headless = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--headless" => headless = true,
            "--port" => {
                port = args
                    .next()
                    .context("--port needs a port number")?
                    .parse()
                    .context("the port must be between 0 and 65535")?
            }
            "--help" | "-h" => {
                println!(
                    "LAN Drop\n  --port <port>  defaults to 8765; tries the next 20 ports if taken; 0 picks a free one\n  --headless     run only the HTTP server\nData is kept in a LanDropData folder next to the executable; for a macOS .app it\nsits next to the .app, or in your home folder once the app is installed into\nApplications. The window footer always shows the path in use."
                );
                return Ok(());
            }
            _ => anyhow::bail!("unknown argument: {arg}"),
        }
    }
    let store = Store::new(data_dir()?)?;
    let listener = bind_listener(port)?;
    let port = listener.local_addr()?.port();
    let urls = lan_urls(port);
    let url = urls
        .first()
        .cloned()
        .unwrap_or_else(|| format!("http://127.0.0.1:{port}"));
    println!(
        "LAN Drop is running\nAddress: {}\nData folder: {}",
        urls.join("\nAddress: "),
        store.root().display()
    );
    let runtime = tokio::runtime::Runtime::new()?;
    let (tx, rx) = mpsc::channel();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let cancel = shutdown.clone();
    let app = server::router(store.clone());
    let server_tx = tx.clone();
    let server_task = {
        let _guard = runtime.enter();
        let listener = tokio::net::TcpListener::from_std(listener)?;
        runtime.spawn(async move {
            let result = axum::serve(listener, app)
                .with_graceful_shutdown(cancel.cancelled_owned())
                .await;
            if let Err(error) = &result {
                let _ = server_tx.send(Update::ServerStopped(format!(
                    "HTTP server stopped: {error}"
                )));
            }
            result
        })
    };
    if headless {
        runtime.block_on(async {
            tokio::select! {
                result = server_task => { result??; }
                _ = tokio::signal::ctrl_c() => { shutdown.cancel(); }
            }
            Ok::<_, anyhow::Error>(())
        })?;
        runtime.shutdown_timeout(Duration::from_secs(3));
        return Ok(());
    }

    slint::platform::set_platform(Box::new(i_slint_backend_winit::Backend::new()?))?;
    let ui = AppWindow::new()?;
    let theme = ui.global::<Theme>();
    theme.set_font_family(ui_font_family().into());
    theme.set_symbol_font(symbol_font_family().into());
    ui.set_server_url(url.clone().into());
    ui.set_all_urls(urls.join("  ·  ").into());
    ui.set_data_path(store.root().display().to_string().into());
    ui.set_status("Ready · devices on this network can open the address above".into());
    let clipboard = Rc::new(RefCell::new(arboard::Clipboard::new().ok()));

    let weak = ui.as_weak();
    let clip = clipboard.clone();
    ui.on_copy_address(move || {
        if let Some(ui) = weak.upgrade() {
            let result = clip
                .borrow_mut()
                .as_mut()
                .context("Clipboard unavailable")
                .and_then(|c| {
                    c.set_text(ui.get_server_url().to_string())
                        .map_err(Into::into)
                });
            ui.set_status(match result {
                Ok(_) => "Address copied".into(),
                Err(e) => e.to_string().into(),
            });
        }
    });
    let browser_url = url.clone();
    let callback_tx = tx.clone();
    ui.on_open_browser(move || report_open(&browser_url, &callback_tx));
    let root = store.root().to_owned();
    let callback_tx = tx.clone();
    ui.on_open_folder(move || report_open(&root, &callback_tx));
    let callback_tx = tx.clone();
    ui.on_open_github(move || report_open(GITHUB_URL, &callback_tx));
    let weak = ui.as_weak();
    let clip = clipboard.clone();
    ui.on_paste_text(move || {
        if let Some(ui) = weak.upgrade() {
            match clip
                .borrow_mut()
                .as_mut()
                .context("Clipboard unavailable")
                .and_then(|c| c.get_text().map_err(Into::into))
            {
                Ok(text) => ui.set_draft(format!("{}{text}", ui.get_draft()).into()),
                Err(e) => ui.set_status(format!("Could not paste text: {e}").into()),
            }
        }
    });
    let handle = runtime.handle().clone();
    let text_store = store.clone();
    let text_tx = tx.clone();
    let weak = ui.as_weak();
    ui.on_save_text(move |text| {
        let store = text_store.clone();
        let tx = text_tx.clone();
        if let Some(ui) = weak.upgrade() {
            ui.set_saving(true);
            ui.set_status("Saving text…".into());
        }
        handle.spawn_blocking(move || {
            match store.save_text(&text) {
                Ok(name) => {
                    let _ = tx.send(Update::TextSaved(name));
                }
                Err(e) => {
                    let _ = tx.send(Update::TextSaved(format!("ERROR:{e}")));
                }
            }
            if let Ok(files) = store.list() {
                let _ = tx.send(Update::Files(files));
            }
        });
    });
    let pick_store = store.clone();
    let pick_tx = tx.clone();
    let handle = runtime.handle().clone();
    ui.on_pick_files(move || {
        let store = pick_store.clone();
        let tx = pick_tx.clone();
        handle.spawn(async move {
            if let Some(files) = rfd::AsyncFileDialog::new()
                .set_title("Choose files to share")
                .pick_files()
                .await
            {
                let paths = files.into_iter().map(|f| f.path().to_owned()).collect();
                let _ = tokio::task::spawn_blocking(move || import_files(store, paths, tx)).await;
            }
        });
    });
    let drop_store = store.clone();
    let drop_tx = tx.clone();
    let handle = runtime.handle().clone();
    let weak = ui.as_weak();
    ui.window().on_winit_window_event(move |_, event| {
        if let Some(ui) = weak.upgrade() {
            match event {
                WindowEvent::Resized(size) => {
                    let scale = ui.window().scale_factor();
                    ui.set_compact((size.height as f32 / scale) < 810.0);
                    ui.set_wide_layout((size.width as f32 / scale) > 960.0);
                }
                WindowEvent::HoveredFile(_) => ui.set_dragging(true),
                WindowEvent::HoveredFileCancelled => ui.set_dragging(false),
                WindowEvent::DroppedFile(path) => {
                    ui.set_dragging(false);
                    let store = drop_store.clone();
                    let tx = drop_tx.clone();
                    let path = path.clone();
                    handle.spawn_blocking(move || import_files(store, vec![path], tx));
                }
                _ => {}
            }
        }
        EventResult::Propagate
    });
    let open_store = store.clone();
    let callback_tx = tx.clone();
    ui.on_open_file(move |name| {
        if open_store.open(&name).is_ok() {
            report_open(open_store.root().join(name.as_str()), &callback_tx);
        } else {
            let _ = callback_tx.send(Update::Status("File not found or not accessible".into()));
        }
    });
    let copy_store = store.clone();
    let weak = ui.as_weak();
    ui.on_copy_text(move |name| {
        let result = copy_store.read_text(&name).and_then(|text| {
            clipboard
                .borrow_mut()
                .as_mut()
                .context("Clipboard unavailable")?
                .set_text(text)?;
            Ok(())
        });
        if let Some(ui) = weak.upgrade() {
            ui.set_status(match result {
                Ok(_) => "Text copied".into(),
                Err(e) => e.to_string().into(),
            });
        }
    });
    let refresh_store = store.clone();
    let refresh_tx = tx.clone();
    let handle = runtime.handle().clone();
    ui.on_refresh(move || {
        let store = refresh_store.clone();
        let tx = refresh_tx.clone();
        handle.spawn_blocking(move || match store.list() {
            Ok(files) => {
                let _ = tx.send(Update::Files(files));
            }
            Err(e) => {
                let _ = tx.send(Update::Status(format!("Refresh failed: {e}")));
            }
        });
    });
    let refresh_store = store.clone();
    runtime.spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        let mut previous = None;
        loop {
            interval.tick().await;
            let store = refresh_store.clone();
            match tokio::task::spawn_blocking(move || store.list()).await {
                Ok(Ok(files)) if previous.as_ref() != Some(&files) => {
                    previous = Some(files.clone());
                    if tx.send(Update::Files(files)).is_err() {
                        break;
                    }
                }
                Ok(Err(e)) => {
                    let _ = tx.send(Update::Status(format!(
                        "Could not read the data folder: {e}"
                    )));
                }
                _ => {}
            }
        }
    });
    let timer = slint::Timer::default();
    let weak = ui.as_weak();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            let Some(ui) = weak.upgrade() else { return };
            for update in rx.try_iter() {
                match update {
                    Update::Status(status) => ui.set_status(status.into()),
                    Update::ServerStopped(status) => {
                        ui.set_online(false);
                        ui.set_status(status.into());
                    }
                    Update::TextSaved(name) => {
                        ui.set_saving(false);
                        if let Some(error) = name.strip_prefix("ERROR:") {
                            ui.set_status(format!("Save failed: {error}").into());
                        } else {
                            ui.set_draft("".into());
                            ui.set_status(format!("Saved {name}").into());
                        }
                    }
                    Update::Files(files) => {
                        ui.set_file_count(files.len() as i32);
                        ui.set_total_size(human_size(files.iter().map(|e| e.size).sum()).into());
                        ui.set_files(ModelRc::new(VecModel::from(
                            files
                                .into_iter()
                                .map(|e| FileRow {
                                    name: e.name.into(),
                                    detail: format!(
                                        "{} · {}",
                                        human_size(e.size),
                                        local_time(e.modified)
                                    )
                                    .into(),
                                    is_text: e.is_text,
                                })
                                .collect::<Vec<_>>(),
                        )));
                    }
                }
            }
        },
    );
    ui.show()?;
    // 必须在 show() 之后：winit 窗口这时才真正存在。
    // Windows 的资源段图标由 build.rs 负责，macOS 用 .app 里的 .icns，
    // 这一步主要是给 Linux 的任务栏和窗口管理器。
    ui.window().with_winit_window(|window| {
        window.set_window_icon(window_icon());
    });
    slint::run_event_loop()?;
    shutdown.cancel();
    runtime.shutdown_timeout(Duration::from_secs(3));
    Ok(())
}

/// 界面字体。界面文案本身是英文，但共享的文件名可能是任何语言，而软件渲染器
/// 只会用选中的那一个字型渲染整段文字，没有逐字回退——所以这里仍然点名一个
/// 自带汉字的系统字体（它们的拉丁字形也够用），否则中文文件名会变成豆腐块。
/// Linux 的中文字体名各发行版不一，返回空串让 fontconfig 决定 sans-serif；
/// 结果不合适时可用 Slint 的 `SLINT_DEFAULT_FONT` 环境变量指定字体文件或目录。
fn ui_font_family() -> &'static str {
    if cfg!(target_os = "macos") {
        "PingFang SC"
    } else if cfg!(target_os = "windows") {
        "Microsoft YaHei"
    } else {
        ""
    }
}

/// 箭头符号（↗ ↑ ↓）用的字体。苹方、微软雅黑这类中文字体不含这些字符，
/// 而软件渲染器不做逐字回退，缺字就是方块，所以单独点名一个带箭头的字体。
fn symbol_font_family() -> &'static str {
    if cfg!(target_os = "macos") {
        "Apple Symbols"
    } else if cfg!(target_os = "windows") {
        "Segoe UI Symbol"
    } else {
        ""
    }
}

fn report_open(path: impl AsRef<std::ffi::OsStr>, tx: &mpsc::Sender<Update>) {
    if let Err(e) = open::that_detached(path) {
        let _ = tx.send(Update::Status(format!("Could not open: {e}")));
    }
}
fn import_files(store: Store, paths: Vec<PathBuf>, tx: mpsc::Sender<Update>) {
    let _ = tx.send(Update::Status(format!(
        "Importing {}…",
        plural_files(paths.len())
    )));
    let mut success = 0;
    let mut errors = Vec::new();
    for path in paths {
        match store.import(&path) {
            Ok(_) => success += 1,
            Err(e) => errors.push(format!(
                "{}: {e}",
                path.file_name().unwrap_or_default().to_string_lossy()
            )),
        }
    }
    let shared = format!("Shared {}", plural_files(success));
    let message = if errors.is_empty() {
        shared
    } else {
        format!("{shared}; {}", errors.join("; "))
    };
    let _ = tx.send(Update::Status(message));
    if let Ok(files) = store.list() {
        let _ = tx.send(Update::Files(files));
    }
}
fn plural_files(count: usize) -> String {
    format!("{count} file{}", if count == 1 { "" } else { "s" })
}

/// 界面、窗口图标和网页共用同一张图；母图 assets/app-icon.png 只在打包时用。
pub const ICON_PNG: &[u8] = include_bytes!("../assets/app-icon-256.png");

fn window_icon() -> Option<i_slint_backend_winit::winit::window::Icon> {
    let pixels = image::load_from_memory_with_format(ICON_PNG, image::ImageFormat::Png)
        .ok()?
        .into_rgba8();
    let (width, height) = pixels.dimensions();
    i_slint_backend_winit::winit::window::Icon::from_rgba(pixels.into_raw(), width, height).ok()
}

const DATA_DIR_NAME: &str = "LanDropData";

/// 数据目录默认放在**可执行文件旁边**，保持免安装、可携带；
/// macOS 打成 .app 后是放在 **.app 旁边**（写进 bundle 内部会破坏代码签名）。
///
/// 例外：一旦 App 被拖进 Applications，"旁边"就成了 `/Applications`——
/// 那里是 root:admin 且 admin 用户可写，会被悄悄塞进一个用户数据目录，
/// 而且对非管理员账号不可写。这种情况改用个人目录下的同名文件夹。
fn data_dir() -> Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let beside = executable
        .parent()
        .context("Could not locate the app directory")?;
    let base = strip_bundle(beside).unwrap_or_else(|| beside.to_path_buf());
    if is_applications_dir(&base) {
        if let Some(home) = std::env::home_dir() {
            return Ok(home.join(DATA_DIR_NAME));
        }
    }
    Ok(base.join(DATA_DIR_NAME))
}

/// `<dir>/Foo.app/Contents/MacOS` -> `<dir>`，其余情况返回 None。
fn strip_bundle(dir: &Path) -> Option<PathBuf> {
    let contents = dir.parent()?;
    let bundle = contents.parent()?;
    if dir.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && bundle.extension()? == "app"
    {
        return bundle.parent().map(Path::to_path_buf);
    }
    None
}

/// 是否是 macOS 的标准应用目录（用户不应把数据写进去的地方）。
fn is_applications_dir(dir: &Path) -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    let system = [
        Path::new("/Applications"),
        Path::new("/System/Applications"),
    ];
    system.contains(&dir)
        || std::env::home_dir().is_some_and(|home| dir == home.join("Applications"))
}

/// Unix 秒转本地时间，格式和网页端的 `toLocaleString('en-US', {month:'short',
/// day:'numeric', hour:'2-digit', minute:'2-digit', hour12:false})` 对齐。
fn local_time(secs: u64) -> String {
    DateTime::<Local>::from(UNIX_EPOCH + Duration::from_secs(secs))
        .format("%b %-d, %H:%M")
        .to_string()
}

fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / 1048576.)
    } else {
        format!("{:.2} GB", bytes as f64 / 1073741824.)
    }
}
fn bind_listener(port: u16) -> Result<std::net::TcpListener> {
    for candidate in port..=port.saturating_add(if port == 0 { 0 } else { 20 }) {
        match std::net::TcpListener::bind(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            candidate,
        )) {
            Ok(listener) => {
                listener.set_nonblocking(true)?;
                return Ok(listener);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(e) => return Err(e).context("Could not start the HTTP server"),
        }
    }
    anyhow::bail!(
        "port {port} and the 20 ports after it are all in use; pass --port to pick another"
    )
}
fn lan_urls(port: u16) -> Vec<String> {
    let mut ips: Vec<Ipv4Addr> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|i| match i.ip() {
            IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() && !ip.is_link_local() => {
                Some(ip)
            }
            _ => None,
        })
        .collect();
    let preferred = std::net::UdpSocket::bind("0.0.0.0:0").ok().and_then(|s| {
        s.connect("192.0.2.1:80").ok()?;
        s.local_addr().ok().map(|a| a.ip())
    });
    ips.sort_by_key(|ip| (!ip.is_private(), Some(IpAddr::V4(*ip)) != preferred, *ip));
    ips.dedup();
    let mut urls: Vec<_> = ips
        .into_iter()
        .map(|ip| format!("http://{ip}:{port}"))
        .collect();
    if urls.is_empty() {
        urls.push(format!("http://127.0.0.1:{port}"));
    }
    urls
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_paths_resolve_beside_the_app() {
        assert_eq!(
            strip_bundle(Path::new("/Volumes/U/LAN Drop.app/Contents/MacOS")),
            Some(PathBuf::from("/Volumes/U"))
        );
        // 裸二进制、以及形似但不完整的路径都不该被当成 bundle。
        assert_eq!(strip_bundle(Path::new("/Users/me/Downloads")), None);
        assert_eq!(strip_bundle(Path::new("/x/LAN Drop/Contents/MacOS")), None);
        assert_eq!(strip_bundle(Path::new("/x/LAN Drop.app/Other/MacOS")), None);
        assert_eq!(
            strip_bundle(Path::new("/x/LAN Drop.app/Contents/Bin")),
            None
        );
    }

    #[test]
    fn only_real_application_folders_divert_to_home() {
        if cfg!(target_os = "macos") {
            assert!(is_applications_dir(Path::new("/Applications")));
            assert!(is_applications_dir(Path::new("/System/Applications")));
        }
        assert!(!is_applications_dir(Path::new("/Applications/Sub")));
        assert!(!is_applications_dir(Path::new("/Users/me/Downloads")));
    }
}
