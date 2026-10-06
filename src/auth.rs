use std::collections::{BTreeMap, BTreeSet};

use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, Method};
use jsonwebtoken::jwk::{AlgorithmParameters, Jwk, KeyAlgorithm, PublicKeyUse};
use jsonwebtoken::{Algorithm, DecodingKey, Header, TokenData, Validation};
use serde::Deserialize;

use crate::config::AuthConfig;

const LEEWAY_SECONDS: u64 = 60;
const BEARER_SCHEME: &str = "bearer";
const ACCEPTED_TYPES: [&str; 2] = ["at+jwt", "application/at+jwt"];

const SCOPE_TABLE: [(&str, &str, &str); 21] = [
    ("GET", "/v1/calendars", "calendar.read"),
    ("GET", "/v1/events", "calendar.read"),
    ("GET", "/v1/events/", "calendar.read"),
    ("GET", "/v1/events/{id}", "calendar.read"),
    ("GET", "/v1/free", "calendar.read"),
    ("POST", "/v1/events", "calendar.write"),
    ("PATCH", "/v1/events/{id}", "calendar.write"),
    ("DELETE", "/v1/events/{id}", "calendar.write"),
    ("PATCH", "/v1/events/", "calendar.write"),
    ("DELETE", "/v1/events/", "calendar.write"),
    ("GET", "/v1/lists", "reminders.read"),
    ("GET", "/v1/places", "reminders.read"),
    ("GET", "/v1/reminders", "reminders.read"),
    ("GET", "/v1/reminders/{id}", "reminders.read"),
    ("POST", "/v1/reminders", "reminders.write"),
    ("PATCH", "/v1/reminders/{id}", "reminders.write"),
    ("DELETE", "/v1/reminders/{id}", "reminders.write"),
    ("GET", "/v1/mail/accounts", "mail.read"),
    ("GET", "/v1/mail/messages", "mail.read"),
    ("GET", "/v1/mail/messages/{id}", "mail.read"),
    ("PATCH", "/v1/mail/messages/{id}", "mail.junk"),
];

/// Why a request's bearer token was refused; carries only the kind, never a claim value or the token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// The request has no `Authorization` header.
    #[error("missing")]
    Missing,
    /// The header or the token could not be parsed.
    #[error("malformed")]
    Malformed,
    /// The token names a signing key the key set does not hold.
    #[error("unknown key")]
    UnknownKey,
    /// The token parsed but failed a check: algorithm, type, signature, lifetime, issuer, audience or subject.
    #[error("invalid")]
    Invalid,
}

/// The client a valid token was issued to and the scopes it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// The token's `client_id`, else its `sub`.
    pub client: String,
    /// The union of the token's `scp` array and `scope` string.
    pub scopes: BTreeSet<String>,
}

impl Principal {
    /// Whether the token carries `scope`.
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.contains(scope)
    }
}

/// The provider's RS256 signing keys by `kid`.
#[derive(Clone, Default)]
pub struct KeySet {
    keys: BTreeMap<String, DecodingKey>,
}

impl KeySet {
    /// Keeps the RSA keys with a `kid`, `alg` absent or `RS256` and `use` absent or `sig`; drops the rest.
    pub fn from_jwks(jwks: &[Jwk]) -> Self {
        let mut keys = BTreeMap::new();
        for jwk in jwks {
            let Some((kid, key)) = usable_key(jwk) else {
                continue;
            };
            keys.insert(kid, key);
        }
        Self { keys }
    }

    /// The number of usable keys.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether no usable key is held.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    fn get(&self, kid: &str) -> Option<&DecodingKey> {
        self.keys.get(kid)
    }
}

