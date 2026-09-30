//! Uçtan uca doğrulama: **gerçek `quine` ikilisi** ayağa kalkıyor mu?
//!
//! Bu test, kullanıcıya verilen sözü ölçer: "tek komutla panel açılır".
//! Kütüphane içinden değil, doğrudan derlenmiş ikiliyi çalıştırıp HTTP
//! uçlarına vurur; böylece CLI argümanları, port bağlama, statik varlıklar ve
//! canlı akış gerçek kod yollarıyla sınanır.

use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Platforma göre "boş aygıt" (çıktıyı yok say).
#[cfg(windows)]
const NULL_DEVICE: &str = "NUL";
#[cfg(not(windows))]
const NULL_DEVICE: &str = "/dev/null";

/// İşletim sisteminden boş bir port alır (sabit port çakışması olmasın).
fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("boş port");
    l.local_addr().unwrap().port()
}

fn get(url: &str) -> Option<(u16, String)> {
    let out = Command::new("curl")
        .args(["-s", "-o", "-", "-w", "\\n%{http_code}", url])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let mut lines: Vec<&str> = text.rsplitn(2, '\n').collect();
    let code = lines.first()?.trim().parse::<u16>().ok()?;
    lines.reverse();
    Some((code, lines.first().copied().unwrap_or("").to_string()))
}

fn post(url: &str, body: &str) -> Option<(u16, String)> {
    let out = Command::new("curl")
        .args([
            "-s",
            "-o",
            "-",
            "-w",
            "\\n%{http_code}",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-d",
            body,
            url,
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let mut lines: Vec<&str> = text.rsplitn(2, '\n').collect();
    let code = lines.first()?.trim().parse::<u16>().ok()?;
    lines.reverse();
    Some((code, lines.first().copied().unwrap_or("").to_string()))
}

/// Sunucuyu başlatır ve hazır olana kadar bekler.
struct Server {
    child: Child,
    base: String,
}

impl Server {
    fn start(extra: &[&str]) -> Option<Server> {
        // curl yoksa test atlanır (CI imajında mevcut; yerelde de genelde var).
        if Command::new("curl").arg("--version").output().is_err() {
            return None;
        }
        let port = free_port();
        let dir = std::env::temp_dir().join(format!("quine-serve-test-{port}"));
        std::fs::create_dir_all(&dir).ok()?;

        let port_s = port.to_string();
        let mut args = vec!["serve", "--port", port_s.as_str(), "--no-open"];
        args.extend_from_slice(extra);
        let child = Command::new(env!("CARGO_BIN_EXE_quine"))
            .args(&args)
            .current_dir(&dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        let base = format!("http://127.0.0.1:{port}");
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if let Some((200, _)) = get(&format!("{base}/health")) {
                return Some(Server { child, base });
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        None
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn serve_boots_and_answers_health() {
    let Some(s) = Server::start(&[]) else {
        eprintln!("atlandı: sunucu başlatılamadı veya curl yok");
        return;
    };
    let (code, body) = get(&format!("{}/health", s.base)).unwrap();
    assert_eq!(code, 200);
    assert!(body.contains("\"status\":\"ok\""), "gövde: {body}");
}

#[test]
fn serve_exposes_problems_and_security_headers() {
    let Some(s) = Server::start(&[]) else { return };
    let (code, body) = get(&format!("{}/api/problems", s.base)).unwrap();
    assert_eq!(code, 200);
    assert!(body.contains("fib-001"), "problemler gelmeli: {body}");

    // UI gerçekten servis ediliyor mu (gömülü varlıklar)?
    let (code, html) = get(&format!("{}/", s.base)).unwrap();
    assert_eq!(code, 200);
    assert!(html.contains("Quine"), "panel HTML'i gelmeli");

    // Güvenlik başlıkları HTTP yanıtında olmalı.
    let headers = Command::new("curl")
        .args(["-s", "-D", "-", "-o", NULL_DEVICE, &format!("{}/", s.base)])
        .output()
        .unwrap();
    let h = String::from_utf8_lossy(&headers.stdout).to_lowercase();
    assert!(h.contains("x-frame-options: deny"), "başlıklar: {h}");
    assert!(h.contains("content-security-policy"), "başlıklar: {h}");
    assert!(
        h.contains("x-content-type-options: nosniff"),
        "başlıklar: {h}"
    );
}

#[test]
fn demo_run_completes_end_to_end() {
    let Some(s) = Server::start(&["--demo"]) else {
        return;
    };
    // Demo modu LLM ve Docker gerektirmez; evrim döngüsü ilk denemede düzeltir.
    let (code, body) = post(
        &format!("{}/api/runs", s.base),
        r#"{"problem_id":"fib-001","mode":"evolve"}"#,
    )
    .unwrap();
    assert_eq!(code, 200, "başlatma gövdesi: {body}");
    let run_id = body
        .split("\"run_id\":\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("run_id dönmeli")
        .to_string();

    let deadline = Instant::now() + Duration::from_secs(90);
    let mut last = String::new();
    while Instant::now() < deadline {
        if let Some((200, b)) = get(&format!("{}/api/runs/{run_id}", s.base)) {
            last = b.clone();
            if b.contains("\"status\":\"completed\"") {
                assert!(b.contains("\"production_success\":true"), "gövde: {b}");
                assert!(b.contains("\"best_score\":100.0"), "gövde: {b}");
                return;
            }
            if b.contains("\"status\":\"failed\"") {
                panic!("demo run başarısız oldu: {b}");
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    panic!("demo run zaman aşımına uğradı; son durum: {last}");
}

#[test]
fn invalid_inputs_are_rejected() {
    let Some(s) = Server::start(&[]) else { return };
    for (body, why) in [
        (
            r#"{"problem_id":"fib-001","sandbox":"banana"}"#,
            "geçersiz sandbox",
        ),
        (r#"{"problem_id":"fib-001","mode":"nope"}"#, "geçersiz mod"),
        (
            r#"{"problem_id":"yok-boyle-problem"}"#,
            "bilinmeyen problem",
        ),
        (r#"{"problem_id":"fib-001","model":""}"#, "boş model"),
        ("{}", "boş gövde"),
    ] {
        let (code, resp) = post(&format!("{}/api/runs", s.base), body).unwrap();
        assert_eq!(code, 400, "{why} reddedilmeli; yanıt: {resp}");
        assert!(
            resp.contains("\"error\""),
            "{why}: yapılandırılmış hata bekleniyor"
        );
    }
}

#[test]
fn doctor_exits_cleanly() {
    // `doctor` hiçbir servis çalışmasa bile çökmemeli (0 dışı kod dönebilir).
    let dir = std::env::temp_dir().join("quine-doctor-test");
    std::fs::create_dir_all(&dir).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_quine"))
        .arg("doctor")
        .current_dir(&dir)
        .output()
        .expect("doctor çalışmalı");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("Quine ortam"), "çıktı: {text}");
    assert!(out.status.code().is_some(), "süreç normal sonlanmalı");
}
