#![forbid(unsafe_code)]

use std::error::Error;
#[cfg(feature = "webdriver")]
use std::fs::File;
#[cfg(feature = "webdriver")]
use std::io;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
#[cfg(feature = "webdriver")]
use std::os::fd::OwnedFd;
#[cfg(feature = "webdriver")]
use std::os::unix::fs::MetadataExt;
#[cfg(feature = "webdriver")]
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

#[cfg(feature = "webdriver")]
use rustix::fs::{FileType, Mode, OFlags};
#[cfg(feature = "webdriver")]
use webdriver::server::{BearerToken, SecretBytes, ServerSecurityPolicy};
use wild_buzzard_linux::{LinuxBackend, LinuxPresentationShutdown};
#[cfg(feature = "contained_inline_classic")]
use wild_buzzard_shell::run_browser_contained_inline_classic;
#[cfg(all(feature = "webdriver", feature = "contained_inline_classic"))]
use wild_buzzard_shell::run_browser_contained_inline_classic_with_webdriver;
use wild_buzzard_shell::{
    BrowserRunReport, BrowserSmokeConfig, is_completed_smoke_exit, run_browser,
};
#[cfg(feature = "webdriver")]
use wild_buzzard_shell::{BrowserWebDriverConfig, run_browser_with_webdriver};

#[cfg(feature = "webdriver")]
enum TokenSource {
    File(PathBuf),
    FileDescriptor(u32),
}

struct BrowserArguments {
    backend: Option<LinuxBackend>,
    url: Option<Box<str>>,
    smoke: bool,
    contained_inline_classic: bool,
    #[cfg(feature = "webdriver")]
    webdriver_address: Option<SocketAddr>,
    #[cfg(feature = "webdriver")]
    webdriver_token_source: Option<TokenSource>,
}

fn parse_arguments() -> Result<Option<BrowserArguments>, Box<dyn Error>> {
    let mut backend = None;
    let mut url = None;
    let mut smoke = false;
    #[cfg(feature = "contained_inline_classic")]
    let mut contained_inline_classic = false;
    #[cfg(not(feature = "contained_inline_classic"))]
    let contained_inline_classic = false;
    #[cfg(feature = "webdriver")]
    let mut webdriver_address = None;
    #[cfg(feature = "webdriver")]
    let mut webdriver_token_source = None;
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
                        .ok_or("--url requires one bounded HTTP or HTTPS URL")?
                        .into_boxed_str(),
                );
            }
            "--smoke" => smoke = true,
            #[cfg(feature = "contained_inline_classic")]
            "--contained-inline-classic" => contained_inline_classic = true,
            #[cfg(feature = "webdriver")]
            "--webdriver-loopback-address" => {
                let value = arguments
                    .next()
                    .ok_or("--webdriver-loopback-address requires an explicit IP:PORT value")?;
                let address: SocketAddr = value.parse()?;
                if !address.ip().is_loopback() || address.port() == 0 {
                    return Err(
                        "--webdriver-loopback-address requires a nonzero loopback IP:PORT".into(),
                    );
                }
                webdriver_address = Some(address);
            }
            #[cfg(feature = "webdriver")]
            "--webdriver-token-fd" => {
                if webdriver_token_source.is_some() {
                    return Err("configure exactly one WebDriver token source".into());
                }
                let fd = arguments
                    .next()
                    .ok_or("--webdriver-token-fd requires a decimal Linux file descriptor")?
                    .parse::<u32>()?;
                webdriver_token_source = Some(TokenSource::FileDescriptor(fd));
            }
            #[cfg(feature = "webdriver")]
            "--webdriver-token-file" => {
                if webdriver_token_source.is_some() {
                    return Err("configure exactly one WebDriver token source".into());
                }
                let path = arguments
                    .next()
                    .ok_or("--webdriver-token-file requires one owner-only regular file")?;
                webdriver_token_source = Some(TokenSource::File(path.into()));
            }
            "--help" => {
                let contained_help = if cfg!(feature = "contained_inline_classic") {
                    " [--contained-inline-classic]"
                } else {
                    ""
                };
                #[cfg(feature = "webdriver")]
                println!(
                    "wild-buzzard [--backend wayland|x11] [--url URL] [--smoke] \
                     {contained_help} \
                     [--webdriver-loopback-address IP:PORT \
                     (--webdriver-token-fd FD|--webdriver-token-file PATH)]"
                );
                #[cfg(not(feature = "webdriver"))]
                println!(
                    "wild-buzzard [--backend wayland|x11] [--url http://HOST/|https://HOST/] \
                     [--smoke]{contained_help}"
                );
                return Ok(None);
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }

    Ok(Some(BrowserArguments {
        backend,
        url,
        smoke,
        contained_inline_classic,
        #[cfg(feature = "webdriver")]
        webdriver_address,
        #[cfg(feature = "webdriver")]
        webdriver_token_source,
    }))
}

