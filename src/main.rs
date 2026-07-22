use axum::{
    Router,
    body::Body,
    extract::{Path as AxumPath, Request, State},
    http::StatusCode,
    response::Response,
    response::{Html, Json},
    routing::{delete, get, post},
};
use axum_server::tls_rustls::RustlsConfig;
use clap::Parser;
use futures::StreamExt;
use libheif_rs::{ColorSpace, HeifContext, LibHeif, RgbChroma};
use mime_guess;
use multer::{Constraints, Multipart, SizeLimit, parse_boundary};
use rcgen::generate_simple_self_signed;
use sanitize_filename::sanitize;
use serde_json::json;
use std::collections::HashMap;
use std::env::{consts, current_dir};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use supports_color::Stream;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use tower_http::services::ServeDir;
use tower_http::timeout::TimeoutLayer;
use tracing::subscriber::set_global_default;
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, Layer, Registry, fmt, layer::SubscriberExt};

const INDEX_HTML: &str = include_str!("../templates/index.html");

const THUMB_EXT: &str = "webp";

struct AppState {
    files_dir: PathBuf,
    upload_progress: Arc<Mutex<HashMap<String, usize>>>,
    thumb_semaphore: Arc<Semaphore>,
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[arg(short, long, default_value_t = 3000)]
    port: u16,

    #[arg(short, long)]
    dir: Option<PathBuf>,

    #[arg(short, long, default_value_t = 300)]
    timeout: u64,

    #[arg(short = 's', long, default_value_t = false)]
    https: bool,
}

struct ProgressEntry {
    map: Arc<Mutex<HashMap<String, usize>>>,
    filename: String,
}

impl ProgressEntry {
    fn new(map: Arc<Mutex<HashMap<String, usize>>>, filename: String) -> Self {
        {
            let mut guard = map.lock().unwrap();
            guard.insert(filename.clone(), 0);
        }
        Self { map, filename }
    }

    fn update(&self, bytes: usize) {
        let mut guard = self.map.lock().unwrap();
        if let Some(entry) = guard.get_mut(&self.filename) {
            *entry = bytes;
        }
    }
}

impl Drop for ProgressEntry {
    fn drop(&mut self) {
        let mut guard = self.map.lock().unwrap();
        guard.remove(&self.filename);
    }
}

fn is_termux() -> bool {
    std::env::var("TERMUX_VERSION").is_ok()
        || std::env::var("PREFIX")
            .map(|p| p.contains("com.termux"))
            .unwrap_or(false)
}

fn default_files_dir() -> PathBuf {
    match dirs::home_dir() {
        Some(home) => home.join("rushdrop_data").join("files"),
        None => current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("files"),
    }
}

fn get_files_dir() -> anyhow::Result<PathBuf> {
    if !is_termux() {
        return Ok(default_files_dir());
    }

    println!("\n📱 检测到 Termux 环境。是否将文件保存到公共存储目录（如图库、下载等）？");
    println!("这将使文件在系统文件管理器中直接可见。");
    print!("输入 y/yes 确认，其他任意键跳过: ");
    Write::flush(&mut std::io::stdout())?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let input = input.trim().to_lowercase();

    if input != "y" && input != "yes" && input != "ok" {
        info!("使用默认存储目录（Termux 私有空间）");
        return Ok(default_files_dir());
    }

    let storage_dir = match dirs::home_dir() {
        Some(home) => home.join("storage"),
        None => {
            warn!("无法获取主目录，回退到默认存储");
            return Ok(default_files_dir());
        }
    };

    if !storage_dir.exists() {
        warn!("未检测到 ~/storage 目录，请先运行 `termux-setup-storage` 授权存储权限。");
        println!("回退到默认存储目录。");
        return Ok(default_files_dir());
    }

    let options = [
        ("Pictures", "pictures"),
        ("DCIM", "dcim"),
        ("Downloads", "downloads"),
        ("Music", "music"),
        ("Movies", "movies"),
    ];
    println!("\n请选择保存目录（输入编号）：");
    for (i, (name, _)) in options.iter().enumerate() {
        println!("  {}. {}", i + 1, name);
    }
    println!("  {}. 自定义路径", options.len() + 1);

    let mut choice = String::new();
    std::io::stdin().read_line(&mut choice)?;
    let choice = choice.trim();

    let selected_dir = if let Ok(num) = choice.parse::<usize>() {
        if num >= 1 && num <= options.len() {
            Some(storage_dir.join(options[num - 1].1))
        } else if num == options.len() + 1 {
            print!("请输入完整路径: ");
            Write::flush(&mut std::io::stdout())?;
            let mut custom = String::new();
            std::io::stdin().read_line(&mut custom)?;
            let custom = custom.trim();
            if custom.is_empty() {
                None
            } else {
                Some(PathBuf::from(custom))
            }
        } else {
            None
        }
    } else {
        None
    };

    if let Some(dir) = selected_dir {
        info!("已选择公共存储目录: {}", dir.display());
        Ok(dir)
    } else {
        warn!("无效选择，回退到默认存储目录。");
        Ok(default_files_dir())
    }
}

