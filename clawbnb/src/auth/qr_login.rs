use std::time::{Duration, Instant};

use tracing::{info, error, debug};

use crate::api::client::ILinkClient;

use super::accounts::get_local_bot_tokens;

const DEFAULT_BOT_TYPE: &str = "3";
const FIXED_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
const QR_TTL: Duration = Duration::from_secs(300);
const MAX_QR_REFRESH: usize = 3;

#[derive(Debug)]
pub struct QrLoginResult {
    pub connected: bool,
    pub already_connected: bool,
    pub bot_token: Option<String>,
    pub account_id: Option<String>,
    pub base_url: Option<String>,
    pub user_id: Option<String>,
    pub message: String,
}

pub async fn interactive_qr_login(client: &ILinkClient) -> Result<QrLoginResult, String> {
    let local_tokens = get_local_bot_tokens();
    info!("Starting QR login (local_token_list count={})", local_tokens.len());

    let qr = client
        .fetch_qr_code(FIXED_BASE_URL, DEFAULT_BOT_TYPE, &local_tokens)
        .await?;

    info!("QR code received");
    println!("\n使用微信扫描以下二维码：");
    print_qr_to_terminal(&qr.qrcode_img_content);
    println!("QR URL: {}\n", qr.qrcode_img_content);

    wait_for_scan(client, FIXED_BASE_URL, &qr.qrcode, &qr.qrcode_img_content).await
}

async fn wait_for_scan(
    client: &ILinkClient,
    current_base: &str,
    qrcode: &str,
    _qr_url: &str,
) -> Result<QrLoginResult, String> {
    let start = Instant::now();
    let deadline = Duration::from_secs(480);
    let mut scanned_printed = false;
    let mut current_qrcode = qrcode.to_string();
    let mut refresh_count: usize = 1;
    let mut owned_base = current_base.to_string();

    while start.elapsed() < deadline {
        let status = client
            .poll_qr_status(&owned_base, &current_qrcode, Some(Duration::from_secs(35)))
            .await;

        let resp = match status {
            Ok(r) => r,
            Err(e) => {
                error!("QR poll failed: {e}");
                return Ok(QrLoginResult {
                    connected: false,
                    already_connected: false,
                    bot_token: None,
                    account_id: None,
                    base_url: None,
                    user_id: None,
                    message: format!("Login failed: {e}"),
                });
            }
        };

        match resp.status.as_str() {
            "wait" => {
                print!(".");
            }
            "scaned" => {
                if !scanned_printed {
                    println!("\n👀 已扫码，在微信继续操作...");
                    scanned_printed = true;
                }
            }
            "confirmed" => {
                let bot_id = resp.ilink_bot_id.as_deref().unwrap_or("");
                if bot_id.is_empty() {
                    return Ok(QrLoginResult {
                        connected: false,
                        already_connected: false,
                        bot_token: None,
                        account_id: None,
                        base_url: None,
                        user_id: None,
                        message: "登录失败：服务器未返回 ilink_bot_id。".into(),
                    });
                }
                println!("\n✅ 与微信连接成功！");
                return Ok(QrLoginResult {
                    connected: true,
                    already_connected: false,
                    bot_token: resp.bot_token,
                    account_id: Some(bot_id.to_string()),
                    base_url: resp.baseurl,
                    user_id: resp.ilink_user_id,
                    message: "✅ 与微信连接成功！".into(),
                });
            }
            "binded_redirect" => {
                println!("\n✅ 已连接过此实例，无需重复连接。");
                return Ok(QrLoginResult {
                    connected: false,
                    already_connected: true,
                    bot_token: None,
                    account_id: None,
                    base_url: None,
                    user_id: None,
                    message: "已连接过此实例，无需重复连接。".into(),
                });
            }
            "scaned_but_redirect" => {
                if let Some(host) = &resp.redirect_host {
                    owned_base = format!("https://{host}");
                    info!("IDC redirect to {}", owned_base);
                }
            }
            "expired" => {
                refresh_count += 1;
                if refresh_count > MAX_QR_REFRESH {
                    return Ok(QrLoginResult {
                        connected: false,
                        already_connected: false,
                        bot_token: None,
                        account_id: None,
                        base_url: None,
                        user_id: None,
                        message: "登录超时：二维码多次过期。".into(),
                    });
                }
                println!("\n⏳ 二维码已过期，正在刷新...({refresh_count}/{MAX_QR_REFRESH})");
                let local_tokens = get_local_bot_tokens();
                match client.fetch_qr_code(FIXED_BASE_URL, DEFAULT_BOT_TYPE, &local_tokens).await {
                    Ok(new_qr) => {
                        current_qrcode = new_qr.qrcode;
                        owned_base = FIXED_BASE_URL.to_string();
                        scanned_printed = false;
                        println!("🔄 新二维码已生成，请重新扫描\n");
                        print_qr_to_terminal(&new_qr.qrcode_img_content);
                    }
                    Err(e) => {
                        return Ok(QrLoginResult {
                            connected: false,
                            already_connected: false,
                            bot_token: None,
                            account_id: None,
                            base_url: None,
                            user_id: None,
                            message: format!("刷新二维码失败: {e}"),
                        });
                    }
                }
            }
            other => {
                debug!("unknown QR status: {other}");
            }
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    Ok(QrLoginResult {
        connected: false,
        already_connected: false,
        bot_token: None,
        account_id: None,
        base_url: None,
        user_id: None,
        message: "登录超时，请重试。".into(),
    })
}

fn print_qr_to_terminal(url: &str) {
    use qrcode::QrCode;
    let code = match QrCode::new(url.as_bytes()) {
        Ok(c) => c,
        Err(_) => {
            println!("(无法生成终端二维码，请访问上方 URL)");
            return;
        }
    };

    let matrix = code.to_colors();
    let width = code.width();
    let height = matrix.len() / width;

    // Use Unicode half-block chars: each row of output = 2 rows of pixels
    // ▀ = top half, ▄ = bottom half, █ = both, ' ' = neither
    // This keeps the QR square in terminal (chars are ~2:1 tall:wide)
    for y in (0..height).step_by(2) {
        for x in 0..width {
            let top = matrix[y * width + x] == qrcode::Color::Dark;
            let bot = if y + 1 < height {
                matrix[(y + 1) * width + x] == qrcode::Color::Dark
            } else {
                false
            };
            print!("{}", match (top, bot) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        println!();
    }
}
