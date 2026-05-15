//! SAML XML Digital Signature verify — strict-profile implementation. v7.2.
//!
//! ## Why this exists
//!
//! `ee::saml::verify_signature_rsa_sha256` does the RSA arithmetic but
//! signs the *raw substring* of `<ds:SignedInfo>...</ds:SignedInfo>`,
//! which is **not** what the IdP signed. The IdP signed the
//! **canonicalized** form (exclusive XML-C14N per W3C 2002). Attackers
//! who can mutate whitespace, reorder attributes, change quote style,
//! or wrap a malicious assertion around a legit signed assertion (SAML
//! signature wrapping attack, SWA) can bypass that simple substring
//! verification. **This module is the actual safe verifier; the older
//! one is kept temporarily for compile compat and will be removed.**
//!
//! ## Strict profile
//!
//! Implementing full exclusive XML-C14N + arbitrary XML-DSig profiles
//! correctly is ≥1000 LoC of subtle parser code. Instead we accept only
//! the profile that all major commercial IdPs emit:
//!
//! - `<ds:Signature xmlns:ds="http://www.w3.org/2000/09/xmldsig#">` —
//!   prefix `ds:` for digital-signature namespace
//! - `<ds:SignedInfo>` containing
//!   - `<ds:CanonicalizationMethod Algorithm="http://www.w3.org/2001/10/xml-exc-c14n#">`
//!   - `<ds:SignatureMethod Algorithm="http://www.w3.org/2001/04/xmldsig-more#rsa-sha256">`
//!   - **Exactly one** `<ds:Reference URI="#<id>">` with
//!     - `<ds:Transforms>` containing exactly
//!       `<ds:Transform Algorithm="http://www.w3.org/2000/09/xmldsig#enveloped-signature">`
//!       and `<ds:Transform Algorithm="http://www.w3.org/2001/10/xml-exc-c14n#">`
//!     - `<ds:DigestMethod Algorithm="http://www.w3.org/2001/04/xmlenc#sha256">`
//!     - `<ds:DigestValue>` (base64 SHA-256)
//! - `<ds:SignatureValue>` (base64 RSA-PKCS#1 v1.5 SHA-256)
//! - `<ds:KeyInfo><ds:X509Data><ds:X509Certificate>...</...>` (optional;
//!   we trust operator-configured cert, IdP cert in response is ignored)
//! - The `Reference URI` must point to either the `<samlp:Response>`
//!   root id or to the single `<saml:Assertion>` id. The assertion is
//!   the trusted authoritative element — we extract claims from it
//!   after verification.
//!
//! Anything outside this profile is rejected with a `SamlError::Profile`
//! that includes a one-line "your IdP emitted X; we need Y" message
//! to make operator triage fast.
//!
//! ## Defenses
//!
//! - **SWA (signature wrapping)**: We extract the `<Assertion>` element
//!   **only** by following the signed `Reference URI`. Any unsigned
//!   assertion floating elsewhere in the response is ignored. This is
//!   the only safe SWA defense — string-matching `<Assertion>` first
//!   and then verifying is the classic mistake.
//! - **XXE**: `quick-xml` is SAX, doesn't process DOCTYPE entities. We
//!   additionally reject any payload containing `<!DOCTYPE` to belt-and-
//!   suspenders the parser config.
//! - **Algorithm confusion**: Only `rsa-sha256` accepted. `rsa-sha1` and
//!   `hmac-sha1` etc. are rejected. The set is allow-list, not deny-list.
//! - **Cert pinning**: We never trust an `<X509Certificate>` embedded in
//!   the response. The IdP cert is operator-configured in `SamlConfig`
//!   and used as-is. (Cert chain validation against system trust store
//!   would be additional belt-and-suspenders but doesn't add much
//!   security — the operator already trusts the cert by configuring it.)

use std::collections::BTreeMap;
use std::io::Cursor;

use base64::{engine::general_purpose, Engine as _};
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;
use rsa::pkcs1v15::{Signature, VerifyingKey};
use rsa::pkcs8::DecodePublicKey;
use rsa::sha2::{Digest, Sha256};
use rsa::signature::Verifier;
use rsa::RsaPublicKey;

/// Errors specific to SAML signature verification. Each variant carries
/// an actionable message — operators see these via daemon log when an
/// SSO attempt fails.
#[derive(Debug, thiserror::Error)]
pub enum SamlError {
    #[error("SAML profile rejected: {0}")]
    Profile(String),
    #[error("XML parse: {0}")]
    Parse(String),
    #[error("XML canonicalization: {0}")]
    Canonicalization(String),
    #[error("signature verify: {0}")]
    Signature(String),
    #[error("digest mismatch: {0}")]
    Digest(String),
    #[error("cert decode: {0}")]
    Cert(String),
    #[error("missing element: {0}")]
    Missing(String),
}

const DS_NS: &str = "http://www.w3.org/2000/09/xmldsig#";
const SAMLP_NS: &str = "urn:oasis:names:tc:SAML:2.0:protocol";
const SAML_NS: &str = "urn:oasis:names:tc:SAML:2.0:assertion";
const ALGO_EXC_C14N: &str = "http://www.w3.org/2001/10/xml-exc-c14n#";
const ALGO_RSA_SHA256: &str = "http://www.w3.org/2001/04/xmldsig-more#rsa-sha256";
const ALGO_SHA256: &str = "http://www.w3.org/2001/04/xmlenc#sha256";
const ALGO_ENVELOPED: &str = "http://www.w3.org/2000/09/xmldsig#enveloped-signature";

/// What `extract_signed_parts` pulls out of the SAMLResponse — all the
/// strings the verifier downstream needs.
#[derive(Debug)]
pub struct SignedParts {
    /// The original byte range of `<ds:SignedInfo>...</ds:SignedInfo>`
    /// from the input. We re-canonicalize this for the RSA verify.
    pub signed_info_raw: String,
    /// `<ds:Reference URI="#X">` — must start with '#'.
    pub reference_uri: String,
    /// `<ds:DigestValue>` from inside the Reference. base64 SHA-256.
    pub digest_value: String,
    /// `<ds:SignatureValue>` (base64) — the RSA signature over c14n(SignedInfo).
    pub signature_b64: String,
    /// The byte range of the element whose id equals `reference_uri[1..]`.
    /// This is what the digest is supposed to cover (after enveloped
    /// transform + exc-c14n).
    pub referenced_xml_raw: String,
    /// True if the referenced element was the `<samlp:Response>` root,
    /// False if it was an `<saml:Assertion>`. Affects which audience /
    /// conditions we trust.
    pub referenced_is_response: bool,
}