fn thumb_dir(files_dir: &Path) -> PathBuf {
    files_dir.parent().unwrap_or(files_dir).join("thumbnails")
}

fn is_image(filename: &str) -> bool {
    let ext = Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    matches!(
        ext.as_str(),
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "heic"
    )
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

fn generate_thumbnail_sync(orig_path: &Path, thumb_path: &Path) -> anyhow::Result<()> {
    let ext = orig_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let img = if ext == "heic" {
        let lib_heif = LibHeif::new();
        let path_str = orig_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("路径包含非 UTF-8 字符: {}", orig_path.display()))?;
        let ctx = HeifContext::read_from_file(path_str)
            .map_err(|e| anyhow::anyhow!("读取 HEIC 文件失败: {}", e))?;
        let handle = ctx
            .primary_image_handle()
            .map_err(|e| anyhow::anyhow!("获取主图像失败: {}", e))?;
        let rgb = lib_heif
            .decode(&handle, ColorSpace::Rgb(RgbChroma::Rgb), None)
            .map_err(|e| anyhow::anyhow!("解码 HEIC 失败: {}", e))?;
        let planes = rgb.planes();
        let width = rgb.width();
        let height = rgb.height();
        let data = planes
            .interleaved
            .ok_or_else(|| anyhow::anyhow!("无法获取 interleaved 数据"))?;
        let buffer = image::ImageBuffer::from_raw(width, height, data.data.to_vec())
            .ok_or_else(|| anyhow::anyhow!("无法构建 ImageBuffer"))?;
        image::DynamicImage::ImageRgb8(buffer)
    } else {
        image::open(orig_path).map_err(|e| anyhow::anyhow!("无法解码图片: {}", e))?
    };

    let thumbnail = img.thumbnail(200, 200);
    thumbnail.save(thumb_path)?;
    Ok(())
}

fn spawn_thumbnail(files_dir: PathBuf, filename: String, semaphore: Arc<Semaphore>) {
    tokio::spawn(async move {
        let Ok(_permit) = semaphore.acquire().await else {
            error!("信号量已关闭，跳过缩略图生成");
            return;
        };

        let orig_path = files_dir.join(&filename);
        let thumb_dir = thumb_dir(&files_dir);
        if let Err(e) = fs::create_dir_all(&thumb_dir).await {
            error!("创建缩略图目录失败: {}", e);
            return;
        }
        let thumb_path = thumb_dir.join(&filename).with_extension(THUMB_EXT);
        if thumb_path.exists() {
            return;
        }
        let result =
            tokio::task::spawn_blocking(move || generate_thumbnail_sync(&orig_path, &thumb_path))
                .await;
        match result {
            Ok(Ok(())) => info!("缩略图生成成功: {}", filename),
            Ok(Err(e)) => error!("生成缩略图失败 {}: {}", filename, e),
            Err(e) => error!("spawn_blocking 失败 {}: {}", filename, e),
        }
    });
}

async fn index(State(state): State<Arc<AppState>>) -> Html<String> {
    let files_dir = &state.files_dir;
    let mut file_list_html = String::new();

    let mut entries = Vec::new();
    if let Ok(mut read_dir) = fs::read_dir(files_dir).await {
        while let Ok(Some(entry)) = read_dir.next_entry().await {
            if let Ok(metadata) = entry.metadata().await {
                if metadata.is_file() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                    let size = metadata.len();
                    entries.push((modified, name, size));
                }
            }
        }
    }

    entries.sort_by(|a, b| b.0.cmp(&a.0));

    for (_, name, _) in &entries {
        if is_image(name) {
            let files_dir_clone = files_dir.clone();
            let semaphore = state.thumb_semaphore.clone();
            spawn_thumbnail(files_dir_clone, name.to_owned(), semaphore);
        }
    }

    for (_, name, size) in entries {
        let size_str = format_size(size);
        let safe_name = name
            .replace('"', "&quot;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");

        let thumbnail_html = if is_image(&name) {
            format!(
                r#"<img src="/thumb/{}" loading="lazy" width="80" height="80" style="object-fit:cover;border-radius:4px;" onerror="setTimeout(()=>this.src='/thumb/{}?'+Date.now(), 1000); this.onerror=null;">"#,
                safe_name, safe_name
            )
        } else {
            String::new()
        };

        file_list_html.push_str(&format!(
            r#"<li>
            {}
            <a href="/files/{}" download>{}</a>
            <span class="file-size">({})</span>
            <button class="delete-btn" data-filename="{}">🗑️ 删除</button>
        </li>"#,
            thumbnail_html, safe_name, safe_name, size_str, safe_name
        ));
    }

    let device_title = match consts::OS {
        "windows" => "Windows 上的文件",
        "macos" => "Mac 上的文件",
        "android" => "Android 上的文件",
        "linux" => "Linux 上的文件",
        _ => "设备上的文件",
    };

    let html = INDEX_HTML
        .replace("{device_title}", device_title)
        .replace("{}", &file_list_html);

    Html(html)
}

