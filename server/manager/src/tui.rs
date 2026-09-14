use crate::{
    client::Client,
    platform,
    terminal::{registration_qr, safe_text},
    Result,
};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    style::{Color, ResetColor, SetBackgroundColor, SetForegroundColor},
    terminal::{self, Clear, ClearType},
};
use serde_json::{json, Value};
use std::{
    io::{self, IsTerminal, Write},
    path::Path,
};

fn capable() -> bool {
    io::stdin().is_terminal()
        && io::stdout().is_terminal()
        && std::env::var("TERM").is_ok_and(|v| v != "dumb")
        || cfg!(windows)
            && io::stdin().is_terminal()
            && io::stdout().is_terminal()
            && std::env::var("TERM").unwrap_or_default() != "dumb"
}
fn clear() {
    if capable() {
        let _ = execute!(io::stdout(), Clear(ClearType::All), cursor::MoveTo(0, 0));
    }
}
fn input(label: &str, default: &str) -> Result<String> {
    print!(
        "{label}{}: ",
        if default.is_empty() {
            String::new()
        } else {
            format!(" [{}]", safe_text(default))
        }
    );
    io::stdout().flush().map_err(|_| "terminal-write-failed")?;
    let mut value = String::new();
    if io::stdin()
        .read_line(&mut value)
        .map_err(|_| "terminal-read-failed")?
        == 0
    {
        return Err("terminal-closed".into());
    }
    Ok(if value.trim().is_empty() {
        default.to_owned()
    } else {
        value.trim().to_owned()
    })
}
fn fixed_endpoint_default(status: &Value) -> &str {
    if status["connectionState"]["mode"] == "managed" {
        ""
    } else {
        status["connection"]["endpoint"].as_str().unwrap_or("")
    }
}
fn fixed_endpoint(value: String) -> Result<String> {
    if value.trim().is_empty() {
        Err("fixed-endpoint-required".into())
    } else {
        Ok(value)
    }
}
fn pause() -> Result<()> {
    input("Enter를 누르면 돌아갑니다", "").map(|_| ())
}
struct Raw;
struct Screen;
impl Drop for Screen {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), ResetColor, terminal::LeaveAlternateScreen);
    }
}
impl Drop for Raw {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

fn choose(title: &str, items: &[String]) -> Result<Option<usize>> {
    if !capable() {
        println!("\n{title}");
        for (i, item) in items.iter().enumerate() {
            println!("{}. {}", i + 1, safe_text(item));
        }
        loop {
            let value = input("번호 선택 (0: 뒤로)", "")?;
            if value == "0" {
                return Ok(None);
            }
            if let Ok(index) = value.parse::<usize>() {
                if index > 0 && index <= items.len() {
                    return Ok(Some(index - 1));
                }
            }
            println!("목록에 있는 번호를 입력하세요.");
        }
    }
    terminal::enable_raw_mode().map_err(|_| "terminal-raw-mode-failed")?;
    let _raw = Raw;
    let mut index = 0;
    loop {
        execute!(io::stdout(), Clear(ClearType::All), cursor::MoveTo(0, 0))
            .map_err(|_| "terminal-write-failed")?;
        print!("{}\r\n\r\n", safe_text(title));
        for (i, item) in items.iter().enumerate() {
            print!(
                "{} {}. {}\r\n",
                if i == index { "›" } else { " " },
                i + 1,
                safe_text(item)
            );
        }
        print!("\r\n↑↓ 이동 · 번호 선택 · Enter 확인 · Esc 뒤로\r\n");
        io::stdout().flush().map_err(|_| "terminal-write-failed")?;
        match event::read().map_err(|_| "terminal-read-failed")? {
            Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Up => index = (index + items.len() - 1) % items.len(),
                KeyCode::Down => index = (index + 1) % items.len(),
                KeyCode::Enter => return Ok(Some(index)),
                KeyCode::Esc | KeyCode::Char('0') => return Ok(None),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(None)
                }
                KeyCode::Char(c) => {
                    if let Some(n) = c.to_digit(10) {
                        if n > 0 && n as usize <= items.len() {
                            return Ok(Some(n as usize - 1));
                        }
                    }
                }
                _ => (),
            },
            _ => (),
        }
    }
}
fn menu(title: &str, items: &[&str]) -> Result<Option<usize>> {
    choose(
        title,
        &items.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
    )
}
fn text(value: &Value) -> String {
    value
        .as_str()
        .map(safe_text)
        .unwrap_or_else(|| "조회할 수 없음".into())
}
pub fn bytes(value: &Value) -> String {
    let Some(n) = value.as_u64() else {
        return "측정 중".into();
    };
    if n >= 1024 * 1024 * 1024 {
        format!("{:.2} GiB", n as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if n >= 1024 * 1024 {
        format!("{:.1} MiB", n as f64 / (1024.0 * 1024.0))
    } else {
        format!("{n} B")
    }
}
pub fn error_message(code: &str) -> &str {
    match code {
        "startup-registration-required" => "실행 설정에서 서버 자동 실행을 등록한 뒤 서버를 시작하세요.",
        "management-stale-state" => "서버 상태가 변경되었습니다. 새로 확인한 뒤 다시 시도하세요.",
        "registration-already-issued" => "이미 발급한 요청입니다. 기기 목록을 확인하세요. 등록 링크를 잃었다면 해당 기기를 해제한 뒤 다시 등록하세요.",
        "management-response-incomplete" => "응답을 끝까지 받지 못했습니다. 다시 등록하기 전에 기기 목록을 확인하세요.",
        "public-endpoint-not-ready" => "서버 주소가 준비되지 않았습니다. 연결 설정을 확인하세요.",
        "fixed-endpoint-required" => "고정 주소를 입력하세요.",
        "invalid-device-name" => "기기 이름을 확인하세요. 1~80자이며 제어 문자는 사용할 수 없습니다.",
        "management-unauthorized" => "관리 인증 정보를 확인할 수 없습니다. 서버에 다시 연결하세요.",
        _ => "작업을 완료하지 못했습니다. 서버 상태와 설정을 확인하세요.",
    }
}
fn overview(value: &Value) {
    println!("서버 실행 중 · 실행 시간 {}초", value["uptimeSeconds"]);
    let endpoint = if value["connectionState"]["mode"] == "managed" {
        &value["tunnel"]["endpoint"]
    } else {
        &value["connection"]["endpoint"]
    };
    println!("서버 주소: {}", text(endpoint));
    println!(
        "서버 파일 용량: {}",
        if !value["storage"]["error"].is_null() && value["storage"]["totalBytes"].is_null() {
            "측정할 수 없음".into()
        } else {
            bytes(&value["storage"]["totalBytes"])
        }
    );
    println!(
        "드라이브 남은 공간: {}",
        bytes(&value["storage"]["availableBytes"])
    );
    if !value["storage"]["error"].is_null() {
        println!("용량을 새로 확인하지 못했습니다.");
        if !value["storage"]["measuredAt"].is_null() {
            println!("마지막 측정값을 표시합니다.");
        }
    }
    let count = value["devices"]
        .as_array()
        .map(|d| d.iter().filter(|d| d["revoked"] == false).count())
        .unwrap_or(0);
    println!("등록된 기기: {count}");
}

async fn register(client: &Client, status: &Value) -> Result<()> {
    clear();
    let name = input("등록할 기기의 이름", "")?;
    let request =
        risunest_sync_server::management::discovery::request_id().map_err(|e| e.code.to_owned())?;
    let issued = client
        .mutate(
            "devices",
            json!({"revision":status["revision"],"name":name,"requestId":request}),
        )
        .await?;
    let uri = issued["uri"]
        .as_str()
        .ok_or("invalid-management-response")?;
    clear();
    println!("기기 등록 링크\n\n{uri}\n");
    let (columns, rows) = terminal::size().unwrap_or((0, 0));
    let unicode = cfg!(windows)
        || ["LC_ALL", "LC_CTYPE", "LANG"]
            .iter()
            .filter_map(|k| std::env::var(k).ok())
            .any(|v| v.to_ascii_lowercase().replace('-', "").contains("utf8"));
    if let Some(qr) = registration_qr(uri, capable() && unicode, columns, rows) {
        execute!(
            io::stdout(),
            SetForegroundColor(Color::Black),
            SetBackgroundColor(Color::White)
        )
        .map_err(|_| "terminal-write-failed")?;
        print!("{qr}");
        execute!(io::stdout(), ResetColor).map_err(|_| "terminal-write-failed")?;
    }
    println!("등록할 기기의 RisuNest 앱에서 이 링크를 사용하세요.\n등록 링크는 이때만 발급합니다. 잃어버렸다면 기기를 해제한 뒤 다시 등록하세요.");
    pause()?;
    clear();
    Ok(())
}

async fn devices(client: &Client, status: &Value) -> Result<()> {
    match menu("기기", &["새 기기 등록", "기기 해제"])? {
        Some(0) => register(client, status).await?,
        Some(1) => {
            let devices: Vec<_> = status["devices"]
                .as_array()
                .ok_or("invalid-management-response")?
                .iter()
                .filter(|d| d["revoked"] == false)
                .collect();
            if devices.is_empty() {
                println!("등록된 기기가 없습니다.");
                pause()?;
                return Ok(());
            }
            let items = devices
                .iter()
                .map(|d| format!("{} ({})", text(&d["name"]), text(&d["id"])))
                .collect::<Vec<_>>();
            if let Some(index) = choose("해제할 기기", &items)? {
                clear();
                println!("해제한 기기는 다시 등록하기 전까지 연결할 수 없습니다.\n라이브러리의 대화와 파일은 삭제되지 않습니다.");
                if input("해제하려면 '해제' 입력", "")? == "해제" {
                    client
                        .mutate(
                            &format!(
                                "devices/{}/revoke",
                                devices[index]["id"]
                                    .as_str()
                                    .ok_or("invalid-management-response")?
                            ),
                            json!({"revision":status["revision"]}),
                        )
                        .await?;
                }
            }
        }
        _ => (),
    }
    Ok(())
}

async fn connection(client: &Client, status: &Value, executable: &Path) -> Result<()> {
    let action = menu(
        "연결",
        &[
            "연결 정보 조회",
            "주소 설정",
            "임시 주소 시작",
            "임시 주소 중지",
            "임시 주소 다시 시작",
            "레지스트리 다시 게시",
        ],
    )?;
    match action {
        Some(0) => {
            clear();
            let c = &status["connection"];
            println!(
                "서버 주소: {}\n현재 임시 주소: {}\n레지스트리 서버 주소: {}\n현재 UUID: {}",
                text(&c["endpoint"]),
                text(&status["tunnel"]["endpoint"]),
                text(&c["registryUrl"]),
                c["uuid"]
                    .as_str()
                    .map(safe_text)
                    .unwrap_or("아직 발급되지 않음".into())
            );
            pause()?;
        }
        Some(1) => {
            let Some(mode) = menu("연결 방식", &["고정 주소", "임시 주소 (Cloudflare Tunnel)"])?
            else {
                return Ok(());
            };
            clear();
            let c = &status["connection"];
            let endpoint = if mode == 0 {
                Some(fixed_endpoint(input(
                    "고정 주소",
                    fixed_endpoint_default(status),
                )?)?)
            } else {
                None
            };
            let cloudflared = if mode == 1 {
                Some(input(
                    "cloudflared 실행 파일",
                    c["cloudflared"].as_str().unwrap_or(
                        &executable
                            .with_file_name(if cfg!(windows) {
                                "cloudflared.exe"
                            } else {
                                "cloudflared"
                            })
                            .to_string_lossy(),
                    ),
                )?)
            } else {
                None
            };
            let Some(registry_mode) = menu(
                "주소 레지스트리",
                &["주소 입력", "기본 주소 사용", "사용 안 함"],
            )?
            else {
                return Ok(());
            };
            let registry = if registry_mode == 0 {
                println!("이미 등록한 기기의 레지스트리 주소는 자동으로 바뀌지 않습니다.");
                Some(input(
                    "레지스트리 서버 주소",
                    c["registryUrl"]
                        .as_str()
                        .or(status["defaultRegistryUrl"].as_str())
                        .unwrap_or(""),
                )?)
            } else if registry_mode == 1 {
                Some(
                    status["defaultRegistryUrl"]
                        .as_str()
                        .ok_or("default-registry-unavailable")?
                        .to_owned(),
                )
            } else {
                None
            };
            if menu("연결 설정을 적용할까요?", &["적용", "취소"])? != Some(0) {
                return Ok(());
            }
            client.mutate("connection",json!({"revision":status["revision"],"options":{"endpoint":endpoint,"cloudflared":cloudflared,"registryUrl":registry}})).await?;
        }
        Some(index @ 2..=5) => {
            let path = [
                "tunnel/start",
                "tunnel/stop",
                "tunnel/restart",
                "registry/repost",
            ][index - 2];
            client
                .mutate(path, json!({"revision":status["revision"]}))
                .await?;
        }
        _ => (),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_tunnel_address_is_not_a_fixed_address_default() {
        let status = json!({
            "connectionState": {"mode": "managed"},
            "connection": {"endpoint": "https://synthetic.trycloudflare.com"}
        });
        assert_eq!(fixed_endpoint_default(&status), "");
        assert_eq!(fixed_endpoint(String::new()).unwrap_err(), "fixed-endpoint-required");
    }

    #[test]
    fn fixed_address_remains_the_fixed_address_default() {
        let status = json!({
            "connectionState": {"mode": "fixed"},
            "connection": {"endpoint": "https://sync.example.com"}
        });
        assert_eq!(fixed_endpoint_default(&status), "https://sync.example.com");
    }
}

pub async fn run(root: &Path, executable: &Path) -> Result<()> {
    let _screen = if capable() {
        execute!(io::stdout(), terminal::EnterAlternateScreen)
            .map_err(|_| "terminal-write-failed")?;
        Some(Screen)
    } else {
        None
    };
    let client = Client::new(root.to_owned())?;
    loop {
        let status = client.status().await;
        let Some(action) = menu(
            if status.is_ok() {
                "RisuNest 동기화 서버 · 연결됨"
            } else {
                "RisuNest 동기화 서버 · 연결 안 됨"
            },
            &["개요", "기기", "연결", "실행 설정"],
        )?
        else {
            break;
        };
        let result = if action == 3 {
            match menu(
                "실행 설정",
                &[
                    "서버 시작",
                    "서버 중지",
                    "로그인 시 서버 자동 실행 등록",
                    "자동 실행 해제",
                    "자동 실행 상태 조회",
                ],
            )? {
                Some(0) => {
                    if status.is_ok() {
                        Ok(())
                    } else {
                        platform::start(root, executable)
                    }
                }
                Some(1) => match &status {
                    Ok(s) => {
                        println!("서버를 중지하면 기기 동기화가 멈춥니다.");
                        if input("중지하려면 '중지' 입력", "")? == "중지" {
                            client
                                .mutate("shutdown", json!({"revision":s["revision"]}))
                                .await
                                .map(|_| ())
                        } else {
                            Ok(())
                        }
                    }
                    Err(e) => Err(e.clone()),
                },
                Some(2) => platform::startup(root, executable, "install").map(|_| ()),
                Some(3) => platform::startup(root, executable, "remove").map(|_| ()),
                Some(4) => {
                    let state = platform::startup(root, executable, "status")?;
                    println!(
                        "자동 실행: {}",
                        if state.enabled {
                            "사용"
                        } else {
                            "사용 안 함"
                        }
                    );
                    pause()
                }
                _ => Ok(()),
            }
        } else {
            match status {
                Ok(status) => match action {
                    0 => {
                        clear();
                        overview(&status);
                        pause()
                    }
                    1 => devices(&client, &status).await,
                    2 => connection(&client, &status, executable).await,
                    _ => Ok(()),
                },
                Err(e) => Err(e),
            }
        };
        if let Err(error) = result {
            println!("{} ({})", error_message(&error), safe_text(&error));
            pause()?;
        }
    }
    clear();
    Ok(())
}