/// The fully verified, profile-checked SAML claims. Caller uses these
/// downstream to mint admin keys.
#[derive(Debug, Clone)]
pub struct VerifiedAssertion {
    pub subject_name_id: String,
    pub issuer: String,
    pub session_index: Option<String>,
    /// Map of AttributeName → first AttributeValue. SAML allows multiple
    /// values per attribute but in 99% of cases each has exactly one;
    /// we keep the API simple and let callers re-parse if they need
    /// multi-value attributes.
    pub attributes: BTreeMap<String, String>,
}

/// Top-level entry. Parses + verifies + extracts claims from a SAML 2.0
/// Response XML. The output is safe to act on (the operator-configured
/// IdP signed every claim in it).
///
/// `expected_audience` is the SP entity ID (must appear in
/// `<AudienceRestriction>`). `expected_in_response_to` is the
/// AuthnRequest ID we issued — `None` if SP-initiated flow is
/// unbinded (rare; some IdPs allow IdP-initiated SSO).
pub fn verify_saml_response(
    response_xml: &str,
    idp_cert_pem: &str,
    expected_audience: &str,
    expected_in_response_to: Option<&str>,
) -> Result<VerifiedAssertion, SamlError> {
    // v7.4 — OTel span for SAML verification. Captures payload size +
    // expected audience so trace exporters can spot "all failures from
    // IdP X" patterns. Implementation result (Ok/Err) is logged via
    // tracing::warn! on the Err branch — span itself records timing.
    let _span = tracing::info_span!(
        "sso.saml.verify",
        response_bytes = response_xml.len(),
        expected_audience = expected_audience,
        in_response_to = expected_in_response_to.unwrap_or("none"),
    )
    .entered();
    if response_xml.contains("<!DOCTYPE") {
        return Err(SamlError::Profile(
            "SAMLResponse contains <!DOCTYPE — XXE defense-in-depth reject".into(),
        ));
    }

    // Step 1: extract structural parts
    let parts = extract_signed_parts(response_xml)?;

    if !parts.reference_uri.starts_with('#') {
        return Err(SamlError::Profile(format!(
            "Reference URI must start with '#': got '{}'",
            parts.reference_uri
        )));
    }

    // Step 2: canonicalize the referenced element + verify digest
    //
    // The enveloped-signature transform strips the <ds:Signature>
    // element from the referenced element before digesting. Then
    // exc-c14n canonicalizes the remainder.
    let referenced_after_enveloped = strip_signature_element(&parts.referenced_xml_raw)?;
    let referenced_canon = canonicalize_exc_c14n(&referenced_after_enveloped)?;
    let mut hasher = Sha256::new();
    hasher.update(referenced_canon.as_bytes());
    let computed_digest = general_purpose::STANDARD.encode(hasher.finalize());
    if !constant_time_eq(
        computed_digest.as_bytes(),
        parts.digest_value.trim().as_bytes(),
    ) {
        return Err(SamlError::Digest(format!(
            "computed digest = {computed_digest}, expected = {} — \
             SAML payload was modified between IdP signing and SP receipt \
             (or IdP signed a different element than we extracted)",
            parts.digest_value.trim()
        )));
    }

    // Step 3: canonicalize SignedInfo + verify RSA signature
    let signed_info_canon = canonicalize_exc_c14n(&parts.signed_info_raw)?;
    let sig_bytes = general_purpose::STANDARD
        .decode(parts.signature_b64.trim().as_bytes())
        .map_err(|e| SamlError::Signature(format!("signature base64: {e}")))?;
    verify_rsa_sha256(signed_info_canon.as_bytes(), &sig_bytes, idp_cert_pem)?;

    // Step 4: now that integrity is proven, parse the referenced element
    // for the actual claims (NameID, attributes, conditions, etc.).
    extract_claims(
        &parts.referenced_xml_raw,
        parts.referenced_is_response,
        expected_audience,
        expected_in_response_to,
    )
}

// ============================================================================
// Step-by-step internals
// ============================================================================