fn main() -> Result<(), Box<dyn Error>> {
    let Some(arguments) = parse_arguments()? else {
        return Ok(());
    };
    let BrowserArguments {
        backend,
        mut url,
        smoke,
        contained_inline_classic,
        #[cfg(feature = "webdriver")]
        webdriver_address,
        #[cfg(feature = "webdriver")]
        webdriver_token_source,
    } = arguments;
    #[cfg(not(feature = "contained_inline_classic"))]
    debug_assert!(!contained_inline_classic);

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

    #[cfg(feature = "webdriver")]
    let report = match (webdriver_address, webdriver_token_source) {
        (Some(address), Some(source)) => {
            let token = read_bearer_token(source)?;
            let policy = ServerSecurityPolicy::new(address, token)?;
            let webdriver = BrowserWebDriverConfig::new(policy)?;
            #[cfg(feature = "contained_inline_classic")]
            if contained_inline_classic {
                run_browser_contained_inline_classic_with_webdriver(
                    backend,
                    url,
                    smoke_config,
                    webdriver,
                )?
            } else {
                run_browser_with_webdriver(backend, url, smoke_config, webdriver)?
            }
            #[cfg(not(feature = "contained_inline_classic"))]
            run_browser_with_webdriver(backend, url, smoke_config, webdriver)?
        }
        (Some(_), None) => {
            return Err("WebDriver listener requires a token FD or owner-only token file".into());
        }
        (None, Some(_)) => {
            return Err("WebDriver token source requires an explicit loopback listener".into());
        }
        (None, None) => {
            #[cfg(feature = "contained_inline_classic")]
            if contained_inline_classic {
                run_browser_contained_inline_classic(backend, url, smoke_config)?
            } else {
                run_browser(backend, url, smoke_config)?
            }
            #[cfg(not(feature = "contained_inline_classic"))]
            run_browser(backend, url, smoke_config)?
        }
    };
    #[cfg(not(feature = "webdriver"))]
    let report = {
        #[cfg(feature = "contained_inline_classic")]
        if contained_inline_classic {
            run_browser_contained_inline_classic(backend, url, smoke_config)?
        } else {
            run_browser(backend, url, smoke_config)?
        }
        #[cfg(not(feature = "contained_inline_classic"))]
        run_browser(backend, url, smoke_config)?
    };
    drop(server);
    validate_run_report(&report, smoke)
}

fn validate_run_report(report: &BrowserRunReport, smoke: bool) -> Result<(), Box<dyn Error>> {
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

#[cfg(feature = "webdriver")]
fn read_bearer_token(source: TokenSource) -> Result<BearerToken, Box<dyn Error>> {
    let opened = open_token_source(source)?;
    let identity = validate_owner_only_file(&opened, process_uid()?)?;
    read_token_bytes(File::from(opened), identity)
}

#[cfg(feature = "webdriver")]
fn open_token_source(source: TokenSource) -> Result<OwnedFd, Box<dyn Error>> {
    let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK;
    Ok(match source {
        TokenSource::FileDescriptor(fd) => {
            let path = PathBuf::from(format!("/proc/self/fd/{fd}"));
            rustix::fs::open(path, flags, Mode::empty())?
        }
        TokenSource::File(path) => rustix::fs::open(path, flags | OFlags::NOFOLLOW, Mode::empty())?,
    })
}

#[cfg(feature = "webdriver")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TokenFileIdentity {
    device: u64,
    inode: u64,
    owner: u64,
    mode: u64,
    size: usize,
}

#[cfg(feature = "webdriver")]
fn process_uid() -> Result<u64, Box<dyn Error>> {
    Ok(u64::from(std::fs::metadata("/proc/self")?.uid()))
}

