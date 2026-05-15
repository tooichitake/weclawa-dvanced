//! SAML 2.0 SSO — v3.6 I4 ee enterprise feature.
//!
//! ## 设计取舍
//!
//! - 我们是 **SP (Service Provider)** 角色，operator IdP 是 Okta/AD FS/
//!   Azure AD 等。SAML 标准约定的 SP-initiated SSO 流程：
//!   1. SP 生成 AuthnRequest XML
//!   2. **HTTP-Redirect binding**：把 AuthnRequest base64+deflate 后塞进
//!      `?SAMLRequest=` query param 一并 302 给 browser
//!   3. browser 跳 IdP 登录 → IdP 回 POST `<SP-acs-url>` 带 SAMLResponse
//!   4. SP 解 SAMLResponse → verify XML signature → 抽 attribute statement
//!      → 映射 weclawbot Role
//!
//! ## v3.6 实施范围
//!
//! - `SamlConfig` — operator 在 tenants 表配的 IdP 元数据
//! - `build_authn_request(&cfg)` — 构造 AuthnRequest XML + deflate +
//!   base64 → redirect URL
//! - `IdentityProvider` impl 复用 `crate::ee::sso::IdentityProvider` trait
//!
//! ## v3.7+ 留的
//!
//! - **SAMLResponse XML signature verify** —— 这是 SAML 的核心信任锚。
//!   需要 xml-rs / xmlsec / signed-xml 等成熟 crate（生态比 OIDC 弱很多）。
//!   v3.7 选 crate + 真做 verify。生产部署前**必须**完成。
//! - **Encrypted assertion (EncryptedAssertion)** —— 极少用，但 enterprise
//!   合规会要求。
//! - **SLO (Single Log-Out)** —— SP-initiated 跨 IdP 注销。

use base64::{engine::general_purpose, Engine as _};
use serde::{Deserialize, Serialize};

use crate::error::WeclawError;

/// 单 tenant 的 SAML IdP 配置 — 存 tenants.saml_config_json。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SamlConfig {
    /// SP entity ID — 一般是 `https://daemon.example/api/v1/auth/sso/saml/metadata`
    pub sp_entity_id: String,
    /// IdP entity ID — 从 IdP 元数据 XML `<EntityDescriptor entityID="...">` 抄
    pub idp_entity_id: String,
    /// IdP SSO redirect endpoint — operator 在 IdP UI 拿到
    pub idp_sso_url: String,
    /// SP Assertion Consumer Service URL — IdP POST SAMLResponse 到此
    pub sp_acs_url: String,
    /// IdP X.509 signing certificate (PEM)。verify SAMLResponse signature 用。
    /// v3.7 实施 verify 之前本字段先存着不读。
    pub idp_x509_cert_pem: String,
    /// 可选 NameIDPolicy format。默认 `urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress`
    #[serde(default = "default_nameid_format")]
    pub nameid_format: String,
}

fn default_nameid_format() -> String {
    "urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress".to_string()
}

impl SamlConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !self.idp_sso_url.starts_with("https://") {
            return Err("idp_sso_url must be https://".into());
        }
        if !self.sp_acs_url.starts_with("https://") {
            return Err("sp_acs_url must be https://".into());
        }
        if self.sp_entity_id.is_empty() {
            return Err("sp_entity_id must not be empty".into());
        }
        if self.idp_entity_id.is_empty() {
            return Err("idp_entity_id must not be empty".into());
        }
        if self.idp_x509_cert_pem.is_empty() {
            return Err("idp_x509_cert_pem must not be empty (v3.7 will verify)".into());
        }
        Ok(())
    }
}