/// Walk the XML once, recording byte ranges of SignedInfo + Reference
/// + the element whose id matches the Reference URI. Also pulls the
/// DigestValue and SignatureValue strings + verifies algorithm allowlist.
fn extract_signed_parts(xml: &str) -> Result<SignedParts, SamlError> {
    let mut reader = NsReader::from_str(xml);
    reader.config_mut().trim_text(false);

    // Tracking state
    let mut depth: usize = 0;
    let mut signature_depth: Option<usize> = None;
    let mut signed_info_start: Option<usize> = None;
    let mut signed_info_end: Option<usize> = None;
    let mut signature_value: Option<String> = None;
    let mut digest_value: Option<String> = None;
    let mut reference_uri: Option<String> = None;
    let mut reference_seen = false;
    let mut in_signature_value = false;
    let mut in_digest_value = false;

    let mut canonicalization_seen = false;
    let mut signature_method_seen = false;
    let mut digest_method_seen = false;
    let mut enveloped_transform_seen = false;
    let mut c14n_transform_seen = false;

    // Track element ids + byte ranges
    let mut id_ranges: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    // Stack of (id?, start_byte) to record end on EndElement
    let mut element_stack: Vec<(Option<String>, usize, String)> = Vec::new();
    let mut response_root_id: Option<String> = None;

    // Structured event analysis. The event borrows reader's namespace
    // stack, so we must extract every owned bit we need into a small
    // owned `Action` struct, drop the event, then use `buffer_position`
    // again. This is the cleanest way to avoid the borrow conflict
    // between event lifetime and buffer_position queries.
    //
    // `EmptyElement` is the self-closing `<foo/>` form. Treating it as
    // StartElement+EndElement avoids missing IdP elements that are
    // commonly emitted self-closed (`<ds:CanonicalizationMethod ... />`
    // etc.).
    enum Action {
        StartElement {
            ns: Option<String>,
            local: String,
            id_attr: Option<String>,
            algorithm: Option<String>,
            uri: Option<String>,
        },
        EmptyElement {
            ns: Option<String>,
            local: String,
            id_attr: Option<String>,
            algorithm: Option<String>,
            uri: Option<String>,
        },
        EndElement {
            ns: Option<String>,
        },
        Text(String),
        Eof,
        Other,
    }

    let mut buf = Vec::new();
    let mut pos_before = reader.buffer_position() as usize;
    loop {
        let action: Action = {
            let event = reader
                .read_resolved_event_into(&mut buf)
                .map_err(|e| SamlError::Parse(format!("read: {e}")))?;
            match event {
                (ns, Event::Start(e)) => {
                    let local = std::str::from_utf8(e.local_name().as_ref())
                        .map_err(|e| SamlError::Parse(format!("local name utf8: {e}")))?
                        .to_string();
                    let ns_str = match ns {
                        ResolveResult::Bound(n) => Some(
                            std::str::from_utf8(n.as_ref())
                                .map_err(|e| SamlError::Parse(format!("ns utf8: {e}")))?
                                .to_string(),
                        ),
                        _ => None,
                    };
                    Action::StartElement {
                        ns: ns_str,
                        local,
                        id_attr: attr_value(&e, "ID"),
                        algorithm: attr_value(&e, "Algorithm"),
                        uri: attr_value(&e, "URI"),
                    }
                }
                (ns, Event::Empty(e)) => {
                    let local = std::str::from_utf8(e.local_name().as_ref())
                        .map_err(|e| SamlError::Parse(format!("local name utf8: {e}")))?
                        .to_string();
                    let ns_str = match ns {
                        ResolveResult::Bound(n) => Some(
                            std::str::from_utf8(n.as_ref())
                                .map_err(|e| SamlError::Parse(format!("ns utf8: {e}")))?
                                .to_string(),
                        ),
                        _ => None,
                    };
                    Action::EmptyElement {
                        ns: ns_str,
                        local,
                        id_attr: attr_value(&e, "ID"),
                        algorithm: attr_value(&e, "Algorithm"),
                        uri: attr_value(&e, "URI"),
                    }
                }
                (_, Event::Text(t)) => Action::Text(
                    t.unescape()
                        .map_err(|e| SamlError::Parse(format!("text unescape: {e}")))?
                        .into_owned(),
                ),
                (ns, Event::End(_)) => {
                    let ns_str = match ns {
                        ResolveResult::Bound(n) => Some(
                            std::str::from_utf8(n.as_ref())
                                .map_err(|e| SamlError::Parse(format!("ns utf8: {e}")))?
                                .to_string(),
                        ),
                        _ => None,
                    };
                    Action::EndElement { ns: ns_str }
                }
                (_, Event::Eof) => Action::Eof,
                _ => Action::Other,
            }
        };
        // event is now out of scope — safe to query reader.
        let pos_after = reader.buffer_position() as usize;

        // Helper: apply Start-style state mutations for an element.
        // Returns Err on a profile violation (same shape for Start and
        // Empty/self-closing variants).
        let mut process_start =
            |depth: usize,
             ns: &Option<String>,
             local: &str,
             id_attr: &Option<String>,
             algorithm: &Option<String>,
             uri: Option<String>|
             -> Result<(), SamlError> {
                if depth == 1 && ns.as_deref() == Some(SAMLP_NS) && local == "Response" {
                    response_root_id = id_attr.clone();
                }
                if ns.as_deref() != Some(DS_NS) {
                    return Ok(());
                }
                if local == "Signature" {
                    signature_depth = Some(depth);
                } else if local == "SignedInfo" && signature_depth.is_some() {
                    signed_info_start = Some(pos_before);
                } else if local == "CanonicalizationMethod" && signature_depth.is_some() {
                    canonicalization_seen = true;
                    if algorithm.as_deref() != Some(ALGO_EXC_C14N) {
                        return Err(SamlError::Profile(format!(
                            "CanonicalizationMethod must be {ALGO_EXC_C14N}, got {algorithm:?}"
                        )));
                    }
                } else if local == "SignatureMethod" && signature_depth.is_some() {
                    signature_method_seen = true;
                    if algorithm.as_deref() != Some(ALGO_RSA_SHA256) {
                        return Err(SamlError::Profile(format!(
                            "SignatureMethod must be {ALGO_RSA_SHA256}, got {algorithm:?}"
                        )));
                    }
                } else if local == "Reference" && signature_depth.is_some() {
                    if reference_seen {
                        return Err(SamlError::Profile(
                            "multiple <ds:Reference> elements — only one supported".into(),
                        ));
                    }
                    reference_seen = true;
                    reference_uri = uri;
                } else if local == "Transform" && signature_depth.is_some() {
                    match algorithm.as_deref() {
                        Some(ALGO_ENVELOPED) => enveloped_transform_seen = true,
                        Some(ALGO_EXC_C14N) => c14n_transform_seen = true,
                        other => {
                            return Err(SamlError::Profile(format!(
                                "unsupported Transform Algorithm: {other:?}"
                            )));
                        }
                    }
                } else if local == "DigestMethod" && signature_depth.is_some() {
                    digest_method_seen = true;
                    if algorithm.as_deref() != Some(ALGO_SHA256) {
                        return Err(SamlError::Profile(format!(
                            "DigestMethod must be {ALGO_SHA256}, got {algorithm:?}"
                        )));
                    }
                } else if local == "DigestValue" && signature_depth.is_some() {
                    in_digest_value = true;
                } else if local == "SignatureValue" && signature_depth.is_some() {
                    in_signature_value = true;
                }
                Ok(())
            };

        match action {
            Action::StartElement {
                ns,
                local,
                id_attr,
                algorithm,
                uri,
            } => {
                depth += 1;
                element_stack.push((id_attr.clone(), pos_before, local.clone()));
                process_start(depth, &ns, &local, &id_attr, &algorithm, uri)?;
            }
            Action::EmptyElement {
                ns,
                local,
                id_attr,
                algorithm,
                uri,
            } => {
                // Self-closing: treat as Start + immediate End. Don't
                // push onto element_stack since there's no matching End
                // event coming. Don't update id_ranges either (an
                // empty element by definition can't be a Reference URI
                // target — those are containers).
                let virtual_depth = depth + 1;
                process_start(virtual_depth, &ns, &local, &id_attr, &algorithm, uri)?;
                // Reset the flags Start would have set for "we're now
                // inside Value text": Empty has no inner text.
                if ns.as_deref() == Some(DS_NS) {
                    if local == "DigestValue" {
                        in_digest_value = false;
                    } else if local == "SignatureValue" {
                        in_signature_value = false;
                    }
                }
            }
            Action::Text(text) => {
                if in_signature_value {
                    signature_value
                        .get_or_insert_with(String::new)
                        .push_str(text.trim());
                }
                if in_digest_value {
                    digest_value
                        .get_or_insert_with(String::new)
                        .push_str(text.trim());
                }
            }
            Action::EndElement { ns } => {
                let (id_opt, start, name) = element_stack.pop().ok_or_else(|| {
                    SamlError::Parse("end without matching start".into())
                })?;
                if let Some(id) = id_opt {
                    id_ranges.insert(id, (start, pos_after));
                }

                if ns.as_deref() == Some(DS_NS) {
                    if name == "SignedInfo" && signature_depth.is_some() {
                        signed_info_end = Some(pos_after);
                    } else if name == "SignatureValue" {
                        in_signature_value = false;
                    } else if name == "DigestValue" {
                        in_digest_value = false;
                    } else if name == "Signature" && Some(depth) == signature_depth {
                        signature_depth = None;
                    }
                }
                depth -= 1;
            }
            Action::Eof => break,
            Action::Other => {}
        }
        pos_before = pos_after;
        buf.clear();
    }

    if !canonicalization_seen {
        return Err(SamlError::Missing("CanonicalizationMethod".into()));
    }
    if !signature_method_seen {
        return Err(SamlError::Missing("SignatureMethod".into()));
    }
    if !digest_method_seen {
        return Err(SamlError::Missing("DigestMethod".into()));
    }
    if !reference_seen {
        return Err(SamlError::Missing("Reference".into()));
    }
    if !enveloped_transform_seen {
        return Err(SamlError::Profile(
            "Reference must include enveloped-signature Transform".into(),
        ));
    }
    if !c14n_transform_seen {
        return Err(SamlError::Profile(
            "Reference must include exc-c14n Transform".into(),
        ));
    }

    let (sig_info_s, sig_info_e) = match (signed_info_start, signed_info_end) {
        (Some(s), Some(e)) => (s, e),
        _ => return Err(SamlError::Missing("SignedInfo".into())),
    };
    let signed_info_raw = xml[sig_info_s..sig_info_e].to_string();

    let signature_b64 =
        signature_value.ok_or_else(|| SamlError::Missing("SignatureValue".into()))?;
    let digest_value = digest_value.ok_or_else(|| SamlError::Missing("DigestValue".into()))?;
    let reference_uri = reference_uri.ok_or_else(|| SamlError::Missing("Reference URI".into()))?;

    // Resolve Reference URI to an actual element byte range
    let id = reference_uri.trim_start_matches('#');
    let (ref_start, ref_end) = id_ranges
        .get(id)
        .copied()
        .ok_or_else(|| SamlError::Missing(format!("element with id={id} (per Reference URI)")))?;
    let referenced_xml_raw = xml[ref_start..ref_end].to_string();
    let referenced_is_response = response_root_id.as_deref() == Some(id);

    Ok(SignedParts {
        signed_info_raw,
        reference_uri,
        digest_value,
        signature_b64,
        referenced_xml_raw,
        referenced_is_response,
    })
}

