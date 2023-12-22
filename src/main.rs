use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt::Debug;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use futures::TryStreamExt;
use printers;
use tokio::sync::{Mutex, Notify};
use uuid::Uuid;

use actix_multipart::Multipart;
use actix_web::error::InternalError;
use actix_web::http::StatusCode;
use actix_web::{web, App, Error, HttpResponse, HttpServer};

static WORKER_COUNTER: AtomicUsize = AtomicUsize::new(1);

thread_local! {
    static WORKER_ID: RefCell<usize> = RefCell::new(0);
}

trait Print {
    fn print(&self) -> Result<(), Box<dyn std::error::Error>>;
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
    fn print(&self) -> Result<(), Box<dyn std::error::Error>> {
        println!("打印文件: {}", self.filepath);
        let mut file = std::fs::File::open(self.filepath.as_str())?;
        let mut contents = Vec::new();
        file.read_to_end(&mut contents)?;

        // TODO: select printer from ui
        let pdf_printer = printers::get_printer_by_name("Microsoft XPS Document Writer")
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "打印机未找到"))?;

        pdf_printer
            .print(contents.as_slice(), None)
            .map(|b| {
                println!("打印结果: {:?}", b);
            })
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e).into())
    }
}

#[derive(Debug)]
struct PrintQueue {
    queue: Mutex<VecDeque<Box<dyn Printable + Send>>>,
    notify: Notify,
    terminate: AtomicBool,
}

impl PrintQueue {
    fn new() -> Arc<Self> {
        let pq = Arc::new(PrintQueue {
            queue: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
            terminate: AtomicBool::new(false),
        });
        let pq_clone = pq.clone();

        tokio::spawn(async move {
            handle_print_jobs(pq_clone).await;
        });

        pq
    }

    async fn add_job(
        &self,
        job: Box<dyn Printable + Send>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut queue = self.queue.lock().await;
        queue.push_back(job);
        self.start();
        Ok(())
    }

    fn start(&self) {
        self.terminate.store(false, Ordering::Release);
        self.notify.notify_one();
    }
}

// 处理打印任务
async fn handle_print_jobs(pq: Arc<PrintQueue>) {
    let worker_id = WORKER_ID.with(|id| *id.borrow());
    loop {
        println!("---- 等待打印任务 {} ----", worker_id);
        if pq.terminate.load(Ordering::Acquire) {
            println!("---- 收到终止信号 {} ----", worker_id);
            break; // 如果收到终止信号，退出循环
        }

        let mut queue = pq.queue.lock().await;

        println!("---- 检查打印队列 {} ----", worker_id);
        println!("---- 当前打印队列 {:?} ----\n", queue);

        if let Some(job) = queue.pop_front() {
            drop(queue); // 释放锁，以便在打印时添加新任务
            match job.print() {
                Ok(_) => println!("打印成功"),
                Err(e) => println!("打印失败: {}", e),
            }
        } else {
            drop(queue); // 在等待之前释放锁
            pq.terminate.store(true, Ordering::Release);
            pq.notify.notified().await;
            if pq.terminate.load(Ordering::Acquire) {
                break; // 再次检查终止信号，以防在等待时收到
            }
        }
    }
}

async fn upload_file(
    mut payload: Multipart,
    print_queue: web::Data<Arc<PrintQueue>>,
) -> Result<HttpResponse, Error> {
    while let Some(field) = payload.try_next().await? {
        // A multipart/form-data stream has to contain `content_disposition`
        let content_disposition = field.content_disposition();
        let filename = content_disposition
            .get_filename()
            .map_or_else(|| Uuid::new_v4().to_string(), sanitize_filename::sanitize);
        let filepath = format!("./tmp/{filename}");

        let saved_filepath = save_file(field, filepath).await?;
        match print_queue
            .add_job(Box::new(PrintJob::new(saved_filepath)))
            .await
        {
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
        WORKER_ID.with(|worker_id| {
            *worker_id.borrow_mut() = WORKER_COUNTER.fetch_add(1, Ordering::SeqCst);
        });

        App::new()
            .app_data(web::Data::new(PrintQueue::new()))
            .route("/upload", web::post().to(upload_file))
    })
    .bind(("127.0.0.1", 8080))?
    .workers(2)
    .run()
    .await
}