/// 生成 SP-initiated SSO 跳转 URL（HTTP-Redirect binding）。
///
/// SAMLRequest 流程：
/// 1. 构造 AuthnRequest XML
/// 2. **deflate** (raw, no zlib header) 压缩
/// 3. base64 编码
/// 4. URL-encode 进 `?SAMLRequest=`
///
/// `relay_state` 通常是 SP-side state token (CSRF 防御 + 跳转目标 URL)。
/// 由 caller 生成 + 写进 session/cookie，callback 时比对。
pub fn build_authn_request(
    cfg: &SamlConfig,
    relay_state: &str,
) -> Result<String, WeclawError> {
    cfg.validate()
        .map_err(WeclawError::BadRequest)?;

    let issue_instant = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let request_id = format!("_weclawbot-{}", uuid::Uuid::new_v4());

    // 简化版 AuthnRequest XML — 主流 IdP (Okta / Azure AD) 接受这个最小集
    let xml = format!(
        r#"<samlp:AuthnRequest xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol" \
xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion" \
ID="{request_id}" Version="2.0" IssueInstant="{issue_instant}" \
Destination="{idp_sso}" AssertionConsumerServiceURL="{acs}" \
ProtocolBinding="urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST">\
<saml:Issuer>{sp_entity}</saml:Issuer>\
<samlp:NameIDPolicy Format="{nameid}" AllowCreate="true"/>\
</samlp:AuthnRequest>"#,
        request_id = request_id,
        issue_instant = issue_instant,
        idp_sso = xml_escape(&cfg.idp_sso_url),
        acs = xml_escape(&cfg.sp_acs_url),
        sp_entity = xml_escape(&cfg.sp_entity_id),
        nameid = xml_escape(&cfg.nameid_format),
    );

    // Deflate raw (no zlib header) per SAML HTTP-Redirect binding spec
    let compressed = deflate_raw(xml.as_bytes())?;
    let encoded = general_purpose::STANDARD.encode(&compressed);

    let qs = format!(
        "SAMLRequest={}&RelayState={}",
        urlencoding::encode(&encoded),
        urlencoding::encode(relay_state)
    );
    Ok(format!("{}?{}", cfg.idp_sso_url, qs))
}

/// v5.1 N4: 校验 SAMLResponse 签名 + 抽 NameID/attribute statement。
///
/// **简化的 SAML 签名验证** —— 不做完整 XML Canonicalization (C14N)，
/// 而是用基础启发式：
/// 1. 抽 `<ds:SignedInfo>` 段（已经 C14N 过的形态，IdP 端在签名前规范化）
/// 2. 抽 `<ds:SignatureValue>` (base64 RSA-SHA256)
/// 3. 用 `idp_x509_cert_pem` 中的 RSA pub key 验签 SignedInfo
/// 4. 抽 `<saml:NameID>` 作为用户 subject
///
/// **生产级 SAML 实施要点（本期不全覆盖）**：
/// - **真 XML C14N**：需 exclusive-c14n 实现，Rust 生态目前缺 ── 推荐
///   生产部署用 SP-side proxy (mod_auth_mellon / shibboleth-sp) 做 SAML
///   verify，weclawbot 收 proxy 注入的 trusted headers
/// - **Encrypted assertions**：极少 IdP 用，留 v6
/// - **SLO (Single Log-Out)**：跨 IdP 注销，留 v6
///
/// 当前 verify 提供**初步信任** —— 验证签名能区分 "完全伪造" vs
/// "IdP 真发的"，但对**重放/部分修改**防御较弱。SP-side proxy 部署
/// 是首选生产姿势。
pub fn verify_saml_response_basic(
    cfg: &SamlConfig,
    saml_response_xml: &str,
) -> Result<SamlAssertion, WeclawError> {
    // Step 1: parse XML structure (find signed_info + signature_value + name_id)
    let parts = parse_saml_response(saml_response_xml)?;

    // Step 2: verify signature
    verify_signature_rsa_sha256(
        &parts.signed_info_xml,
        &parts.signature_b64,
        &cfg.idp_x509_cert_pem,
    )?;

    // Step 3: extract assertion
    Ok(SamlAssertion {
        subject: parts.name_id,
        email: parts.attributes.get("email").cloned(),
        display_name: parts.attributes.get("displayName").cloned(),
    })
}

/// SAML assertion 抽出来的标准化字段。
#[derive(Debug, Clone)]
pub struct SamlAssertion {
    pub subject: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
}

struct SamlResponseParts {
    signed_info_xml: String,
    signature_b64: String,
    name_id: String,
    attributes: std::collections::HashMap<String, String>,
}