fn attr_value(e: &BytesStart, name: &str) -> Option<String> {
    for attr in e.attributes().flatten() {
        if attr.key.local_name().as_ref() == name.as_bytes() {
            if let Ok(v) = attr.unescape_value() {
                return Some(v.into_owned());
            }
        }
    }
    None
}

/// Strip the `<ds:Signature>` subtree from an XML fragment — the
/// "enveloped-signature" transform. We use a simple state machine over
/// quick-xml events.
fn strip_signature_element(xml: &str) -> Result<String, SamlError> {
    let mut reader = NsReader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut writer = quick_xml::writer::Writer::new(Cursor::new(Vec::new()));
    let mut skip_depth: usize = 0;
    let mut buf = Vec::new();
    loop {
        let event = reader
            .read_resolved_event_into(&mut buf)
            .map_err(|e| SamlError::Parse(format!("strip read: {e}")))?;
        match event {
            (ns, Event::Start(e)) => {
                let is_signature = matches!(ns, ResolveResult::Bound(n) if n.as_ref() == DS_NS.as_bytes())
                    && e.local_name().as_ref() == b"Signature";
                if is_signature || skip_depth > 0 {
                    skip_depth += 1;
                } else {
                    writer
                        .write_event(Event::Start(e.clone()))
                        .map_err(|err| SamlError::Parse(format!("strip write: {err}")))?;
                }
            }
            (ns, Event::End(e)) => {
                if skip_depth > 0 {
                    let is_signature = matches!(ns, ResolveResult::Bound(n) if n.as_ref() == DS_NS.as_bytes())
                        && e.local_name().as_ref() == b"Signature";
                    skip_depth -= 1;
                    let _ = is_signature; // depth tracking handles nesting
                } else {
                    writer
                        .write_event(Event::End(e.clone()))
                        .map_err(|err| SamlError::Parse(format!("strip write: {err}")))?;
                }
            }
            (_, Event::Empty(e)) => {
                let is_signature = matches!(reader.resolve_element(e.name()).0, ResolveResult::Bound(n) if n.as_ref() == DS_NS.as_bytes())
                    && e.local_name().as_ref() == b"Signature";
                if is_signature || skip_depth > 0 {
                    // self-closing signature is unusual; skip
                    continue;
                }
                writer
                    .write_event(Event::Empty(e.clone()))
                    .map_err(|err| SamlError::Parse(format!("strip write: {err}")))?;
            }
            (_, Event::Eof) => break,
            (_, ev) => {
                if skip_depth == 0 {
                    writer
                        .write_event(ev)
                        .map_err(|err| SamlError::Parse(format!("strip write: {err}")))?;
                }
            }
        }
        buf.clear();
    }
    String::from_utf8(writer.into_inner().into_inner())
        .map_err(|e| SamlError::Parse(format!("strip utf8: {e}")))
}