#[cfg(feature = "webdriver")]
fn validate_owner_only_file(
    opened: &impl std::os::fd::AsFd,
    expected_uid: u64,
) -> Result<TokenFileIdentity, Box<dyn Error>> {
    let stat = rustix::fs::fstat(opened)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err("WebDriver token source must be a regular file or regular memfd".into());
    }
    let owner = u64::from(stat.st_uid);
    let mode = u64::from(stat.st_mode);
    if owner != expected_uid || mode & 0o077 != 0 {
        return Err(
            "WebDriver token file must be owned by this user with no group/other access".into(),
        );
    }
    let size = usize::try_from(stat.st_size).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "WebDriver token source has an invalid negative size",
        )
    })?;
    if !matches!(size, 64..=66) {
        return Err("WebDriver token source must contain exactly one bounded token".into());
    }
    Ok(TokenFileIdentity {
        device: stat.st_dev,
        inode: stat.st_ino,
        owner,
        mode,
        size,
    })
}

#[cfg(feature = "webdriver")]
fn read_token_bytes(
    mut reader: File,
    identity: TokenFileIdentity,
) -> Result<BearerToken, Box<dyn Error>> {
    let mut bytes = SecretBytes::<67>::zeroed();
    reader.read_exact(&mut bytes.as_mut_slice()[..identity.size])?;
    if reader.read(&mut bytes.as_mut_slice()[identity.size..])? != 0 {
        return Err("WebDriver token source grew beyond its validated bound".into());
    }
    if validate_owner_only_file(&reader, identity.owner)? != identity {
        return Err("WebDriver token source identity changed while reading".into());
    }
    let token_len = if bytes.as_slice()[..identity.size].ends_with(b"\r\n") {
        identity.size - 2
    } else if bytes.as_slice()[..identity.size].ends_with(b"\n") {
        identity.size - 1
    } else {
        identity.size
    };
    BearerToken::from_lower_hex(&bytes.as_slice()[..token_len]).map_err(Into::into)
}

