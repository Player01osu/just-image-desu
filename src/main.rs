use axum::{
    extract::{multipart, Multipart},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, get_service, post},
    Json, Router,
};
use axum_extra::routing::SpaRouter;
use futures::{Stream, TryStreamExt};
use std::{collections::VecDeque, thread};
use std::{io, net::SocketAddr, sync::Arc};
use tokio::{
    fs::{remove_file, write, File, OpenOptions},
    io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter},
    join,
    sync::{Mutex, MutexGuard},
};
use tokio_util::io::StreamReader;
use tracing_subscriber::{prelude::__tracing_subscriber_SubscriberExt, util::SubscriberInitExt};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tower_http::{
    services::{ServeDir, ServeFile},
    trace::TraceLayer,
};

const HTML_HEAD: &str = r#"<!DOCTYPE html>
<html lang="en">
	<head>
		<title></title>
		<meta charset="UTF-8">
		<meta name="viewport" content="width=device-width, initial-scale=1">
		<link href="css/style.css" rel="stylesheet">
	</head>
	<body>
		<div style="display: flex; flex-direction: column; max-width: 30vw";>
"#;

async fn create_user(
    // this argument tells axum to parse the request body
    // as JSON into a `CreateUser` type
    Json(payload): Json<CreateUser>,
) -> impl IntoResponse {
    // insert your application logic here
    let user = User {
        id: 1337,
        username: payload.username,
    };

    // this will be converted into a JSON response
    // with a status code of `201 Created`
    (StatusCode::CREATED, Json(user))
}

// the input to our `create_user` handler
#[derive(Deserialize)]
struct CreateUser {
    username: String,
}

// the output to our `create_user` handler
#[derive(Serialize)]
struct User {
    id: u64,
    username: String,
}

async fn handle_error(_err: io::Error) -> impl IntoResponse {
    (StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong...")
}

async fn generate_html(mut buffer: UploadQueue) -> Result<impl IntoResponse, AppError> {
    let mut buf_write = BufWriter::new(HTML_HEAD.as_bytes().to_vec());

    let b = buf_write.get_mut();
    b.push(b'\t');
    b.push(b'\t');
    b.push(b'\t');
    b.append(&mut buffer.buf);

    let mut f = OpenOptions::new()
        .write(true)
        .read(true)
        .truncate(false)
        .create(true)
        .open("./assets/static/bruh.html")
        .await?;

    // Create new file to 'buffer'
    // Wait for unspecified amount of time
    // Join all buffers and write accordingly
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).await?;

    let mut lines = buf.split_inclusive(|c| *c == b'\n').enumerate();

    let mut end_buf = vec![];
    while let Some((idx, line)) = lines.next() {
        if idx > 9 {
            end_buf.append(&mut line.to_vec());
        }
    }
    remove_file("./assets/static/bruh.html").await?;
    b.push(b'\n');
    b.append(&mut end_buf);

    let mut f = OpenOptions::new()
        .write(true)
        .read(true)
        .truncate(false)
        .create(true)
        .open("./assets/static/bruh.html")
        .await?;
    f.sync_all().await?;
    f.write_all(b).await?;

    Ok(StatusCode::CREATED)
}

#[derive(Debug)]
struct AppError(anyhow::Error);

// Tell axum how to convert `AppError` into a response.
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Something went wrong: {}", self.0),
        )
            .into_response()
    }
}

// This enables using `?` on functions that return `Result<_, anyhow::Error>` to turn them into
// `Result<_, AppError>`. That way you don't need to do that manually.
impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

#[derive(Debug)]
pub struct UploadQueue {
    buf: Vec<u8>,
    packets: Vec<u8>,
}

async fn post_media(
    mut multipart: Multipart,
    state: Arc<Mutex<VecDeque<UploadQueue>>>,
) -> Result<impl IntoResponse, AppError> {
    // Obtain lock to queue.
    let mut state = state.lock().await;

    // Write media and get media name from multipart.
    let mut file_name = String::new();
    while let Some(field) = multipart.next_field().await? {
        match field.name().unwrap() {
            "media" => {
                file_name = field.file_name().unwrap().to_owned();
                let field = field.map_err(|err| io::Error::new(io::ErrorKind::Other, err));

                let mut body_reader = StreamReader::new(field);

                let path = std::path::Path::new("./assets/media").join(&file_name);
                let mut file = BufWriter::new(File::create(&path).await?);

                tokio::io::copy(&mut body_reader, &mut file).await?;
            }
            _ => (),
        }
    }

    let buf = format!(r#"<img src="media/{}" loading="lazy">"#, file_name)
        .as_bytes()
        .to_vec();

    state.push_back(UploadQueue {
        buf,
        packets: vec![2],
    });

    Ok(StatusCode::OK)
}

#[tokio::main]
async fn main() -> Result<(), AppError> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "example_static_file_server=debug,tower_http=debug".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let media_queue = Arc::new(Mutex::new(VecDeque::new()));

    let app = Router::new()
        .route(
            "/",
            get_service(ServeFile::new("assets/static/bruh.html")).handle_error(handle_error),
        )
        .route(
            "/home",
            get_service(ServeFile::new("assets/static/home.html")).handle_error(handle_error),
        )
        .route(
            "/media",
            post({
                let state = Arc::clone(&media_queue);
                move |multipart| post_media(multipart, Arc::clone(&state))
            }),
        )
        .route("/users", post(create_user))
        .merge(SpaRouter::new("/media", "assets/media"))
        .fallback(
            get_service(ServeFile::new("assets/static/not_found.html")).handle_error(handle_error),
        )
        .layer(TraceLayer::new_for_http());

    let addr = SocketAddr::from(([127, 0, 0, 1], 8080));

    use tokio::task;
    let queue = Arc::clone(&media_queue);
    let queue_pp = task::spawn(async move {
        loop {
            let mut queue = queue.lock().await;

            if queue.is_empty() {
                task::yield_now().await;
                continue;
            }

            generate_html(queue.pop_back().unwrap()).await.unwrap();
        }
    });

    join!(
        queue_pp,
        axum::Server::bind(&addr).serve(app.into_make_service()),
    )
    .1?;

    Ok(())
}