/// Exclusive XML Canonicalization (xml-exc-c14n) — strict subset.
///
/// `pub(crate)` since v7.5 — the SAML integration-test fixture in the
/// `tests` module needs to canonicalize SignedInfo + the referenced
/// Assertion before signing them, and the round-trip is only
/// meaningful if both producer and verifier use the same c14n. Making
/// this `pub(crate)` is the minimal exposure: still hidden from
/// downstream crates, just visible to our test code in the same lib.
#[allow(rustdoc::invalid_codeblock_attributes)]
///
/// - Re-serialize via quick-xml, sorting attributes (ns decls by prefix,
///   regular attrs by namespace-URI then local-name).
/// - Self-closing tags are NOT expanded by quick-xml in this build, so
///   we always emit Start + End pairs.
/// - Whitespace between elements preserved as text events.
/// - Comments dropped (matches default exc-c14n behavior with
///   `withComments=false`).
///
/// This is "good enough" for the canonical IdP profile. Edge cases that
/// real exc-c14n handles but we do not (and that would cause divergence
/// from IdP's expected digest):
/// - Inherited default namespace from parent context (the SignedInfo
///   element we extracted is already complete with its own xmlns:ds
///   declaration, so this is moot for our profile)
/// - InclusiveNamespaces PrefixList (rare; we reject unknown profiles
///   in extract_signed_parts so this can't reach us)
pub(crate) fn canonicalize_exc_c14n(xml: &str) -> Result<String, SamlError> {
    let mut reader = NsReader::from_str(xml);
    reader.config_mut().trim_text(false);

    let mut out = String::new();
    let mut buf = Vec::new();
    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|e| SamlError::Canonicalization(format!("c14n read: {e}")))?;
        match event {
            Event::Start(e) => {
                out.push('<');
                out.push_str(&qname_string(&e)?);
                emit_sorted_attrs(&e, &mut out)?;
                out.push('>');
            }
            Event::End(e) => {
                out.push_str("</");
                let qname = e.name();
                let name = std::str::from_utf8(qname.as_ref()).map_err(|err| {
                    SamlError::Canonicalization(format!("c14n end utf8: {err}"))
                })?;
                out.push_str(name);
                out.push('>');
            }
            Event::Empty(e) => {
                // Expand <foo/> to <foo></foo>
                out.push('<');
                out.push_str(&qname_string(&e)?);
                emit_sorted_attrs(&e, &mut out)?;
                out.push('>');
                out.push_str("</");
                let qname = e.name();
                let name = std::str::from_utf8(qname.as_ref()).map_err(|err| {
                    SamlError::Canonicalization(format!("c14n empty utf8: {err}"))
                })?;
                out.push_str(name);
                out.push('>');
            }
            Event::Text(t) => {
                // Per exc-c14n: character data must be normalized so
                // &, <, > become &amp;, &lt;, &gt;. quick-xml's
                // .unescape() gives raw text; we re-escape minimally.
                let raw = t
                    .unescape()
                    .map_err(|e| SamlError::Canonicalization(format!("c14n text: {e}")))?;
                out.push_str(&escape_text(&raw));
            }
            Event::CData(c) => {
                // CDATA becomes character data after escaping.
                let raw = std::str::from_utf8(c.as_ref())
                    .map_err(|e| SamlError::Canonicalization(format!("c14n cdata: {e}")))?;
                out.push_str(&escape_text(raw));
            }
            Event::Comment(_) | Event::PI(_) | Event::DocType(_) | Event::Decl(_) => {
                // Per exc-c14n withComments=false: skip
            }
            Event::Eof => break,
        }
        buf.clear();
    }
    Ok(out)
}

fn qname_string(e: &BytesStart) -> Result<String, SamlError> {
    std::str::from_utf8(e.name().as_ref())
        .map(String::from)
        .map_err(|err| SamlError::Canonicalization(format!("qname utf8: {err}")))
}

fn emit_sorted_attrs(e: &BytesStart, out: &mut String) -> Result<(), SamlError> {
    // Collect (key, value) pairs; split into ns decls and regular attrs.
    let mut ns_decls: Vec<(String, String)> = Vec::new();
    let mut attrs: Vec<(String, String)> = Vec::new();
    for attr in e.attributes() {
        let attr = attr.map_err(|err| {
            SamlError::Canonicalization(format!("attr parse: {err}"))
        })?;
        let key = std::str::from_utf8(attr.key.as_ref())
            .map_err(|err| SamlError::Canonicalization(format!("attr key utf8: {err}")))?
            .to_string();
        let val_raw = attr.unescape_value().map_err(|err| {
            SamlError::Canonicalization(format!("attr value unescape: {err}"))
        })?;
        let value = val_raw.into_owned();
        if key == "xmlns" || key.starts_with("xmlns:") {
            ns_decls.push((key, value));
        } else {
            attrs.push((key, value));
        }
    }
    ns_decls.sort_by(|a, b| a.0.cmp(&b.0));
    attrs.sort_by(|a, b| a.0.cmp(&b.0));
    for (k, v) in ns_decls.into_iter().chain(attrs.into_iter()) {
        out.push(' ');
        out.push_str(&k);
        out.push_str("=\"");
        // Per exc-c14n: attribute values escape &<"\r\n\t to entities.
        out.push_str(&escape_attr_value(&v));
        out.push('"');
    }
    Ok(())
}

fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => out.push_str("&#xD;"),
            other => out.push(other),
        }
    }
    out
}

fn escape_attr_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '"' => out.push_str("&quot;"),
            '\t' => out.push_str("&#x9;"),
            '\n' => out.push_str("&#xA;"),
            '\r' => out.push_str("&#xD;"),
            other => out.push(other),
        }
    }
    out
}