#[cfg(all(test, feature = "webdriver"))]
mod webdriver_token_tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    use std::time::Instant;

    const TEST_TOKEN: &[u8; 64] =
        b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = NEXT_TEMP.fetch_add(1, AtomicOrdering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "wild-buzzard-w9-a6n-token-{}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }

        fn write(&self, name: &str, bytes: &[u8], mode: u32) -> PathBuf {
            let path = self.path(name);
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(mode)
                .open(&path)
                .unwrap();
            file.write_all(bytes).unwrap();
            file.flush().unwrap();
            path
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn assert_rejected_within(source: TokenSource) {
        let started = Instant::now();
        let Err(error) = read_bearer_token(source) else {
            panic!("invalid token source was accepted");
        };
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(!format!("{error:?}").contains(std::str::from_utf8(TEST_TOKEN).unwrap()));
    }

    #[test]
    fn valid_regular_path_and_fd_are_nonblocking_cloexec_and_redacted() {
        let directory = TestDirectory::new();
        let path = directory.write("token", TEST_TOKEN, 0o600);
        let opened = open_token_source(TokenSource::File(path.clone())).unwrap();
        assert!(
            rustix::io::fcntl_getfd(&opened)
                .unwrap()
                .contains(rustix::io::FdFlags::CLOEXEC)
        );
        assert!(
            rustix::fs::fcntl_getfl(&opened)
                .unwrap()
                .contains(OFlags::NONBLOCK)
        );
        drop(opened);

        let token = read_bearer_token(TokenSource::File(path.clone())).unwrap();
        assert!(!format!("{token:?}").contains(std::str::from_utf8(TEST_TOKEN).unwrap()));
        drop(token);

        let mut newline = TEST_TOKEN.to_vec();
        newline.push(b'\n');
        let newline_path = directory.write("token-newline", &newline, 0o600);
        let file = File::open(newline_path).unwrap();
        let fd = u32::try_from(file.as_raw_fd()).unwrap();
        let token = read_bearer_token(TokenSource::FileDescriptor(fd)).unwrap();
        assert!(!format!("{token:?}").contains(std::str::from_utf8(TEST_TOKEN).unwrap()));
    }

    #[test]
    fn path_symlink_fifo_socket_device_mode_owner_and_size_fail_bounded() {
        let directory = TestDirectory::new();
        let valid = directory.write("valid", TEST_TOKEN, 0o600);
        let link = directory.path("link");
        symlink(&valid, &link).unwrap();
        assert_rejected_within(TokenSource::File(link));

        let fifo = directory.path("fifo");
        rustix::fs::mkfifoat(rustix::fs::CWD, &fifo, Mode::from_raw_mode(0o600)).unwrap();
        assert_rejected_within(TokenSource::File(fifo));

        let socket = directory.path("socket");
        let _listener = UnixListener::bind(&socket).unwrap();
        assert_rejected_within(TokenSource::File(socket));
        assert_rejected_within(TokenSource::File(PathBuf::from("/dev/null")));

        let wrong_mode = directory.write("wrong-mode", TEST_TOKEN, 0o600);
        std::fs::set_permissions(&wrong_mode, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert_rejected_within(TokenSource::File(wrong_mode));

        let opened = open_token_source(TokenSource::File(valid)).unwrap();
        assert!(validate_owner_only_file(&opened, process_uid().unwrap() + 1).is_err());

        let short = directory.write("short", &TEST_TOKEN[..63], 0o600);
        assert_rejected_within(TokenSource::File(short));
        let mut oversized = TEST_TOKEN.to_vec();
        oversized.extend_from_slice(b"xxx");
        let oversized = directory.write("oversized", &oversized, 0o600);
        assert_rejected_within(TokenSource::File(oversized));
    }

    #[test]
    fn fd_fifo_socket_device_mode_and_size_fail_bounded_without_consuming_source() {
        let directory = TestDirectory::new();
        let fifo = directory.path("fd-fifo");
        rustix::fs::mkfifoat(rustix::fs::CWD, &fifo, Mode::from_raw_mode(0o600)).unwrap();
        let fifo = rustix::fs::open(
            &fifo,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        assert_rejected_within(TokenSource::FileDescriptor(
            u32::try_from(fifo.as_raw_fd()).unwrap(),
        ));
        assert!(rustix::fs::fstat(&fifo).is_ok());

        let (socket, _peer) = UnixStream::pair().unwrap();
        assert_rejected_within(TokenSource::FileDescriptor(
            u32::try_from(socket.as_raw_fd()).unwrap(),
        ));
        let device = File::open("/dev/null").unwrap();
        assert_rejected_within(TokenSource::FileDescriptor(
            u32::try_from(device.as_raw_fd()).unwrap(),
        ));

        let wrong_mode = directory.write("fd-mode", TEST_TOKEN, 0o640);
        let wrong_mode = File::open(wrong_mode).unwrap();
        assert_rejected_within(TokenSource::FileDescriptor(
            u32::try_from(wrong_mode.as_raw_fd()).unwrap(),
        ));
        let short = directory.write("fd-short", &TEST_TOKEN[..63], 0o600);
        let short = File::open(short).unwrap();
        assert_rejected_within(TokenSource::FileDescriptor(
            u32::try_from(short.as_raw_fd()).unwrap(),
        ));
        let mut oversized = TEST_TOKEN.to_vec();
        oversized.extend_from_slice(b"xxx");
        let oversized = directory.write("fd-oversized", &oversized, 0o600);
        let oversized = File::open(oversized).unwrap();
        assert_rejected_within(TokenSource::FileDescriptor(
            u32::try_from(oversized.as_raw_fd()).unwrap(),
        ));
    }

    #[test]
    fn concurrent_path_replacement_never_traverses_symlink_or_blocks() {
        let directory = TestDirectory::new();
        let target = directory.write("target", TEST_TOKEN, 0o600);
        let live = directory.path("live");
        std::fs::hard_link(&target, &live).unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let attacker_running = Arc::clone(&running);
        let attacker_target = target.clone();
        let attacker_live = live.clone();
        let attacker = thread::spawn(move || {
            while attacker_running.load(Ordering::Acquire) {
                let _ = std::fs::remove_file(&attacker_live);
                let _ = symlink(&attacker_target, &attacker_live);
                let _ = std::fs::remove_file(&attacker_live);
                let _ = std::fs::hard_link(&attacker_target, &attacker_live);
            }
        });
        let started = Instant::now();
        for _ in 0..256 {
            if let Ok(token) = read_bearer_token(TokenSource::File(live.clone())) {
                assert!(!format!("{token:?}").contains(std::str::from_utf8(TEST_TOKEN).unwrap()));
            }
        }
        running.store(false, Ordering::Release);
        attacker.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
    }
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