fn usable_key(jwk: &Jwk) -> Option<(String, DecodingKey)> {
    let Jwk { common, algorithm } = jwk;
    let AlgorithmParameters::RSA(params) = algorithm else {
        return None;
    };
    let rs256 = match common.key_algorithm {
        None => true,
        Some(alg) => alg == KeyAlgorithm::RS256,
    };
    let signing = match &common.public_key_use {
        None => true,
        Some(PublicKeyUse::Signature) => true,
        Some(PublicKeyUse::Encryption) => false,
        Some(PublicKeyUse::Other(_)) => false,
    };
    if !rs256 || !signing {
        return None;
    }
    let kid = common.key_id.clone()?;
    if kid.is_empty() {
        return None;
    }
    let key = DecodingKey::from_rsa_components(&params.n, &params.e).ok()?;
    Some((kid, key))
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T: PartialEq> OneOrMany<T> {
    fn contains(&self, wanted: &T) -> bool {
        match self {
            Self::One(value) => value == wanted,
            Self::Many(values) => values.contains(wanted),
        }
    }
}

#[derive(Deserialize)]
struct Claims {
    iss: String,
    sub: Option<String>,
    client_id: Option<String>,
    aud: Option<OneOrMany<String>>,
    scp: Option<Vec<String>>,
    scope: Option<String>,
}

/// Validates bearer tokens against one issuer, audience and scope prefix.
pub struct Validator {
    issuer: String,
    audience: String,
    scope_prefix: String,
    validation: Validation,
}

impl Validator {
    /// Takes the issuer, audience and scope prefix from the `[auth]` table.
    pub fn new(config: &AuthConfig) -> Self {
        let AuthConfig {
            issuer,
            audience,
            jwks_url: _,
            required: _,
            scope_prefix,
        } = config;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.leeway = LEEWAY_SECONDS;
        validation.validate_exp = true;
        validation.validate_nbf = true;
        validation.validate_aud = false;
        validation.set_required_spec_claims(&["exp"]);
        Self {
            issuer: issuer.clone(),
            audience: audience.clone(),
            scope_prefix: scope_prefix.clone(),
            validation,
        }
    }

    /// Reads the bearer token from `headers` and validates it against `keys`.
    pub fn validate(&self, headers: &HeaderMap, keys: &KeySet) -> Result<Principal, AuthError> {
        let token = bearer_token(headers)?;
        let kid = key_id(token)?;
        let Some(key) = keys.get(&kid) else {
            return Err(AuthError::UnknownKey);
        };
        self.verify(token, key)
    }

    /// The scope a token needs for `method` on the route template `route`, `None` when the pair has no row.
    pub fn required_scope(&self, method: &Method, route: &str) -> Option<String> {
        let name = scope_for(method, route)?;
        Some(format!("{}{name}", self.scope_prefix))
    }

    fn verify(&self, token: &str, key: &DecodingKey) -> Result<Principal, AuthError> {
        let Ok(data) = jsonwebtoken::decode::<Claims>(token, key, &self.validation) else {
            return Err(AuthError::Invalid);
        };
        let TokenData { header: _, claims } = data;
        let Claims {
            iss,
            sub,
            client_id,
            aud,
            scp,
            scope,
        } = claims;
        if iss != self.issuer {
            return Err(AuthError::Invalid);
        }
        let Some(aud) = aud else {
            return Err(AuthError::Invalid);
        };
        if !aud.contains(&self.audience) {
            return Err(AuthError::Invalid);
        }
        let mut client = client_id.unwrap_or_default();
        if client.is_empty() {
            client = sub.unwrap_or_default();
        }
        if client.is_empty() {
            return Err(AuthError::Invalid);
        }
        let mut scopes = BTreeSet::new();
        for name in scp.unwrap_or_default() {
            if !name.is_empty() {
                scopes.insert(name);
            }
        }
        for name in scope.as_deref().unwrap_or_default().split(' ') {
            if !name.is_empty() {
                scopes.insert(name.to_owned());
            }
        }
        Ok(Principal { client, scopes })
    }
}

/// The scope name `method` on the route template `route` needs, before the prefix; `None` when the pair has no row.
pub fn scope_for(method: &Method, route: &str) -> Option<&'static str> {
    for (row_method, row_route, name) in SCOPE_TABLE {
        if row_method == method.as_str() && row_route == route {
            return Some(name);
        }
    }
    None
}

fn bearer_token(headers: &HeaderMap) -> Result<&str, AuthError> {
    let mut values = headers.get_all(AUTHORIZATION).iter();
    let Some(value) = values.next() else {
        return Err(AuthError::Missing);
    };
    if values.next().is_some() {
        return Err(AuthError::Malformed);
    }
    let Ok(value) = value.to_str() else {
        return Err(AuthError::Malformed);
    };
    let Some((scheme, token)) = value.split_once(' ') else {
        return Err(AuthError::Malformed);
    };
    if !scheme.eq_ignore_ascii_case(BEARER_SCHEME) {
        return Err(AuthError::Malformed);
    }
    let token = token.trim();
    if token.is_empty() {
        return Err(AuthError::Malformed);
    }
    Ok(token)
}

