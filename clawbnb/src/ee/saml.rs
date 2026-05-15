//! SAML 2.0 SSO — v3.6 I4 ee enterprise feature.
//!
//! # SECURITY: NOT PRODUCTION-READY YET
//!
//! **DO NOT mount the SAML routes (`/api/v1/auth/sso/saml/*`) into the
//! axum router until `verify_saml_response` properly verifies the
//! IdP's XML signature.** Currently `verify_saml_response_basic` only
//! validates timestamps and entity IDs; it does NOT check the
//! `<ds:Signature>` block. Any attacker who can POST to the SP ACS
//! endpoint can forge a SAMLResponse claiming to be from the IdP and
//! impersonate any user.
//!
//! What's needed to finish (plan v3.7+ scope):
//! - Pick a Rust XML-DSig crate (`signed-xml`? `xmlsec-rs`? Or shell out
//!   to `xmlsec1` C binary?). Rust ecosystem here is weak — most projects
//!   wrap `xmlsec1` via FFI.
//! - Implement canonicalization (C14N) — XML-DSig requires the signed
//!   element be C14N'd before hash, and the spec has subtle rules
//!   around namespace prefixes / whitespace.
//! - Validate certificate chain against operator-configured IdP cert
//!   (stored in tenants.saml_config_json).
//! - Add anti-replay (track seen response IDs for the assertion's
//!   lifetime).
//! - Re-audit `decode_saml_response` for XXE: the current quick-xml
//!   parser is XXE-safe by default, but any switch to a DOM parser
//!   must explicitly disable external entities.
//!
//! Tracking: see GitHub issues tagged `saml` and `sso`. Until those
//! land, leave the SAML implementation gated behind `ee` feature and
//! unrouted.
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
/// v7.2: full SAML 2.0 SP-receiver verify via
/// [`crate::ee::saml_dsig::verify_saml_response`].
///
/// **This is the production-safe entry point.** It enforces the strict
/// IdP profile (rsa-sha256 + exc-c14n + enveloped-signature + single
/// Reference), checks the digest covers the trusted assertion, verifies
/// the RSA signature, and validates audience + time conditions +
/// optional InResponseTo. Returns extracted [`SamlAssertion`] on
/// success.
///
/// Pass `expected_in_response_to=Some(req_id)` for SP-initiated flows
/// (we issued an AuthnRequest), `None` for IdP-initiated (unsolicited).
pub fn verify_saml_response_basic(
    cfg: &SamlConfig,
    saml_response_xml: &str,
) -> Result<SamlAssertion, WeclawError> {
    verify_saml_response_full(cfg, saml_response_xml, None)
}

/// Full SP-receiver verify with optional InResponseTo binding.
pub fn verify_saml_response_full(
    cfg: &SamlConfig,
    saml_response_xml: &str,
    expected_in_response_to: Option<&str>,
) -> Result<SamlAssertion, WeclawError> {
    use crate::ee::saml_dsig::{verify_saml_response, SamlError};
    let verified = verify_saml_response(
        saml_response_xml,
        &cfg.idp_x509_cert_pem,
        &cfg.sp_entity_id,
        expected_in_response_to,
    )
    .map_err(|e: SamlError| match e {
        SamlError::Profile(m) => WeclawError::BadRequest(format!("saml profile: {m}")),
        SamlError::Missing(m) => WeclawError::BadRequest(format!("saml missing: {m}")),
        SamlError::Parse(m) => WeclawError::BadRequest(format!("saml parse: {m}")),
        SamlError::Canonicalization(m) => {
            WeclawError::BadRequest(format!("saml c14n: {m}"))
        }
        SamlError::Digest(m) => WeclawError::BadRequest(format!("saml digest: {m}")),
        SamlError::Signature(m) => WeclawError::BadRequest(format!("saml signature: {m}")),
        SamlError::Cert(m) => WeclawError::Internal(format!("saml cert: {m}")),
    })?;
    Ok(SamlAssertion {
        subject: verified.subject_name_id,
        email: verified.attributes.get("email").cloned(),
        display_name: verified.attributes.get("displayName").cloned(),
    })
}

/// SAML assertion 抽出来的标准化字段。
#[derive(Debug, Clone)]
pub struct SamlAssertion {
    pub subject: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
}

// v7.2: `parse_saml_response` + `extract_between` removed. They were
// substring-based extractors only safe to extract *anything* if you
// also did c14n + digest verify (which they didn't). All callers go
// through `crate::ee::saml_dsig::verify_saml_response` which parses
// via quick-xml, canonicalizes per W3C exc-c14n, and rejects
// non-canonical IdP profiles loudly.

// v7.2: `verify_signature_rsa_sha256` (both feature-gated variants)
// removed. The RSA verify lives in `crate::ee::saml_dsig` now, where
// it's called after exc-c14n on the SignedInfo bytes — that's the
// only safe order. The old standalone signed-substring verify was
// vulnerable to whitespace mutation and namespace-prefix attacks.

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
    fn verify_response_rejects_no_signature() {
        let cfg = sample_cfg();
        let r = verify_saml_response_basic(&cfg, "<samlp:Response/>");
        assert!(r.is_err());
    }

    #[test]
    fn verify_response_rejects_non_canonical_profile() {
        // SAMLResponse with SignatureMethod = rsa-sha1 (deprecated) →
        // strict profile reject before we get anywhere near cert verify.
        let cfg = sample_cfg();
        let xml = r##"<samlp:Response xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol" ID="r1">
  <ds:Signature xmlns:ds="http://www.w3.org/2000/09/xmldsig#">
    <ds:SignedInfo>
      <ds:CanonicalizationMethod Algorithm="http://www.w3.org/2001/10/xml-exc-c14n#"/>
      <ds:SignatureMethod Algorithm="http://www.w3.org/2000/09/xmldsig#rsa-sha1"/>
      <ds:Reference URI="#r1">
        <ds:Transforms>
          <ds:Transform Algorithm="http://www.w3.org/2000/09/xmldsig#enveloped-signature"/>
          <ds:Transform Algorithm="http://www.w3.org/2001/10/xml-exc-c14n#"/>
        </ds:Transforms>
        <ds:DigestMethod Algorithm="http://www.w3.org/2001/04/xmlenc#sha256"/>
        <ds:DigestValue>abc</ds:DigestValue>
      </ds:Reference>
    </ds:SignedInfo>
    <ds:SignatureValue>def</ds:SignatureValue>
  </ds:Signature>
</samlp:Response>"##;
        let r = verify_saml_response_basic(&cfg, xml);
        let err = r.unwrap_err().to_string();
        assert!(
            err.contains("SignatureMethod must be"),
            "expected rsa-sha1 to be rejected; got: {err}"
        );
    }

    #[test]
    fn verify_response_rejects_xxe_doctype() {
        let cfg = sample_cfg();
        let xml = "<!DOCTYPE foo SYSTEM 'file:///etc/passwd'><samlp:Response/>";
        let r = verify_saml_response_basic(&cfg, xml);
        let err = r.unwrap_err().to_string();
        assert!(err.contains("DOCTYPE"), "expected XXE reject; got: {err}");
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
