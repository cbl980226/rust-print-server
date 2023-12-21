use ipp;

use std::collections::VecDeque;
use std::fmt::Debug;
use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver, SendError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use futures::TryStreamExt;
use uuid::Uuid;

use actix_multipart::Multipart;
use actix_web::error::InternalError;
use actix_web::http::StatusCode;
use actix_web::{web, App, Error, HttpResponse, HttpServer};

trait Print {
    fn print(&self) -> Result<(), std::io::Error>;
}

trait Printable: Print + std::fmt::Debug {}
impl<T: Print + std::fmt::Debug> Printable for T {}

#[derive(Debug)]
struct PrintJob {
    filepath: String,
}

impl PrintJob {
    fn new(filepath: String) -> Self {
        PrintJob { filepath }
    }
}

impl Print for PrintJob {
    fn print(&self) -> Result<(), std::io::Error> {
        println!("打印文件: {}", self.filepath);
        let mut file = std::fs::File::open(self.filepath.as_str())?;
        let mut contents = Vec::new();
        file.read_to_end(&mut contents)?;

        println!("{:?}", contents);

        Ok(())
    }
}

#[derive(Debug)]
struct PrintQueue {
    queue: Arc<Mutex<VecDeque<Box<dyn Printable + Send>>>>,
    sender: Sender<Arc<Mutex<VecDeque<Box<dyn Printable + Send>>>>>,
}

impl PrintQueue {
    fn new() -> PrintQueue {
        let (sender, receiver): (
            Sender<Arc<Mutex<VecDeque<Box<dyn Printable + Send>>>>>,
            Receiver<Arc<Mutex<VecDeque<Box<dyn Printable + Send>>>>>,
        ) = mpsc::channel();

        let queue = Arc::new(Mutex::new(VecDeque::<Box<dyn Printable + Send>>::new()));

        thread::spawn(move || handle_print_jobs(receiver));

        PrintQueue { queue, sender }
    }

    fn add_job(
        &self,
        job: Box<dyn Printable + Send>,
    ) -> Result<(), SendError<Arc<Mutex<VecDeque<Box<dyn Printable + Send>>>>>> {
        let mut queue = self.queue.lock().unwrap();
        queue.push_back(job);
        self.sender.send(self.queue.clone())
    }
}

// 处理打印任务
fn handle_print_jobs(receiver: Receiver<Arc<Mutex<VecDeque<Box<dyn Printable + Send>>>>>) {
    while let Ok(queue) = receiver.recv() {
        let mut queue = queue.lock().unwrap();
        if let Some(job) = queue.pop_front() {
            match job.print() {
                Ok(_) => println!("打印成功"),
                Err(e) => println!("打印失败: {}", e),
            }
        }
    }
}

async fn upload_file(
    mut payload: Multipart,
    print_queue: web::Data<PrintQueue>,
) -> Result<HttpResponse, Error> {
    while let Some(field) = payload.try_next().await? {
        // A multipart/form-data stream has to contain `content_disposition`
        let content_disposition = field.content_disposition();
        let filename = content_disposition
            .get_filename()
            .map_or_else(|| Uuid::new_v4().to_string(), sanitize_filename::sanitize);
        let filepath = format!("./tmp/{filename}");

        let saved_filepath = save_file(field, filepath).await?;
        match print_queue.add_job(Box::new(PrintJob::new(saved_filepath))) {
            Ok(_) => {}
            Err(e) => {
                println!("打印任务添加失败: {}", e);
            }
        };
    }

    Ok(HttpResponse::Ok().body("文件上传成功"))
}

async fn save_file(mut field: actix_multipart::Field, filepath: String) -> Result<String, Error> {
    let filepath_clone = filepath.clone();

    let mut file = web::block(|| std::fs::File::create(filepath_clone))
        .await
        .map_err(|e| {
            eprintln!("文件创建失败: {}", e);
            InternalError::new("文件创建失败", StatusCode::INTERNAL_SERVER_ERROR)
        })??;

    while let Some(chunk) = field.try_next().await? {
        file = web::block(move || file.write_all(&chunk).map(|_| file))
            .await
            .map_err(|e| {
                eprintln!("文件写入失败: {}", e);
                InternalError::new("文件写入失败", StatusCode::INTERNAL_SERVER_ERROR)
            })??;
    }

    Ok(filepath)
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));

    log::info!("creating temporary upload directory");
    std::fs::create_dir_all("./tmp")?;

    log::info!("starting HTTP server at http://localhost:8080");

    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(PrintQueue::new()))
            .route("/upload", web::post().to(upload_file))
    })
    .bind(("127.0.0.1", 8080))?
    .workers(2)
    .run()
    .await
}