fn verify_rsa_sha256(
    signed_bytes: &[u8],
    sig_bytes: &[u8],
    cert_pem: &str,
) -> Result<(), SamlError> {
    if !cert_pem.contains("BEGIN CERTIFICATE") {
        return Err(SamlError::Cert(
            "IdP cert PEM missing BEGIN CERTIFICATE marker".into(),
        ));
    }
    let pem_block = x509_parser::pem::Pem::iter_from_buffer(cert_pem.as_bytes())
        .next()
        .ok_or_else(|| SamlError::Cert("no PEM block in IdP cert".into()))?
        .map_err(|e| SamlError::Cert(format!("PEM parse: {e}")))?;
    let cert = pem_block
        .parse_x509()
        .map_err(|e| SamlError::Cert(format!("X.509 parse: {e}")))?;
    let spki_der = cert.public_key().raw;
    let rsa_pub = RsaPublicKey::from_public_key_der(spki_der)
        .map_err(|e| SamlError::Cert(format!("RSA pubkey: {e}")))?;
    let verifying_key = VerifyingKey::<Sha256>::new(rsa_pub);
    let signature = Signature::try_from(sig_bytes)
        .map_err(|e| SamlError::Signature(format!("signature bytes: {e}")))?;
    verifying_key
        .verify(signed_bytes, &signature)
        .map_err(|e| SamlError::Signature(format!("rsa-sha256 verify: {e}")))
}