async fn upload(
    State(state): State<Arc<AppState>>,
    req: Request,
) -> Result<Html<String>, (StatusCode, String)> {
    let content_type = req
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            let msg = "缺少 Content-Type 头".to_string();
            error!("{}", msg);
            (StatusCode::BAD_REQUEST, msg)
        })?;

    let boundary = parse_boundary(content_type).map_err(|e| {
        let msg = format!("解析 boundary 失败: {}", e);
        error!("{}", msg);
        (StatusCode::BAD_REQUEST, msg)
    })?;

    let size_limit = SizeLimit::new()
        .whole_stream(1024 * 1024 * 1024)
        .per_field(1024 * 1024 * 1024);

    let constraints = Constraints::new().size_limit(size_limit);

    let body_stream = req.into_body().into_data_stream();
    let mut multipart = Multipart::with_constraints(body_stream, boundary, constraints);

    let save_dir = &state.files_dir;
    let canonical_base = dunce::canonicalize(save_dir).map_err(|e| {
        let msg = format!("服务器路径规范化失败: {}", e);
        error!("{}", msg);
        (StatusCode::INTERNAL_SERVER_ERROR, msg)
    })?;

    let progress_map = state.upload_progress.clone();

    while let Some(mut field) = multipart.next_field().await.map_err(|e| {
        let err_msg = format!("读取 multipart 字段失败: {}", e);
        error!("{}", err_msg);
        (StatusCode::BAD_REQUEST, err_msg)
    })? {
        let raw_name = if let Some(name) = field.file_name() {
            name.to_string()
        } else {
            let file_extension = field.content_type().and_then(|mime| {
                if mime.type_() == "image" && mime.subtype() == "heic" {
                    return Some("heic");
                }
                mime_guess::get_mime_extensions(mime).and_then(|exts| exts.first().copied())
            });

            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis();
            let ext = file_extension
                .map(|e| format!(".{}", e))
                .unwrap_or_default();
            format!("unnamed_{}{}", now, ext)
        };

        let progress_entry = ProgressEntry::new(progress_map.clone(), raw_name.clone());

        let safe_name = sanitize(&raw_name);
        let file_path = canonical_base.join(&safe_name);

        let mut file = fs::File::create(&file_path).await.map_err(|e| {
            let err_msg = format!("创建文件 '{}' 失败: {}", file_path.display(), e);
            error!("{}", err_msg);
            (StatusCode::INTERNAL_SERVER_ERROR, err_msg)
        })?;

        let mut total_bytes = 0;
        let mut last_updated = 0;
        const UPDATE_THRESHOLD: usize = 64 * 1024;

        while let Some(chunk_result) = field.next().await {
            let chunk = chunk_result.map_err(|e| {
                let err_msg = format!(
                    "读取文件 '{}' 数据块失败 (已接收 {} 字节): {}",
                    raw_name, total_bytes, e
                );
                error!("{}", err_msg);
                if e.to_string().contains("limit") {
                    (StatusCode::PAYLOAD_TOO_LARGE, err_msg)
                } else {
                    (StatusCode::BAD_REQUEST, err_msg)
                }
            })?;

            file.write_all(&chunk).await.map_err(|e| {
                let err_msg = format!("写入文件 '{}' 失败: {}", file_path.display(), e);
                error!("{}", err_msg);
                (StatusCode::INTERNAL_SERVER_ERROR, err_msg)
            })?;

            total_bytes += chunk.len();

            if total_bytes - last_updated >= UPDATE_THRESHOLD {
                progress_entry.update(total_bytes);
                last_updated = total_bytes;
            }
        }

        file.flush().await.map_err(|e| {
            let err_msg = format!("刷新文件 '{}' 失败: {}", file_path.display(), e);
            error!("{}", err_msg);
            (StatusCode::INTERNAL_SERVER_ERROR, err_msg)
        })?;

        progress_entry.update(total_bytes);
        drop(progress_entry);

        info!(
            "✅ 上传成功: {} ({} 字节) -> {}",
            raw_name,
            total_bytes,
            file_path.display()
        );
    }

    Ok(Html("<p>上传成功</p><p><a href='/'>返回首页</a></p>".to_string()))
}