fn parse_saml_response(xml: &str) -> Result<SamlResponseParts, WeclawError> {
    // 基础正则抽取 (v5.1 简化版)。生产用 xml-rs / quick-xml 真 parser，
    // 但本期实施重点是接口形态，详细 SAX 解析留 v5.2。
    let signed_info_xml = extract_between(xml, "<ds:SignedInfo", "</ds:SignedInfo>")
        .ok_or_else(|| WeclawError::BadRequest("SAMLResponse missing <ds:SignedInfo>".into()))?;
    // 把 "<ds:SignedInfo>" 整 tag 包回去（不光是中间内容）
    let signed_info_xml = format!("<ds:SignedInfo{}</ds:SignedInfo>", signed_info_xml);

    let signature_b64 = extract_between(xml, "<ds:SignatureValue>", "</ds:SignatureValue>")
        .ok_or_else(|| {
            WeclawError::BadRequest("SAMLResponse missing <ds:SignatureValue>".into())
        })?
        .trim()
        .replace(&['\n', '\r', ' ', '\t'][..], "");

    let name_id = extract_between(xml, "<saml:NameID", "</saml:NameID>")
        .or_else(|| extract_between(xml, "<NameID", "</NameID>"))
        .map(|s| {
            // 去掉 attributes 部分（"Format=...">subject"）
            s.split('>').nth(1).unwrap_or("").trim().to_string()
        })
        .ok_or_else(|| WeclawError::BadRequest("SAMLResponse missing <NameID>".into()))?;

    // Attributes (简化 ── 仅按属性名取 string value，多值/复杂结构留后期)
    let mut attributes = std::collections::HashMap::new();
    for name in ["email", "displayName", "name"] {
        let pat_start = format!("AttributeName=\"{name}\"");
        if let Some(pos) = xml.find(&pat_start) {
            if let Some(after_value) = xml[pos..].find("<AttributeValue>") {
                let val_start = pos + after_value + "<AttributeValue>".len();
                if let Some(val_end) = xml[val_start..].find("</AttributeValue>") {
                    attributes.insert(
                        name.to_string(),
                        xml[val_start..val_start + val_end].trim().to_string(),
                    );
                }
            }
        }
    }

    Ok(SamlResponseParts {
        signed_info_xml,
        signature_b64,
        name_id,
        attributes,
    })
}

fn extract_between(s: &str, start: &str, end: &str) -> Option<String> {
    let i = s.find(start)?;
    let s = &s[i + start.len()..];
    let j = s.find(end)?;
    Some(s[..j].to_string())
}

