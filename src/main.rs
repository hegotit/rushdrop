use askama::Template;
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Path as AxumPath, Request, State},
    http::{StatusCode, header::CONTENT_TYPE},
    response::{Html, IntoResponse, Json, Response},
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
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use supports_color::Stream;
use thiserror::Error;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use tower_http::services::ServeDir;
use tower_http::timeout::TimeoutLayer;
use tracing::subscriber::set_global_default;
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, Layer, Registry, fmt, layer::SubscriberExt};

const THUMB_EXT: &str = "webp";

const PRESET_DIRS: &[(&str, &str)] = &[
    ("Pictures", "pictures"),
    ("DCIM", "dcim"),
    ("Downloads", "downloads"),
    ("Music", "music"),
    ("Movies", "movies"),
];

enum SelectedDir {
    Preset(PathBuf),
    Custom(PathBuf),
}

#[derive(Debug, Error)]
pub enum AppError {
    #[error("缺少 Content-Type 头")]
    MissingContentType,
    #[error("Content-Type 头包含非 UTF-8 字符: {0}")]
    InvalidContentType(#[from] axum::http::header::ToStrError),
    #[error("multipart 处理错误: {0}")]
    MulterError(#[from] multer::Error),
    #[error("文件 I/O 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("路径遍历攻击")]
    PathTraversal,
    #[error("文件不存在: {0}")]
    NotFound(String),
    #[error("内部错误: {0}")]
    Internal(String),
    #[error("缩略图尚未生成，请稍后重试")]
    ThumbnailNotReady,

    #[error("获取锁失败")]
    LockError,
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        AppError::Internal(e.to_string())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, msg) = match &self {
            AppError::MissingContentType => (StatusCode::BAD_REQUEST, self.to_string()),
            AppError::InvalidContentType(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            AppError::MulterError(e) => {
                let status = match e {
                    multer::Error::FieldSizeExceeded { .. }
                    | multer::Error::StreamSizeExceeded { .. } => StatusCode::PAYLOAD_TOO_LARGE,
                    _ => StatusCode::BAD_REQUEST,
                };
                (status, self.to_string())
            }
            AppError::Io(_) => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
            AppError::PathTraversal => (StatusCode::FORBIDDEN, self.to_string()),
            AppError::NotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            AppError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
            AppError::ThumbnailNotReady => (StatusCode::NOT_FOUND, self.to_string()),
            AppError::LockError => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
        };

        if matches!(&self, AppError::ThumbnailNotReady) {
            warn!("{}", msg);
        } else {
            error!("请求错误: {} (状态码 {})", msg, status);
        }

        (status, msg).into_response()
    }
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    device_title: String,
    file_list: String,
}

struct AppState {
    files_dir: PathBuf,
    thumb_dir: PathBuf,
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

fn read_user_input(prompt: &str) -> anyhow::Result<String> {
    print!("{}", prompt);
    Write::flush(&mut std::io::stdout())?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_string())
}

fn confirm_use_public_storage() -> anyhow::Result<bool> {
    let prompt = "\n📱 检测到 Termux 环境。是否将文件保存到公共存储目录（如图库、下载等）？\n这将使文件在系统文件管理器中直接可见。\n输入 y/yes 确认，其他任意键跳过: ";
    let input = read_user_input(prompt)?;
    Ok(input == "y" || input == "yes" || input == "ok")
}

fn select_preset_or_custom_directory(storage_dir: &Path) -> anyhow::Result<SelectedDir> {
    println!("\n请选择保存目录（输入编号）：");
    for (i, (name, _)) in PRESET_DIRS.iter().enumerate() {
        println!("  {}. {}", i + 1, name);
    }
    println!("  {}. 自定义路径", PRESET_DIRS.len() + 1);

    let input = read_user_input("")?;
    let num = input
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("无效数字"))?;

    if num >= 1 && num <= PRESET_DIRS.len() {
        let path = storage_dir.join(PRESET_DIRS[num - 1].1);
        Ok(SelectedDir::Preset(path))
    } else if num == PRESET_DIRS.len() + 1 {
        let custom = read_user_input("请输入完整路径: ")?;
        if custom.is_empty() {
            anyhow::bail!("路径为空");
        }
        Ok(SelectedDir::Custom(PathBuf::from(custom)))
    } else {
        anyhow::bail!("无效编号");
    }
}