async fn progress_handler(
    AxumPath(filename): AxumPath<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let map = state.upload_progress.lock().map_err(|_| {
        error!("获取进度锁失败");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    if let Some(&progress) = map.get(&filename) {
        Ok(Json(json!({ "progress": progress })))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

async fn delete_file(
    State(state): State<Arc<AppState>>,
    AxumPath(filename): AxumPath<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let file_path = state.files_dir.join(&filename);

    let canonical_base = dunce::canonicalize(&state.files_dir).map_err(|e| {
        let msg = format!("服务器路径规范化失败: {}", e);
        error!("{}", msg);
        (StatusCode::INTERNAL_SERVER_ERROR, msg)
    })?;
    let canonical_full = dunce::canonicalize(&file_path).map_err(|_| {
        let msg = "无效的文件路径".to_string();
        error!("{}", msg);
        (StatusCode::BAD_REQUEST, msg)
    })?;
    if !canonical_full.starts_with(&canonical_base) {
        let msg = "路径遍历攻击尝试".to_string();
        error!("路径遍历攻击尝试: {}", filename);
        return Err((StatusCode::FORBIDDEN, msg));
    }

    match fs::remove_file(&file_path).await {
        Ok(_) => {
            let thumb_path = thumb_dir(&state.files_dir)
                .join(&filename)
                .with_extension(THUMB_EXT);
            let _ = fs::remove_file(thumb_path).await;
            info!("文件删除成功: {}", filename);
            Ok(Json(json!({ "success": true })))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let msg = format!("文件不存在: {}", filename);
            error!("{}", msg);
            Err((StatusCode::NOT_FOUND, msg))
        }
        Err(e) => {
            let msg = format!("删除文件失败: {}", e);
            error!("{}", msg);
            Err((StatusCode::INTERNAL_SERVER_ERROR, msg))
        }
    }
}

async fn thumb_handler(
    AxumPath(filename): AxumPath<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Response, (StatusCode, String)> {
    let thumb_dir = thumb_dir(&state.files_dir);
    let thumb_path = thumb_dir.join(&filename).with_extension(THUMB_EXT);

    let canonical_base = dunce::canonicalize(&thumb_dir).map_err(|e| {
        let msg = format!("缩略图目录规范化失败: {}", e);
        error!("{}", msg);
        (StatusCode::INTERNAL_SERVER_ERROR, msg)
    })?;

    if thumb_path.strip_prefix(&canonical_base).is_err() {
        let msg = "路径遍历攻击尝试 (缩略图)".to_string();
        error!("{}: {}", msg, filename);
        return Err((StatusCode::FORBIDDEN, msg));
    }

    if !thumb_path.exists() {
        let files_dir = state.files_dir.clone();
        let semaphore = state.thumb_semaphore.clone();
        spawn_thumbnail(files_dir, filename, semaphore);
        return Err((StatusCode::NOT_FOUND, "缩略图未生成".to_string()));
    }

    match fs::read(&thumb_path).await {
        Ok(data) => {
            let mime = mime_guess::from_path(&thumb_path).first_or_octet_stream();
            let body = Body::from(data);
            let response = Response::builder()
                .header("Content-Type", mime.as_ref())
                .body(body)
                .map_err(|e| {
                    let msg = format!("构建响应失败: {}", e);
                    error!("{}", msg);
                    (StatusCode::INTERNAL_SERVER_ERROR, msg)
                })?;
            Ok(response)
        }
        Err(e) => {
            let msg = format!("读取缩略图失败: {}", e);
            error!("{}", msg);
            Err((StatusCode::INTERNAL_SERVER_ERROR, msg))
        }
    }
}

async fn start_http(addr: String, app: Router) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("✅ HTTP 服务已启动，监听 {}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn start_https(addr: String, app: Router) -> anyhow::Result<()> {
    let cert_path = Path::new("cert.pem");
    let key_path = Path::new("key.pem");

    if !cert_path.exists() || !key_path.exists() {
        info!("未找到证书文件，正在生成自签名证书...");
        let ip_str = addr.split(':').next().unwrap_or("127.0.0.1");
        let subject_alt_names = vec!["localhost".to_string(), ip_str.to_string()];
        let cert = generate_simple_self_signed(subject_alt_names)?;

        let cert_pem = cert.cert.pem();
        let key_pem = cert.signing_key.serialize_pem();

        fs::write(cert_path, cert_pem).await?;
        fs::write(key_path, key_pem).await?;
        info!("✅ 自签名证书已生成并保存为 cert.pem / key.pem");
    }

    let cert_bytes = fs::read(cert_path).await?;
    let key_bytes = fs::read(key_path).await?;

    let config = RustlsConfig::from_pem(cert_bytes, key_bytes).await?;

    info!("✅ HTTPS 服务已启动，监听 {}", addr);
    let socket_addr: std::net::SocketAddr = addr.parse()?;
    axum_server::bind_rustls(socket_addr, config)
        .serve(app.into_make_service())
        .await?;

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    #[cfg(windows)]
    enable_ansi_support::enable_ansi_support().ok();

    // 日志初始化
    let current_dir = current_dir()?;
    let log_dir = current_dir.join("logs");
    if !log_dir.exists() {
        fs::create_dir_all(&log_dir).await?;
    }
    let file_appender = tracing_appender::rolling::daily(&log_dir, "rushdrop.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    let file_layer = fmt::layer().with_writer(non_blocking).with_ansi(false);

    let enable_ansi = if std::env::var("NO_COLOR").is_ok() {
        false
    } else if cfg!(windows) {
        supports_color::on(Stream::Stdout).map_or(false, |s| s.has_basic)
    } else {
        true
    };

    let console_layer = if enable_ansi {
        fmt::layer()
            .with_writer(std::io::stdout)
            .with_ansi(enable_ansi)
            .pretty()
            .boxed()
    } else {
        fmt::layer()
            .with_writer(std::io::stdout)
            .with_ansi(enable_ansi)
            .with_target(false)
            .with_thread_ids(false)
            .with_level(true)
            .boxed()
    };

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"))
        .add_directive("rushdrop=info".parse()?);

    let subscriber = Registry::default()
        .with(filter)
        .with(console_layer)
        .with(file_layer);
    set_global_default(subscriber).expect("设置全局日志订阅者失败");

    // 目录
    let files_dir = if let Some(dir) = cli.dir {
        dir
    } else {
        get_files_dir()?
    };

    if !files_dir.exists() {
        fs::create_dir_all(&files_dir).await?;
    }

    let abs_path = dunce::canonicalize(&files_dir).unwrap_or_else(|_| files_dir.clone());
    info!("📁 文件存储目录: {}", abs_path.display());

    let thumb_semaphore = Arc::new(Semaphore::new(4));

    let state = Arc::new(AppState {
        files_dir: files_dir.clone(),
        upload_progress: Arc::new(Mutex::new(HashMap::new())),
        thumb_semaphore,
    });

    // 地址
    let ip = local_ip_address::local_ip().unwrap_or_else(|e| {
        warn!("⚠️ 获取本机局域网 IP 失败: {}，将使用 127.0.0.1", e);
        warn!("请确保电脑已连接网络（Wi-Fi 或热点），否则手机无法访问！");
        "127.0.0.1".parse().unwrap()
    });
    let port = cli.port;
    let addr = format!("{}:{}", ip, port);

    let protocol = if cli.https { "https" } else { "http" };
    let url = format!("{}://{}", protocol, addr);

    if let Err(e) = qr2term::print_qr(&url) {
        warn!("打印二维码失败: {}", e);
    }
    println!("\n访问地址: {}", url);
    if ip.is_loopback() {
        println!("❗ 当前 IP 为回环地址，手机若不在同一电脑上无法访问，请检查网络连接。");
    }
    if cli.https {
        println!("🔒 HTTPS 已启用，浏览器会提示不安全，请点击“继续访问”或“高级”->“继续前往”。");
    }
    println!("按 Ctrl+C 停止服务\n");

    // 路由
    let upload_route =
        Router::new()
            .route("/upload", post(upload))
            .layer(TimeoutLayer::with_status_code(
                StatusCode::REQUEST_TIMEOUT,
                Duration::from_secs(cli.timeout),
            ));

    let app = Router::new()
        .route("/", get(index))
        .route("/progress/{filename}", get(progress_handler))
        .route("/delete/{filename}", delete(delete_file))
        .route("/thumb/{filename}", get(thumb_handler))
        .merge(upload_route)
        .nest_service("/files", ServeDir::new(files_dir))
        .with_state(state);

    // 启动
    if cli.https {
        start_https(addr, app).await?;
    } else {
        start_http(addr, app).await?;
    }

    Ok(())
}