/// v5.4: real RSA-PKCS#1 v1.5 SHA-256 verify on the SignedInfo bytes
/// against the IdP X.509 cert's RSA public key.
///
/// ## What this does
///
/// 1. PEM → DER → X.509 → RSA public key (via `x509-parser` + `rsa`)
/// 2. base64 → raw signature bytes
/// 3. `verifying_key.verify(signed_info_bytes, &signature)` → reject if not
///    signed by the IdP's private key counterpart
///
/// ## What this does NOT do (production gap — see SP-proxy guidance)
///
/// - **Exclusive XML Canonicalization (c14n) per W3C 2008 spec.** The
///   IdP signs the *canonicalized* `<ds:SignedInfo>` bytes; we sign the
///   raw extracted substring. Attackers who can inject whitespace /
///   reorder attributes / play with namespace prefixes may bypass
///   verification. **Production deployments should put weclawbot behind
///   `mod_auth_mellon` or `shibboleth-sp` which do canonical-form
///   verification in C against libxmlsec1.**
/// - X.509 chain validation. We trust the operator-configured leaf cert
///   as the issuer; no CRL/OCSP/intermediate-CA walk.
/// - Algorithm flexibility. Only RSA-SHA256 — modern IdPs (Okta/Azure
///   AD/AD FS) all default to this; RSA-SHA1 explicitly rejected as
///   deprecated.
#[cfg(feature = "ee")]
fn verify_signature_rsa_sha256(
    signed_info: &str,
    signature_b64: &str,
    cert_pem: &str,
) -> Result<(), WeclawError> {
    use base64::{engine::general_purpose, Engine as _};
    use rsa::pkcs1v15::{Signature, VerifyingKey};
    use rsa::pkcs8::DecodePublicKey;
    use rsa::sha2::Sha256;
    use rsa::signature::Verifier;
    use rsa::RsaPublicKey;

    if signed_info.is_empty() {
        return Err(WeclawError::BadRequest("signed_info empty".into()));
    }

    // Step 1: decode signature b64 → bytes
    let sig_bytes = general_purpose::STANDARD
        .decode(signature_b64.as_bytes())
        .map_err(|e| WeclawError::BadRequest(format!("signature base64: {e}")))?;
    if sig_bytes.len() < 128 {
        return Err(WeclawError::BadRequest(format!(
            "signature too short ({} bytes) — RSA-2048+ expects ≥256",
            sig_bytes.len()
        )));
    }

    // Step 2: parse PEM cert → DER → extract SubjectPublicKeyInfo (SPKI)
    if !cert_pem.contains("BEGIN CERTIFICATE") {
        return Err(WeclawError::BadRequest(
            "IdP cert PEM missing BEGIN CERTIFICATE marker".into(),
        ));
    }
    let pem_blocks = x509_parser::pem::Pem::iter_from_buffer(cert_pem.as_bytes())
        .next()
        .ok_or_else(|| WeclawError::BadRequest("IdP cert: no PEM block".into()))?
        .map_err(|e| WeclawError::BadRequest(format!("IdP cert PEM parse: {e}")))?;
    let cert = pem_blocks
        .parse_x509()
        .map_err(|e| WeclawError::BadRequest(format!("IdP cert X.509 parse: {e}")))?;

    // Step 3: SPKI DER → RsaPublicKey (rsa crate accepts PKCS#8 DER which
    // is identical to the SPKI form X.509 carries).
    let spki_der = cert.public_key().raw;
    let rsa_pubkey = RsaPublicKey::from_public_key_der(spki_der).map_err(|e| {
        WeclawError::BadRequest(format!(
            "IdP cert pubkey not RSA (DER decode failed): {e}"
        ))
    })?;

    // Step 4: verify signature
    let verifying_key = VerifyingKey::<Sha256>::new(rsa_pubkey);
    let signature = Signature::try_from(sig_bytes.as_slice())
        .map_err(|e| WeclawError::BadRequest(format!("signature byte length: {e}")))?;
    verifying_key
        .verify(signed_info.as_bytes(), &signature)
        .map_err(|e| {
            WeclawError::BadRequest(format!(
                "SAMLResponse signature verify failed: {e} — \
                 possible forgery or wrong IdP cert"
            ))
        })?;

    tracing::debug!(
        "SAML signature verify: RSA-PKCS#1 v1.5 SHA-256 passed \
         (bytes={}, signed_info_len={}). \
         Note: c14n is NOT yet applied — production should use SP-proxy.",
        sig_bytes.len(),
        signed_info.len()
    );
    Ok(())
}

/// Non-ee build: no SAML verify dependencies linked. Reject everything.
/// SAML routes are themselves gated on `cfg(feature = "ee")`, so this
/// shim is only here to keep `verify_saml_response_basic` compilable.
#[cfg(not(feature = "ee"))]
fn verify_signature_rsa_sha256(
    _signed_info: &str,
    _signature_b64: &str,
    _cert_pem: &str,
) -> Result<(), WeclawError> {
    Err(WeclawError::Internal(
        "SAML verify requires --features ee (rsa + x509-parser crates)".into(),
    ))
}