fn choose_subdirectory(dir: &Path) -> anyhow::Result<Option<PathBuf>> {
    if !dir.is_dir() {
        return Ok(None);
    }

    let subdirs = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        Err(e) => {
            warn!("读取目录 {} 失败: {}", dir.display(), e);
            return Ok(None);
        }
    };

    if subdirs.is_empty() {
        return Ok(None);
    }

    println!("\n检测到 {} 下有以下子目录：", dir.display());
    for (idx, name) in subdirs.iter().enumerate() {
        println!("  {}. {}", idx + 1, name);
    }
    println!("  {}. 使用根目录", subdirs.len() + 1);

    let choice = read_user_input("")?;
    if let Ok(num) = choice.parse::<usize>() {
        if num >= 1 && num <= subdirs.len() {
            let final_dir = dir.join(&subdirs[num - 1]);
            info!("已选择子目录: {}", final_dir.display());
            return Ok(Some(final_dir));
        }
    }
    Ok(None)
}

fn get_files_dir() -> anyhow::Result<PathBuf> {
    let default = default_files_dir();
    if !is_termux() {
        return Ok(default);
    }

    if !confirm_use_public_storage()? {
        info!("使用默认存储目录（Termux 私有空间）");
        return Ok(default);
    }

    let storage_dir = match dirs::home_dir() {
        Some(home) => home.join("storage"),
        None => {
            warn!("无法获取主目录，回退到默认存储");
            return Ok(default);
        }
    };

    if !storage_dir.exists() {
        warn!("未检测到 ~/storage 目录，请先运行 `termux-setup-storage` 授权存储权限。");
        println!("回退到默认存储目录。");
        return Ok(default);
    }

    let selected = match select_preset_or_custom_directory(&storage_dir) {
        Ok(dir) => dir,
        Err(e) => {
            warn!("选择目录失败: {}，回退到默认存储目录。", e);
            return Ok(default);
        }
    };

    let selected_dir = match selected {
        SelectedDir::Preset(path) => {
            // 只有预设目录才尝试展开子目录
            if let Some(subdir) = choose_subdirectory(&path)? {
                subdir
            } else {
                path
            }
        }
        SelectedDir::Custom(path) => path,
    };

    info!("使用目录: {}", selected_dir.display());
    Ok(selected_dir)
}

fn thumb_dir(files_dir: &Path) -> PathBuf {
    files_dir.parent().unwrap_or(files_dir).join("thumbnails")
}

async fn ensure_thumb_dir(files_dir: &Path) -> Result<PathBuf, std::io::Error> {
    let dir = thumb_dir(files_dir);
    fs::create_dir_all(&dir).await?;
    Ok(dir)
}

fn thumb_base_name(files_dir: &Path, filename: &str) -> String {
    let canonical = dunce::canonicalize(files_dir).unwrap_or_else(|_| files_dir.to_path_buf());
    let hash = blake3::hash(canonical.to_string_lossy().as_bytes());
    let hash_hex = hash.to_hex().to_string();
    let short_hash = &hash_hex[..16];
    format!("{}_{}", short_hash, filename)
}

