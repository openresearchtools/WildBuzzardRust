#![forbid(unsafe_code)]

use std::error::Error;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use wild_buzzard_linux::{LinuxBackend, LinuxPresentationShutdown};
use wild_buzzard_shell::{BrowserSmokeConfig, is_completed_smoke_exit, run_browser};

fn main() -> Result<(), Box<dyn Error>> {
    let mut backend = None;
    let mut url = None;
    let mut smoke = false;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--backend" => {
                backend = Some(match arguments.next().as_deref() {
                    Some("wayland") => LinuxBackend::Wayland,
                    Some("x11") => LinuxBackend::X11,
                    other => return Err(format!("invalid --backend value: {other:?}").into()),
                });
            }
            "--url" => {
                url = Some(
                    arguments
                        .next()
                        .ok_or("--url requires one bounded HTTP URL")?
                        .into_boxed_str(),
                );
            }
            "--smoke" => smoke = true,
            "--help" => {
                println!(
                    "wild-buzzard [--backend wayland|x11] [--url http://127.0.0.1:PORT/] [--smoke]"
                );
                return Ok(());
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }

    let mut server = None;
    let smoke_config = if smoke {
        if std::env::var("WILDBUZZARD_REAL_DISPLAY_TEST").as_deref() != Ok("1") {
            return Err("--smoke requires WILDBUZZARD_REAL_DISPLAY_TEST=1".into());
        }
        let local = SmokeServer::start()?;
        let first = format!("http://{}/a", local.address());
        let second = format!("http://{}/b", local.address());
        url = Some(first.into_boxed_str());
        server = Some(local);
        Some(BrowserSmokeConfig {
            second_url: second.into_boxed_str(),
            hard_deadline: Duration::from_secs(20),
        })
    } else {
        None
    };

    let report = run_browser(backend, url, smoke_config)?;
    drop(server);
    println!(
        "Wild Buzzard stopped: reason={:?} compositions={} last_receipt={:?}",
        report.native.reason, report.successful_compositions, report.last_receipt
    );
    if smoke {
        if !is_completed_smoke_exit(report.native.reason, report.smoke_completed) {
            return Err(
                "browser smoke did not reach its terminal hold and exact requested stop".into(),
            );
        }
        if report.successful_compositions < 6 {
            return Err("browser smoke produced too few exact compositions".into());
        }
        let LinuxPresentationShutdown::BrowserWrappersReleased(presentation) =
            report.native.presentation
        else {
            return Err("browser smoke lacked normal browser-owner release evidence".into());
        };
        if presentation.text_font_templates_released() == 0
            || presentation.text_font_instances_released() == 0
            || presentation.text_font_bytes_released() == 0
        {
            return Err("browser smoke did not release its nonempty shaped-text resources".into());
        }
    }
    Ok(())
}

struct SmokeServer {
    address: SocketAddr,
    running: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl SmokeServer {
    fn start() -> Result<Self, Box<dyn Error>> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let running = Arc::new(AtomicBool::new(true));
        let thread_running = Arc::clone(&running);
        let thread = thread::Builder::new()
            .name("wild-buzzard-smoke-http".to_owned())
            .spawn(move || {
                while thread_running.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => serve(&mut stream),
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })?;
        Ok(Self {
            address,
            running,
            thread: Some(thread),
        })
    }

    const fn address(&self) -> SocketAddr {
        self.address
    }
}

impl Drop for SmokeServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(stream: &mut TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut request = [0_u8; 4_096];
    let size = stream.read(&mut request).unwrap_or(0);
    let request = String::from_utf8_lossy(&request[..size]);
    let second = request.starts_with("GET /b ");
    let (title, color) = if second {
        ("Second tab", "#d97706")
    } else {
        ("First tab", "#2563eb")
    };
    let body = format!(
        "<!doctype html><html><head><title>{title}</title><style>body{{margin:0;background:{color};color:white;font:28px sans-serif}}main{{padding:72px}}h1{{font-size:48px}}</style></head><body><main><h1>{title}</h1><p>Wild Buzzard Rust browser compositor smoke.</p></main></body></html>"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}