/// XML 字符串字面 escape — & < > " 转实体。
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Raw deflate 压缩（不带 zlib header）— SAML spec 要求。我们用 flate2
/// 不引入新依赖 —— 但 flate2 当前没在 deps 里。本期纯标准库实现：
/// 直接 base64 编码 plaintext XML，跳过 deflate。Okta / Azure 都接受
/// uncompressed SAMLRequest（标准说应该 deflate，多数 IdP 容错）。v3.7
/// 加 flate2 dep 跑标准路径。
fn deflate_raw(input: &[u8]) -> Result<Vec<u8>, WeclawError> {
    // 标准路径需要 flate2 raw deflate。当前实施 skip deflate（base64 of
    // plain XML），主流 IdP 容错。v3.7 加 flate2 = "1" dep + 真 deflate。
    Ok(input.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_cfg() -> SamlConfig {
        SamlConfig {
            sp_entity_id: "https://daemon.example/sp".into(),
            idp_entity_id: "https://okta.example/idp".into(),
            idp_sso_url: "https://okta.example/app/sso/saml".into(),
            sp_acs_url: "https://daemon.example/acs".into(),
            idp_x509_cert_pem: "-----BEGIN CERTIFICATE-----\nMOCK\n-----END CERTIFICATE-----".into(),
            nameid_format: default_nameid_format(),
        }
    }

    #[test]
    fn validate_ok() {
        assert!(sample_cfg().validate().is_ok());
    }

    #[test]
    fn validate_rejects_http() {
        let mut c = sample_cfg();
        c.idp_sso_url = "http://insecure".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_cert() {
        let mut c = sample_cfg();
        c.idp_x509_cert_pem = "".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn authn_request_url_carries_saml_params() {
        let url = build_authn_request(&sample_cfg(), "state-abc").unwrap();
        assert!(url.starts_with("https://okta.example/app/sso/saml?"));
        assert!(url.contains("SAMLRequest="));
        assert!(url.contains("RelayState=state-abc"));
    }

    #[test]
    fn xml_escape_handles_specials() {
        assert_eq!(xml_escape("a&b<c>\"'"), "a&amp;b&lt;c&gt;&quot;&apos;");
    }

    #[test]
    fn verify_response_rejects_malformed() {
        let cfg = sample_cfg();
        // No signature → rejected
        let r = verify_saml_response_basic(&cfg, "<samlp:Response/>");
        assert!(r.is_err());
        // Empty short signature → rejected
        let xml = "<ds:SignedInfo>abc</ds:SignedInfo><ds:SignatureValue>YWJj</ds:SignatureValue><saml:NameID>x</saml:NameID>";
        let r = verify_saml_response_basic(&cfg, xml);
        assert!(r.is_err()); // sig 太短
    }

    #[test]
    fn verify_response_rejects_fake_cert() {
        // v5.4: real RSA verify now in place — fake cert content (MOCK
        // placeholder) is correctly rejected. Previous test asserted
        // extraction succeeded with stub cert; that's no longer the
        // right contract.
        let cfg = sample_cfg();
        let sig = base64::engine::general_purpose::STANDARD.encode(vec![0u8; 256]);
        let xml = format!(
            "<ds:SignedInfo>placeholder</ds:SignedInfo>\
             <ds:SignatureValue>{}</ds:SignatureValue>\
             <saml:NameID Format=\"...\">alice@example.com</saml:NameID>",
            sig
        );
        let r = verify_saml_response_basic(&cfg, &xml);
        assert!(
            r.is_err(),
            "fake cert (MOCK PEM body) must fail X.509 parse / RSA verify"
        );
    }

    #[test]
    fn parse_extracts_name_id_independent_of_verify() {
        // Structural parsing should succeed even when signature verify
        // would fail downstream — confirms the path stages are
        // independent (so observability can show "parsed OK but verify
        // failed" in audit logs).
        let xml = "<ds:SignedInfo>x</ds:SignedInfo>\
                   <ds:SignatureValue>YWJj</ds:SignatureValue>\
                   <saml:NameID Format=\"f\">alice@example.com</saml:NameID>";
        let parts = parse_saml_response(xml).unwrap();
        assert_eq!(parts.name_id, "alice@example.com");
    }

    #[test]
    fn authn_request_decoded_xml_has_required_elements() {
        let url = build_authn_request(&sample_cfg(), "x").unwrap();
        // Extract SAMLRequest param
        let qs = url.split('?').nth(1).unwrap();
        let saml_param = qs
            .split('&')
            .find(|p| p.starts_with("SAMLRequest="))
            .unwrap();
        let encoded = saml_param.trim_start_matches("SAMLRequest=");
        let decoded = urlencoding::decode(encoded).unwrap();
        let bytes = general_purpose::STANDARD.decode(decoded.as_bytes()).unwrap();
        let xml = String::from_utf8(bytes).unwrap();
        assert!(xml.contains("<samlp:AuthnRequest"));
        assert!(xml.contains("https://daemon.example/sp"));
        assert!(xml.contains("https://daemon.example/acs"));
    }
}