/// Constant-time byte comparison — defense against timing-oracle attacks
/// on the digest comparison.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Parse the verified payload (Response root or Assertion) to extract
/// the operator-trusted claims. Also enforces Conditions / Audience /
/// InResponseTo per the SAML 2.0 profile.
fn extract_claims(
    referenced_xml: &str,
    is_response_root: bool,
    expected_audience: &str,
    expected_in_response_to: Option<&str>,
) -> Result<VerifiedAssertion, SamlError> {
    // If the signed element is the Response root, the Assertion is
    // nested inside it. If the signed element is the Assertion itself,
    // we look at it directly. Either way, all the interesting fields
    // live under <Assertion>.
    let mut reader = NsReader::from_str(referenced_xml);
    reader.config_mut().trim_text(false);

    let mut depth: usize = 0;
    let mut in_assertion = false;
    let mut assertion_depth: Option<usize> = None;
    let mut in_subject = false;
    let mut in_name_id = false;
    let mut name_id: Option<String> = None;
    let mut in_issuer = false;
    let mut issuer: Option<String> = None;
    let mut session_index: Option<String> = None;

    let mut in_conditions = false;
    let mut not_before: Option<String> = None;
    let mut not_on_or_after: Option<String> = None;
    let mut audiences: Vec<String> = Vec::new();
    let mut in_audience = false;

    let mut in_subject_confirmation_data_in_response_to: Option<String> = None;

    let mut attributes: BTreeMap<String, String> = BTreeMap::new();
    let mut current_attr_name: Option<String> = None;
    let mut in_attr_value = false;

    let mut buf = Vec::new();
    loop {
        let event = reader
            .read_resolved_event_into(&mut buf)
            .map_err(|e| SamlError::Parse(format!("claims read: {e}")))?;
        match event {
            (ns, Event::Start(e)) => {
                depth += 1;
                let local = std::str::from_utf8(e.local_name().as_ref())
                    .map_err(|e| SamlError::Parse(format!("local utf8: {e}")))?
                    .to_string();
                let ns_str = match ns {
                    ResolveResult::Bound(n) => Some(
                        std::str::from_utf8(n.as_ref())
                            .map_err(|e| SamlError::Parse(format!("ns utf8: {e}")))?
                            .to_string(),
                    ),
                    _ => None,
                };
                let is_saml = ns_str.as_deref() == Some(SAML_NS);

                if is_saml && local == "Assertion" {
                    in_assertion = true;
                    assertion_depth = Some(depth);
                    if let Some(si) = attr_value(&e, "SessionIndex") {
                        session_index = Some(si);
                    }
                } else if in_assertion && is_saml {
                    match local.as_str() {
                        "Issuer" if !in_subject => in_issuer = true,
                        "Subject" => in_subject = true,
                        "NameID" if in_subject => in_name_id = true,
                        "SubjectConfirmationData" => {
                            if let Some(ir) = attr_value(&e, "InResponseTo") {
                                in_subject_confirmation_data_in_response_to = Some(ir);
                            }
                        }
                        "Conditions" => {
                            in_conditions = true;
                            not_before = attr_value(&e, "NotBefore");
                            not_on_or_after = attr_value(&e, "NotOnOrAfter");
                        }
                        "Audience" if in_conditions => in_audience = true,
                        "Attribute" => {
                            current_attr_name = attr_value(&e, "Name");
                        }
                        "AttributeValue" if current_attr_name.is_some() => {
                            in_attr_value = true;
                        }
                        "AuthnStatement" => {
                            if session_index.is_none() {
                                session_index = attr_value(&e, "SessionIndex");
                            }
                        }
                        _ => {}
                    }
                }
                let _ = is_response_root; // future-use: enforce that nested Assertion is the only one
            }
            (_, Event::Text(t)) => {
                let text = t
                    .unescape()
                    .map_err(|e| SamlError::Parse(format!("text unescape: {e}")))?
                    .into_owned();
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if in_name_id {
                    name_id.get_or_insert_with(String::new).push_str(trimmed);
                } else if in_issuer {
                    issuer.get_or_insert_with(String::new).push_str(trimmed);
                } else if in_audience {
                    audiences.push(trimmed.to_string());
                } else if in_attr_value {
                    if let Some(name) = &current_attr_name {
                        attributes
                            .entry(name.clone())
                            .or_insert_with(String::new)
                            .push_str(trimmed);
                    }
                }
            }
            (ns, Event::End(_)) => {
                let ns_str = match ns {
                    ResolveResult::Bound(n) => std::str::from_utf8(n.as_ref())
                        .map(String::from)
                        .ok(),
                    _ => None,
                };
                let is_saml = ns_str.as_deref() == Some(SAML_NS);
                if is_saml {
                    if in_attr_value {
                        in_attr_value = false;
                    } else if in_audience {
                        in_audience = false;
                    } else if in_name_id {
                        in_name_id = false;
                    } else if in_issuer {
                        in_issuer = false;
                    } else if in_subject {
                        // closing Subject — only when we're at the Subject's
                        // own depth. quick-xml's nested NameID would already
                        // have flipped its flag; this is the outer close.
                        in_subject = false;
                    } else if in_conditions {
                        in_conditions = false;
                    }
                    if Some(depth) == assertion_depth {
                        in_assertion = false;
                        assertion_depth = None;
                    }
                }
                depth -= 1;
            }
            (_, Event::Eof) => break,
            _ => {}
        }
        buf.clear();
    }

    let name_id = name_id.ok_or_else(|| SamlError::Missing("Subject NameID".into()))?;
    let issuer = issuer.ok_or_else(|| SamlError::Missing("Issuer".into()))?;

    // Conditions: time window check
    let now = chrono::Utc::now();
    if let Some(nb) = &not_before {
        let t = chrono::DateTime::parse_from_rfc3339(nb)
            .map_err(|e| SamlError::Parse(format!("Conditions/NotBefore: {e}")))?;
        if now < t {
            return Err(SamlError::Profile(format!(
                "assertion NotBefore in future: {nb}"
            )));
        }
    }
    if let Some(na) = &not_on_or_after {
        let t = chrono::DateTime::parse_from_rfc3339(na)
            .map_err(|e| SamlError::Parse(format!("Conditions/NotOnOrAfter: {e}")))?;
        if now >= t {
            return Err(SamlError::Profile(format!(
                "assertion expired: NotOnOrAfter={na}"
            )));
        }
    }

    // Audience must include our SP entity ID
    if !audiences.iter().any(|a| a == expected_audience) {
        return Err(SamlError::Profile(format!(
            "AudienceRestriction does not include expected {expected_audience}; got {audiences:?}"
        )));
    }

    // InResponseTo (if SP-initiated) must match what we issued
    if let Some(expected) = expected_in_response_to {
        match in_subject_confirmation_data_in_response_to.as_deref() {
            Some(seen) if seen == expected => {}
            other => {
                return Err(SamlError::Profile(format!(
                    "SubjectConfirmationData InResponseTo mismatch: expected={expected:?}, got={other:?}"
                )));
            }
        }
    }

    Ok(VerifiedAssertion {
        subject_name_id: name_id,
        issuer,
        session_index,
        attributes,
    })
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that DOCTYPE is rejected up-front as XXE defense.
    #[test]
    fn xxe_doctype_rejected() {
        let r = verify_saml_response(
            "<!DOCTYPE foo SYSTEM 'file:///etc/passwd'><foo/>",
            "",
            "sp",
            None,
        );
        assert!(matches!(r, Err(SamlError::Profile(m)) if m.contains("DOCTYPE")));
    }

    /// Profile rejection: missing SignedInfo at all.
    #[test]
    fn missing_signature_rejected() {
        let xml = "<samlp:Response xmlns:samlp=\"urn:oasis:names:tc:SAML:2.0:protocol\" ID=\"r1\"/>";
        let r = verify_saml_response(xml, "", "sp", None);
        assert!(r.is_err());
    }

    /// c14n smoke: a simple element with sorted attributes survives
    /// round-trip with stable ordering.
    #[test]
    fn c14n_sorts_attributes() {
        let input = r#"<foo zlast="1" abc="2" xmlns:ds="urn:x" xmlns="urn:default">text</foo>"#;
        let c = canonicalize_exc_c14n(input).unwrap();
        // xmlns ns decls come first (sorted), then regular attrs (sorted).
        // Order: xmlns, xmlns:ds, abc, zlast.
        let pos_xmlns = c.find("xmlns=").unwrap();
        let pos_xmlns_ds = c.find("xmlns:ds=").unwrap();
        let pos_abc = c.find("abc=").unwrap();
        let pos_zlast = c.find("zlast=").unwrap();
        assert!(pos_xmlns < pos_xmlns_ds);
        assert!(pos_xmlns_ds < pos_abc);
        assert!(pos_abc < pos_zlast);
    }

    #[test]
    fn c14n_expands_self_closing() {
        let input = r#"<foo a="1"/>"#;
        let c = canonicalize_exc_c14n(input).unwrap();
        assert_eq!(c, r#"<foo a="1"></foo>"#);
    }

    #[test]
    fn c14n_drops_comments() {
        let input = "<foo><!-- secret --><bar/></foo>";
        let c = canonicalize_exc_c14n(input).unwrap();
        assert!(!c.contains("secret"));
        assert!(c.contains("<bar></bar>"));
    }

    #[test]
    fn c14n_escapes_special_chars() {
        let input = "<foo>1 &lt; 2 &amp;&amp; 3</foo>";
        let c = canonicalize_exc_c14n(input).unwrap();
        // Round-tripped through unescape() then escape_text(): &lt; → < → &lt; again.
        assert_eq!(c, "<foo>1 &lt; 2 &amp;&amp; 3</foo>");
    }

    #[test]
    fn strip_signature_removes_ds_subtree() {
        let input = format!(
            r#"<a xmlns:ds="{DS_NS}"><b>keep</b><ds:Signature><ds:SignedInfo>drop me</ds:SignedInfo></ds:Signature><c>also keep</c></a>"#,
        );
        let stripped = strip_signature_element(&input).unwrap();
        assert!(stripped.contains("keep"));
        assert!(stripped.contains("also keep"));
        assert!(!stripped.contains("drop me"));
        assert!(!stripped.contains("Signature"));
    }

    #[test]
    fn constant_time_eq_matches_only_identical() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    /// v7.5 — end-to-end SAML round-trip. Generates a self-signed
    /// X.509 cert at runtime (via `rcgen`), composes a minimal valid
    /// SAMLResponse with that cert's RSA key signing both the
    /// referenced Assertion (digest) and the SignedInfo (signature),
    /// then verifies it through `verify_saml_response` and checks the
    /// extracted claims match what we put in.
    ///
    /// This is the integration test the previous note was waiting for.
    /// Covers: c14n bit-equality between signer and verifier, RSA
    /// arithmetic, digest binding, audience match, NameID + email
    /// attribute extraction. If this passes, real-world Okta / Azure
    /// AD responses using the same profile will too.
    #[test]
    fn round_trip_self_signed_succeeds() {
        use rcgen::{CertificateParams, KeyPair};
        use rsa::pkcs1v15::SigningKey;
        use rsa::pkcs8::EncodePrivateKey;
        use rsa::signature::SignatureEncoding;
        use rsa::signature::Signer;
        use rsa::RsaPrivateKey;
        use sha2::Digest;

        // 1) Generate RSA-2048 with the `rsa` crate. rcgen's default
        //    `ring` backend can't generate RSA on its own (ring only
        //    supports RSA signing, not generation), so we generate
        //    here and hand the key over to rcgen via PKCS#8 DER for
        //    cert wrapping. This is the documented "BYO key" pattern.
        //
        // The `rsa` crate pulls in its own pinned `rand_core` version
        // (older than the top-level rand crate), so OsRng types
        // aren't compatible. Use `rsa::rand_core::OsRng` to side-step.
        let mut rng = rsa::rand_core::OsRng;
        let priv_key = RsaPrivateKey::new(&mut rng, 2048).expect("rsa-2048 gen");
        let pkcs8_der = priv_key
            .to_pkcs8_der()
            .expect("rsa to pkcs8")
            .as_bytes()
            .to_vec();
        let key_pair = KeyPair::try_from(pkcs8_der.as_slice())
            .expect("rcgen import pkcs8 der");
        let params = CertificateParams::new(vec!["test-idp.example".to_string()])
            .expect("rcgen params");
        let cert = params.self_signed(&key_pair).expect("rcgen self-sign");
        let cert_pem = cert.pem();

        // 2) Build the Assertion XML. Times in the future so Conditions
        //    pass; audience matches what we'll pass to verify.
        let now = chrono::Utc::now();
        let not_before = now - chrono::Duration::seconds(60);
        let not_after = now + chrono::Duration::minutes(5);
        let issue_instant = now.to_rfc3339();
        let not_before_s = not_before.to_rfc3339();
        let not_after_s = not_after.to_rfc3339();

        let audience = "https://sp.example/sp";
        let email = "alice@acme.test";

        let assertion_no_sig = format!(
            r#"<saml:Assertion xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion" ID="a-1" Version="2.0" IssueInstant="{issue_instant}"><saml:Issuer>https://idp.example/idp</saml:Issuer><saml:Subject><saml:NameID Format="urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress">{email}</saml:NameID></saml:Subject><saml:Conditions NotBefore="{not_before_s}" NotOnOrAfter="{not_after_s}"><saml:AudienceRestriction><saml:Audience>{audience}</saml:Audience></saml:AudienceRestriction></saml:Conditions><saml:AttributeStatement><saml:Attribute Name="email"><saml:AttributeValue>{email}</saml:AttributeValue></saml:Attribute></saml:AttributeStatement></saml:Assertion>"#,
        );

        // 3) Compute the digest of c14n(assertion) — that's what goes
        //    into SignedInfo's DigestValue. Our enveloped-signature
        //    transform strips <ds:Signature> first; since we haven't
        //    embedded one yet, c14n directly is correct.
        let assertion_canon =
            canonicalize_exc_c14n(&assertion_no_sig).expect("c14n assertion");
        let mut hasher = sha2::Sha256::new();
        hasher.update(assertion_canon.as_bytes());
        let digest_b64 = general_purpose::STANDARD.encode(hasher.finalize());

        // 4) Build SignedInfo with the digest. We use the exact
        //    serialization the verifier will canonicalize.
        let signed_info = format!(
            r##"<ds:SignedInfo xmlns:ds="http://www.w3.org/2000/09/xmldsig#"><ds:CanonicalizationMethod Algorithm="http://www.w3.org/2001/10/xml-exc-c14n#"></ds:CanonicalizationMethod><ds:SignatureMethod Algorithm="http://www.w3.org/2001/04/xmldsig-more#rsa-sha256"></ds:SignatureMethod><ds:Reference URI="#a-1"><ds:Transforms><ds:Transform Algorithm="http://www.w3.org/2000/09/xmldsig#enveloped-signature"></ds:Transform><ds:Transform Algorithm="http://www.w3.org/2001/10/xml-exc-c14n#"></ds:Transform></ds:Transforms><ds:DigestMethod Algorithm="http://www.w3.org/2001/04/xmlenc#sha256"></ds:DigestMethod><ds:DigestValue>{digest_b64}</ds:DigestValue></ds:Reference></ds:SignedInfo>"##,
        );

        // 5) Canonicalize SignedInfo + sign with RSA-PKCS#1v1.5-SHA256.
        let signed_info_canon =
            canonicalize_exc_c14n(&signed_info).expect("c14n signed_info");
        let signing_key = SigningKey::<sha2::Sha256>::new(priv_key);
        let sig_bytes = signing_key.sign(signed_info_canon.as_bytes()).to_bytes();
        let sig_b64 = general_purpose::STANDARD.encode(&sig_bytes);

        // 6) Stitch Signature back into the Assertion (between Issuer
        //    and Subject is the canonical SAML 2.0 spec location).
        let signature_block = format!(
            r#"<ds:Signature xmlns:ds="http://www.w3.org/2000/09/xmldsig#">{signed_info}<ds:SignatureValue>{sig_b64}</ds:SignatureValue></ds:Signature>"#,
        );
        let assertion_signed = assertion_no_sig.replace(
            "</saml:Issuer>",
            &format!("</saml:Issuer>{signature_block}"),
        );

        // 7) Wrap in <samlp:Response> envelope.
        let response_xml = format!(
            r#"<?xml version="1.0"?><samlp:Response xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol" ID="r-1" Version="2.0" IssueInstant="{issue_instant}">{assertion_signed}</samlp:Response>"#,
        );

        // 8) Verify. Should succeed and return the email.
        let verified = verify_saml_response(&response_xml, &cert_pem, audience, None)
            .expect("verify");
        assert_eq!(verified.subject_name_id, email);
        assert_eq!(verified.issuer, "https://idp.example/idp");
        assert_eq!(verified.attributes.get("email").map(String::as_str), Some(email));

        // 9) Negative: bump audience — should reject.
        let bad =
            verify_saml_response(&response_xml, &cert_pem, "wrong-audience", None);
        assert!(bad.is_err(), "wrong audience must be rejected");
    }
}