fn key_id(token: &str) -> Result<String, AuthError> {
    let Ok(header) = jsonwebtoken::decode_header(token) else {
        return Err(AuthError::Malformed);
    };
    let Header { alg, kid, typ, .. } = header;
    if alg != Algorithm::RS256 {
        return Err(AuthError::Invalid);
    }
    let Some(kid) = kid else {
        return Err(AuthError::Invalid);
    };
    if kid.is_empty() {
        return Err(AuthError::Invalid);
    }
    let Some(typ) = typ else {
        return Err(AuthError::Invalid);
    };
    let mut accepted = false;
    for name in ACCEPTED_TYPES {
        if typ.eq_ignore_ascii_case(name) {
            accepted = true;
            break;
        }
    }
    if !accepted {
        return Err(AuthError::Invalid);
    }
    Ok(kid)
}

#[cfg(test)]
pub(crate) mod test_keys {
    use std::sync::LazyLock;

    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use jsonwebtoken::jwk::Jwk;
    use jsonwebtoken::{Algorithm, EncodingKey, Header};
    use rsa::pkcs1::{EncodeRsaPrivateKey, EncodeRsaPublicKey};
    use rsa::traits::PublicKeyParts;
    use rsa::{RsaPrivateKey, RsaPublicKey};
    use serde_json::{Value, json};

    const KEY_BITS: usize = 2048;

    /// The type every minted token carries unless a test overrides it.
    pub(crate) const TOKEN_TYPE: &str = "at+jwt";

    /// An RSA key pair generated once per test binary; never committed.
    pub(crate) struct TestKey {
        public: RsaPublicKey,
        encoding: EncodingKey,
    }

    impl TestKey {
        fn generate() -> Self {
            let private = RsaPrivateKey::new(&mut rand::thread_rng(), KEY_BITS).unwrap();
            let der = private.to_pkcs1_der().unwrap();
            Self {
                public: private.to_public_key(),
                encoding: EncodingKey::from_rsa_der(der.as_bytes()),
            }
        }

        /// The public key as an RS256 signing JWK named `kid`.
        pub(crate) fn jwk(&self, kid: &str) -> Jwk {
            serde_json::from_value(self.jwk_json(kid)).unwrap()
        }

        /// The public key as JWK JSON, for tests that edit a member.
        pub(crate) fn jwk_json(&self, kid: &str) -> Value {
            json!({
                "kty": "RSA",
                "kid": kid,
                "alg": "RS256",
                "use": "sig",
                "n": URL_SAFE_NO_PAD.encode(self.public.n().to_bytes_be()),
                "e": URL_SAFE_NO_PAD.encode(self.public.e().to_bytes_be()),
            })
        }

        /// Signs `claims` under `header` with the private key.
        pub(crate) fn sign(&self, header: &Header, claims: &Value) -> String {
            jsonwebtoken::encode(header, claims, &self.encoding).unwrap()
        }

        fn public_der(&self) -> Vec<u8> {
            self.public.to_pkcs1_der().unwrap().into_vec()
        }
    }

    /// The key the tests' key sets trust.
    pub(crate) static KEY: LazyLock<TestKey> = LazyLock::new(TestKey::generate);
    /// A second key pair no key set trusts.
    pub(crate) static OTHER_KEY: LazyLock<TestKey> = LazyLock::new(TestKey::generate);

    /// An RS256 `at+jwt` header naming `kid`.
    pub(crate) fn header(kid: &str) -> Header {
        let mut header = Header::new(Algorithm::RS256);
        header.typ = Some(TOKEN_TYPE.to_owned());
        header.kid = Some(kid.to_owned());
        header
    }

    /// The trusted key as a JWK named `kid`.
    pub(crate) fn jwk(kid: &str) -> Jwk {
        KEY.jwk(kid)
    }

