use axum::{
    Router,
    extract::{Multipart, State},
    http::StatusCode,
    response::Html,
    routing::{get, post},
};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs;
use tower_http::services::ServeDir;

struct AppState {
    files_dir: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. 创建文件目录
    let files_dir = std::env::current_dir()?.join("files");
    if !files_dir.exists() {
        fs::create_dir_all(&files_dir).await?;
    }

    let state = Arc::new(AppState {
        files_dir: files_dir.clone(),
    });

    // 2. 获取本机 IP
    let ip = local_ip_address::local_ip()?;
    let port = 3000;
    let addr = format!("{}:{}", ip, port);
    let url = format!("http://{}", addr);

    // 3. 打印二维码
    if let Err(e) = qr2term::print_qr(&url) {
        eprintln!("打印二维码失败: {}", e);
        // 即使打印失败，仍然继续启动，只输出文本地址
    }
    println!("\n访问地址: {}", url);
    println!("按 Ctrl+C 停止服务\n");

    // 4. 构建路由
    let app = Router::new()
        .route("/", get(index))
        .route("/upload", post(upload))
        .nest_service("/files", ServeDir::new(files_dir))
        .with_state(state);

    // 5. 启动服务
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

// 主页处理器：返回包含上传和文件列表的 HTML
async fn index(State(state): State<Arc<AppState>>) -> Html<String> {
    let files_dir = &state.files_dir;
    let mut file_list_html = String::new();

    // 读取目录并生成文件列表
    if let Ok(mut entries) = fs::read_dir(files_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            file_list_html.push_str(&format!(
                "<li><a href='/files/{}' download>{}</a></li>",
                name, name
            ));
        }
    }

    let html = format!(
        r#"
<!DOCTYPE html>
<html>
<head><meta charset="utf-8"><title>RushDrop</title></head>
<body>
    <h2>上传文件</h2>
    <form action="/upload" method="post" enctype="multipart/form-data">
        <input type="file" name="file" multiple>
        <button type="submit">上传</button>
    </form>
    <h2>电脑上的文件</h2>
    <ul>{}</ul>
</body>
</html>
"#,
        file_list_html
    );

    Html(html)
}

// 上传处理器
async fn upload(
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> Result<Html<String>, (StatusCode, String)> {
    let save_dir = &state.files_dir;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("读取上传字段出错: {}", e)))?
    {
        let file_name = field.file_name().unwrap_or("unknown").to_string();
        let data = field.bytes().await.map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("读取文件数据出错: {}", e),
            )
        })?;

        let file_path = save_dir.join(&file_name);
        fs::write(&file_path, &data).await.map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("写入文件失败: {}", e),
            )
        })?;
    }

    Ok(Html("<p>上传成功，<a href='/'>返回</a></p>".to_string()))
}