fn is_image(filename: &str) -> bool {
    let ext = Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    matches!(
        ext.as_str(),
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "heic" | "heif"
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

    let img = if ext == "heic" || ext == "heif" {
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

        let thumb_dir = match ensure_thumb_dir(&files_dir).await {
            Ok(dir) => dir,
            Err(e) => {
                error!("创建缩略图目录失败: {}", e);
                return;
            }
        };

        let thumb_name = thumb_base_name(&files_dir, &filename);
        let thumb_path = thumb_dir.join(thumb_name).with_extension(THUMB_EXT);
        if thumb_path.exists() {
            return;
        }

        let orig_path = files_dir.join(&filename);
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

    let template = IndexTemplate {
        device_title: device_title.into(),
        file_list: file_list_html,
    };
    let rendered = template.render().unwrap_or_else(|e| {
        error!("模板渲染失败: {}", e);
        "模板渲染错误".to_string()
    });
    Html(rendered)
}

async fn upload(
    State(state): State<Arc<AppState>>,
    req: Request,
) -> Result<Html<String>, AppError> {
    let content_type = req
        .headers()
        .get(CONTENT_TYPE)
        .ok_or(AppError::MissingContentType)?
        .to_str()?;

    let boundary = parse_boundary(content_type)?;

    let constraints = Constraints::new().size_limit(
        SizeLimit::new()
            .whole_stream(1024 * 1024 * 1024)
            .per_field(1024 * 1024 * 1024),
    );

    let mut multipart =
        Multipart::with_constraints(req.into_body().into_data_stream(), boundary, constraints);

    let canonical_base = dunce::canonicalize(&state.files_dir)?;

    let progress_map = state.upload_progress.clone();

    while let Some(mut field) = multipart.next_field().await? {
        let (file_name, first_chunk) = generate_filename_from_field(&mut field).await?;
        let progress_entry = ProgressEntry::new(progress_map.clone(), file_name.clone());

        let safe_name = sanitize(&file_name);
        let file_path = canonical_base.join(&safe_name);

        let mut file = fs::File::create(&file_path).await?;

        let mut total_bytes = 0;
        let mut last_updated = 0;
        const UPDATE_THRESHOLD: usize = 64 * 1024;

        if let Some(data) = first_chunk {
            file.write_all(&data).await?;
            total_bytes += data.len();
            if total_bytes - last_updated >= UPDATE_THRESHOLD {
                progress_entry.update(total_bytes);
                last_updated = total_bytes;
            }
        }

        while let Some(chunk) = field.next().await.transpose()? {
            file.write_all(&chunk).await?;
            total_bytes += chunk.len();
            if total_bytes - last_updated >= UPDATE_THRESHOLD {
                progress_entry.update(total_bytes);
                last_updated = total_bytes;
            }
        }

        file.flush().await?;
        progress_entry.update(total_bytes);
        drop(progress_entry);

        info!(
            "✅ 上传成功: {} ({} 字节) -> {}",
            file_name,
            total_bytes,
            file_path.display()
        );
    }

    Ok(Html(
        "<p>上传成功</p><p><a href='/'>返回首页</a></p>".to_string(),
    ))
}

async fn generate_filename_from_field(
    field: &mut multer::Field<'_>,
) -> Result<(String, Option<Bytes>), AppError> {
    let provided_name = field.file_name().map(|s| s.to_string());
    let content_type = field.content_type();

    let has_extension = provided_name
        .as_ref()
        .and_then(|name| Path::new(name).extension())
        .is_some();

    let need_magic_check = !has_extension
        && content_type.map_or(true, |m| {
            m.type_() == "application" && m.subtype() == "octet-stream"
        });

    let base_name = match provided_name {
        Some(name) if !name.is_empty() => name,
        _ => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis();
            format!("unnamed_{}", now)
        }
    };

    if need_magic_check {
        let chunk = field.next().await.transpose()?;
        if let Some(chunk) = chunk {
            let ext = infer::Infer::new().get(&chunk).map(|kind| kind.extension());
            let final_name = if let Some(ext) = ext {
                format!("{}.{}", base_name, ext)
            } else {
                base_name
            };
            Ok((final_name, Some(chunk)))
        } else {
            Ok((base_name, None))
        }
    } else {
        let final_name = if !has_extension {
            if let Some(ext) = content_type
                .and_then(|mime| mime_guess::get_mime_extensions(mime))
                .and_then(|exts| {
                    if exts.contains(&"jpg") {
                        Some("jpg")
                    } else {
                        exts.first().copied()
                    }
                })
            {
                format!("{}.{}", base_name, ext)
            } else {
                base_name
            }
        } else {
            base_name
        };
        Ok((final_name, None))
    }
}

async fn progress_handler(
    AxumPath(filename): AxumPath<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let map = state
        .upload_progress
        .lock()
        .map_err(|_| AppError::LockError)?;
    if let Some(&progress) = map.get(&filename) {
        return Ok(Json(
            json!({ "progress": progress , "status": "uploading" }),
        ));
    }

    let file_path = state.files_dir.join(&filename);
    if file_path.exists() {
        return Ok(Json(json!({ "progress": 100, "status": "done" })));
    }

    Ok(Json(json!({ "progress": 0, "status": "pending" })))
}

async fn delete_file(
    State(state): State<Arc<AppState>>,
    AxumPath(filename): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let file_path = state.files_dir.join(&filename);

    let canonical_base = dunce::canonicalize(&state.files_dir)?;
    let canonical_full =
        dunce::canonicalize(&file_path).map_err(|_| AppError::NotFound(filename.clone()))?;

    if !canonical_full.starts_with(&canonical_base) {
        return Err(AppError::PathTraversal);
    }

    match fs::remove_file(&file_path).await {
        Ok(_) => {
            let thumb_name = thumb_base_name(&state.files_dir, &filename);
            let thumb_path = state.thumb_dir.join(thumb_name).with_extension(THUMB_EXT);
            let _ = fs::remove_file(thumb_path).await;
            info!("文件删除成功: {}", filename);
            Ok(Json(json!({ "success": true })))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Err(AppError::NotFound(filename)),
        Err(e) => Err(e.into()),
    }
}

async fn thumb_handler(
    AxumPath(filename): AxumPath<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Response, AppError> {
    let thumb_name = thumb_base_name(&state.files_dir, &filename);
    let thumb_path = state.thumb_dir.join(thumb_name).with_extension(THUMB_EXT);

    if !thumb_path.exists() {
        let files_dir = state.files_dir.clone();
        let semaphore = state.thumb_semaphore.clone();
        spawn_thumbnail(files_dir, filename, semaphore);
        return Err(AppError::ThumbnailNotReady);
    }

    // 规范化缩略图文件路径，防御符号链接攻击
    let canonical_thumb = dunce::canonicalize(&thumb_path)
        .map_err(|e| AppError::Internal(format!("无法规范化缩略图路径: {}", e)))?;

    // 检查缩略图是否真正位于 thumb_dir 内（使用缓存的规范根目录）
    if !canonical_thumb.starts_with(&state.thumb_dir) {
        return Err(AppError::PathTraversal);
    }

    // 读取文件并添加缓存头
    let data = fs::read(&canonical_thumb).await?;
    let mime = mime_guess::from_path(&canonical_thumb).first_or_octet_stream();

    // 获取修改时间作为 ETag
    let metadata = fs::metadata(&canonical_thumb).await?;
    let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let etag = format!(
        "\"{:x}-{:x}\"",
        modified
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        data.len()
    );

    let body = Body::from(data);
    Ok(Response::builder()
        .header("Content-Type", mime.as_ref())
        .header("Cache-Control", "public, max-age=86400") // 缓存一天
        .header("ETag", etag)
        .body(body)
        .map_err(|e| AppError::Internal(e.to_string()))?)
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

    // 获取并规范化文件存储目录
    let files_dir_raw = if let Some(dir) = cli.dir {
        dir
    } else {
        get_files_dir()?
    };
    let files_dir = dunce::canonicalize(&files_dir_raw).unwrap_or_else(|_| files_dir_raw.into());

    if !files_dir.exists() {
        fs::create_dir_all(&files_dir).await?;
    }

    // 计算并规范化缩略图目录
    let thumb_dir_raw = thumb_dir(&files_dir);
    let thumb_dir = dunce::canonicalize(&thumb_dir_raw).unwrap_or_else(|_| thumb_dir_raw.into());
    if !thumb_dir.exists() {
        fs::create_dir_all(&thumb_dir).await?;
    }

    info!("📁 文件存储目录: {}", files_dir.display());
    info!("📁 缩略图存储目录: {}", thumb_dir.display());

    let thumb_semaphore = Arc::new(Semaphore::new(4));

    let state = Arc::new(AppState {
        files_dir: files_dir.clone(),
        thumb_dir: thumb_dir.into(),
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