    /// `claims` signed by the trusted key with an RS256 `at+jwt` header naming `kid`.
    pub(crate) fn mint(claims: &Value, kid: &str) -> String {
        KEY.sign(&header(kid), claims)
    }

    /// `claims` signed with HS256, the trusted public key's DER bytes as the secret.
    pub(crate) fn mint_hs256(claims: &Value, kid: &str) -> String {
        let mut header = Header::new(Algorithm::HS256);
        header.typ = Some(TOKEN_TYPE.to_owned());
        header.kid = Some(kid.to_owned());
        let secret = EncodingKey::from_secret(&KEY.public_der());
        jsonwebtoken::encode(&header, claims, &secret).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use serde_json::{Value, json};
    use url::Url;

    use super::test_keys::{KEY, OTHER_KEY, header, jwk, mint, mint_hs256};
    use super::*;

    const ISSUER: &str = "https://auth.example.com";
    const AUDIENCE: &str = "https://eventkit-bridge";
    const KID: &str = "key-1";
    const CLIENT: &str = "agent";
    const SUBJECT: &str = "subject-7";
    const JTI: &str = "jti-secret-value";

    fn config() -> AuthConfig {
        AuthConfig {
            issuer: ISSUER.to_owned(),
            audience: AUDIENCE.to_owned(),
            jwks_url: Url::parse("https://auth.example.com/jwks.json").unwrap(),
            required: true,
            scope_prefix: "bridge:".to_owned(),
        }
    }

    fn validator() -> Validator {
        Validator::new(&config())
    }

    fn keys() -> KeySet {
        KeySet::from_jwks(&[jwk(KID)])
    }

    fn now() -> i64 {
        i64::try_from(jsonwebtoken::get_current_timestamp()).unwrap()
    }

    fn claims() -> Value {
        let now = now();
        json!({
            "iss": ISSUER,
            "sub": SUBJECT,
            "client_id": CLIENT,
            "aud": [AUDIENCE],
            "exp": now + 900,
            "iat": now,
            "nbf": now,
            "jti": JTI,
            "scp": ["bridge:calendar.read", "bridge:mail.read"],
        })
    }

    fn with(mut claims: Value, key: &str, value: Value) -> Value {
        claims[key] = value;
        claims
    }

    fn without(mut claims: Value, key: &str) -> Value {
        claims.as_object_mut().unwrap().remove(key);
        claims
    }

    fn bearer(token: &str) -> HeaderMap {
        authorization(&format!("Bearer {token}"))
    }

    fn authorization(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        headers
    }

    fn check(token: &str) -> Result<Principal, AuthError> {
        validator().validate(&bearer(token), &keys())
    }

    fn scopes(names: &[&str]) -> BTreeSet<String> {
        let mut set = BTreeSet::new();
        for name in names {
            set.insert((*name).to_owned());
        }
        set
    }

    fn unsigned(header: &Value, claims: &Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(header.to_string());
        let claims = URL_SAFE_NO_PAD.encode(claims.to_string());
        format!("{header}.{claims}.")
    }

    #[test]
    fn valid_token_with_scp_array() {
        let principal = check(&mint(&claims(), KID)).unwrap();

        assert_eq!(
            principal,
            Principal {
                client: CLIENT.to_owned(),
                scopes: scopes(&["bridge:calendar.read", "bridge:mail.read"]),
            }
        );
        assert!(principal.has_scope("bridge:mail.read"));
        assert!(!principal.has_scope("bridge:mail.junk"));
    }

    #[test]
    fn valid_token_with_scope_string() {
        let claims = without(claims(), "scp");
        let claims = with(claims, "scope", json!("bridge:lists bridge:reminders.read"));

        let principal = check(&mint(&claims, KID)).unwrap();

        assert_eq!(
            principal.scopes,
            scopes(&["bridge:lists", "bridge:reminders.read"])
        );
    }

    #[test]
    fn scp_and_scope_are_unioned() {
        let claims = with(
            claims(),
            "scope",
            json!("bridge:calendar.read  bridge:calendar.write"),
        );

        let principal = check(&mint(&claims, KID)).unwrap();

        assert_eq!(
            principal.scopes,
            scopes(&[
                "bridge:calendar.read",
                "bridge:calendar.write",
                "bridge:mail.read"
            ])
        );
    }

    #[test]
    fn token_without_scopes_is_valid_with_none() {
        let claims = without(claims(), "scp");

        let principal = check(&mint(&claims, KID)).unwrap();

        assert!(principal.scopes.is_empty());
    }

    #[test]
    fn missing_header_is_missing() {
        let result = validator().validate(&HeaderMap::new(), &keys());

        assert_eq!(result, Err(AuthError::Missing));
    }

    #[test]
    fn basic_scheme_is_malformed() {
        let result = validator().validate(&authorization("Basic dXNlcjpwYXNz"), &keys());

        assert_eq!(result, Err(AuthError::Malformed));
    }

    #[test]
    fn scheme_is_case_insensitive() {
        let token = mint(&claims(), KID);

        let result = validator().validate(&authorization(&format!("bEaReR {token}")), &keys());

        assert_eq!(result.unwrap().client, CLIENT);
    }

    #[test]
    fn two_headers_are_malformed() {
        let token = mint(&claims(), KID);
        let mut headers = bearer(&token);
        headers.append(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );

        let result = validator().validate(&headers, &keys());

        assert_eq!(result, Err(AuthError::Malformed));
    }

    #[test]
    fn empty_token_is_malformed() {
        for value in ["Bearer", "Bearer ", "Bearer    "] {
            let result = validator().validate(&authorization(value), &keys());

            assert_eq!(result, Err(AuthError::Malformed), "{value:?}");
        }
    }

    #[test]
    fn garbage_token_is_malformed() {
        assert_eq!(check("not-a-jwt"), Err(AuthError::Malformed));
    }

    #[test]
    fn wrong_signature_is_invalid() {
        let token = OTHER_KEY.sign(&header(KID), &claims());

        assert_eq!(check(&token), Err(AuthError::Invalid));
    }

    #[test]
    fn unknown_kid_is_unknown_key() {
        assert_eq!(check(&mint(&claims(), "key-2")), Err(AuthError::UnknownKey));
    }

    #[test]
    fn no_kid_is_invalid() {
        let mut header = header(KID);
        header.kid = None;

        assert_eq!(
            check(&KEY.sign(&header, &claims())),
            Err(AuthError::Invalid)
        );
    }

    #[test]
    fn expired_beyond_leeway_is_invalid() {
        let claims = with(claims(), "exp", json!(now() - 120));

        assert_eq!(check(&mint(&claims, KID)), Err(AuthError::Invalid));
    }

    #[test]
    fn expired_within_leeway_is_accepted() {
        let claims = with(claims(), "exp", json!(now() - 30));

        assert!(check(&mint(&claims, KID)).is_ok());
    }

    #[test]
    fn missing_exp_is_invalid() {
        let claims = without(claims(), "exp");

        assert_eq!(check(&mint(&claims, KID)), Err(AuthError::Invalid));
    }

    #[test]
    fn nbf_beyond_leeway_is_invalid() {
        let claims = with(claims(), "nbf", json!(now() + 120));

        assert_eq!(check(&mint(&claims, KID)), Err(AuthError::Invalid));
    }

    #[test]
    fn nbf_within_leeway_is_accepted() {
        let claims = with(claims(), "nbf", json!(now() + 30));

        assert!(check(&mint(&claims, KID)).is_ok());
    }

    #[test]
    fn wrong_issuer_is_invalid() {
        let claims = with(claims(), "iss", json!("https://evil.example.com"));

        assert_eq!(check(&mint(&claims, KID)), Err(AuthError::Invalid));
    }

    #[test]
    fn missing_issuer_is_invalid() {
        let claims = without(claims(), "iss");

        assert_eq!(check(&mint(&claims, KID)), Err(AuthError::Invalid));
    }

    #[test]
    fn missing_aud_is_invalid() {
        let claims = without(claims(), "aud");

        assert_eq!(check(&mint(&claims, KID)), Err(AuthError::Invalid));
    }

    #[test]
    fn aud_without_the_audience_is_invalid() {
        let claims = with(claims(), "aud", json!(["https://other", "bridge"]));

        assert_eq!(check(&mint(&claims, KID)), Err(AuthError::Invalid));
    }

    #[test]
    fn aud_array_containing_the_audience_is_accepted() {
        let claims = with(claims(), "aud", json!(["https://other", AUDIENCE]));

        assert!(check(&mint(&claims, KID)).is_ok());
    }

    #[test]
    fn aud_as_a_plain_matching_string_is_accepted() {
        let claims = with(claims(), "aud", json!(AUDIENCE));

        assert!(check(&mint(&claims, KID)).is_ok());
    }

    #[test]
    fn aud_as_a_plain_other_string_is_invalid() {
        let claims = with(claims(), "aud", json!("https://other"));

        assert_eq!(check(&mint(&claims, KID)), Err(AuthError::Invalid));
    }

    #[test]
    fn alg_none_is_refused() {
        let token = unsigned(
            &json!({"alg": "none", "typ": "at+jwt", "kid": KID}),
            &claims(),
        );

        assert!(check(&token).is_err());
    }

    #[test]
    fn hs256_signed_with_the_public_key_is_invalid() {
        let token = mint_hs256(&claims(), KID);

        assert_eq!(check(&token), Err(AuthError::Invalid));
    }

    #[test]
    fn hs256_never_verifies_against_an_rsa_key() {
        let token = mint_hs256(&claims(), KID);
        let keys = keys();
        let key = keys.get(KID).unwrap();

        assert_eq!(validator().verify(&token, key), Err(AuthError::Invalid));
    }

    #[test]
    fn other_rsa_algorithm_is_invalid() {
        let mut header = header(KID);
        header.alg = Algorithm::RS512;

        assert_eq!(
            check(&KEY.sign(&header, &claims())),
            Err(AuthError::Invalid)
        );
    }

    #[test]
    fn missing_typ_is_invalid() {
        let mut header = header(KID);
        header.typ = None;

        assert_eq!(
            check(&KEY.sign(&header, &claims())),
            Err(AuthError::Invalid)
        );
    }

    #[test]
    fn jwt_typ_is_invalid() {
        let mut header = header(KID);
        header.typ = Some("JWT".to_owned());

        assert_eq!(
            check(&KEY.sign(&header, &claims())),
            Err(AuthError::Invalid)
        );
    }

    #[test]
    fn application_at_jwt_typ_in_any_case_is_accepted() {
        for typ in ["application/AT+JWT", "AT+JWT", "application/at+jwt"] {
            let mut header = header(KID);
            header.typ = Some(typ.to_owned());

            assert!(check(&KEY.sign(&header, &claims())).is_ok(), "{typ}");
        }
    }

    #[test]
    fn neither_sub_nor_client_id_is_invalid() {
        let claims = without(without(claims(), "sub"), "client_id");

        assert_eq!(check(&mint(&claims, KID)), Err(AuthError::Invalid));
    }

    #[test]
    fn empty_sub_and_client_id_are_invalid() {
        let claims = with(with(claims(), "sub", json!("")), "client_id", json!(""));

        assert_eq!(check(&mint(&claims, KID)), Err(AuthError::Invalid));
    }

    #[test]
    fn client_id_is_preferred_over_sub() {
        assert_eq!(check(&mint(&claims(), KID)).unwrap().client, CLIENT);
    }

    #[test]
    fn sub_is_the_client_without_client_id() {
        let claims = without(claims(), "client_id");

        assert_eq!(check(&mint(&claims, KID)).unwrap().client, SUBJECT);
    }

    #[test]
    fn sub_is_the_client_when_client_id_is_empty() {
        let claims = with(claims(), "client_id", json!(""));

        assert_eq!(check(&mint(&claims, KID)).unwrap().client, SUBJECT);
    }

    #[test]
    fn error_text_carries_no_claim_value() {
        let token = mint(&claims(), KID);
        let kinds = [
            AuthError::Missing,
            AuthError::Malformed,
            AuthError::UnknownKey,
            AuthError::Invalid,
        ];
        for kind in kinds {
            let texts = [kind.to_string(), format!("{kind:?}")];
            for text in texts {
                for secret in [
                    ISSUER, AUDIENCE, KID, CLIENT, SUBJECT, JTI, "bridge:", &token,
                ] {
                    assert!(!text.contains(secret), "{text} contains {secret}");
                }
            }
        }
    }

    #[test]
    fn error_texts_name_the_kind() {
        assert_eq!(AuthError::Missing.to_string(), "missing");
        assert_eq!(AuthError::Malformed.to_string(), "malformed");
        assert_eq!(AuthError::UnknownKey.to_string(), "unknown key");
        assert_eq!(AuthError::Invalid.to_string(), "invalid");
    }

    #[test]
    fn key_set_keeps_only_rs256_signing_keys_with_a_kid() {
        let mut enc = KEY.jwk_json("enc");
        enc["use"] = json!("enc");
        let mut es256 = KEY.jwk_json("es256");
        es256["alg"] = json!("ES256");
        let mut no_kid = KEY.jwk_json("none");
        no_kid.as_object_mut().unwrap().remove("kid");
        let mut bare = KEY.jwk_json("bare");
        bare.as_object_mut().unwrap().remove("alg");
        bare.as_object_mut().unwrap().remove("use");
        let jwks: Vec<Jwk> = serde_json::from_value(json!([
            KEY.jwk_json(KID),
            enc,
            es256,
            no_kid,
            bare,
            {"kty": "oct", "kid": "hmac", "k": "c2VjcmV0"},
        ]))
        .unwrap();

        let keys = KeySet::from_jwks(&jwks);

        assert_eq!(keys.len(), 2);
        assert!(keys.get(KID).is_some());
        assert!(keys.get("bare").is_some());
        assert!(!keys.is_empty());
        assert!(KeySet::default().is_empty());
    }

    #[test]
    fn scope_table_maps_each_group() {
        let rows = [
            (Method::GET, "/v1/calendars", "calendar.read"),
            (Method::GET, "/v1/events", "calendar.read"),
            (Method::GET, "/v1/events/", "calendar.read"),
            (Method::GET, "/v1/events/{id}", "calendar.read"),
            (Method::GET, "/v1/free", "calendar.read"),
            (Method::POST, "/v1/events", "calendar.write"),
            (Method::PATCH, "/v1/events/{id}", "calendar.write"),
            (Method::DELETE, "/v1/events/{id}", "calendar.write"),
            (Method::PATCH, "/v1/events/", "calendar.write"),
            (Method::DELETE, "/v1/events/", "calendar.write"),
            (Method::GET, "/v1/lists", "reminders.read"),
            (Method::GET, "/v1/places", "reminders.read"),
            (Method::GET, "/v1/reminders", "reminders.read"),
            (Method::GET, "/v1/reminders/{id}", "reminders.read"),
            (Method::POST, "/v1/reminders", "reminders.write"),
            (Method::PATCH, "/v1/reminders/{id}", "reminders.write"),
            (Method::DELETE, "/v1/reminders/{id}", "reminders.write"),
            (Method::GET, "/v1/mail/accounts", "mail.read"),
            (Method::GET, "/v1/mail/messages", "mail.read"),
            (Method::GET, "/v1/mail/messages/{id}", "mail.read"),
            (Method::PATCH, "/v1/mail/messages/{id}", "mail.junk"),
        ];
        for (method, route, name) in rows {
            assert_eq!(scope_for(&method, route), Some(name), "{method} {route}");
        }
    }

    #[test]
    fn scope_table_has_no_row_for_health_or_unknown_pairs() {
        assert_eq!(scope_for(&Method::GET, "/healthz"), None);
        assert_eq!(scope_for(&Method::GET, "/v1/unknown"), None);
        assert_eq!(scope_for(&Method::PUT, "/v1/events/{id}"), None);
        assert_eq!(scope_for(&Method::DELETE, "/v1/mail/messages/{id}"), None);
    }

    #[test]
    fn required_scope_adds_the_prefix() {
        assert_eq!(
            validator().required_scope(&Method::PATCH, "/v1/mail/messages/{id}"),
            Some("bridge:mail.junk".to_owned())
        );
        assert_eq!(validator().required_scope(&Method::GET, "/healthz"), None);
    }

    #[test]
    fn required_scope_with_an_empty_prefix_is_the_name() {
        let config = AuthConfig {
            scope_prefix: String::new(),
            ..config()
        };

        assert_eq!(
            Validator::new(&config).required_scope(&Method::GET, "/v1/free"),
            Some("calendar.read".to_owned())
        );
    }
}
