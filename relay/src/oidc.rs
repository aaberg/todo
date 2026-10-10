//! OIDC client — talks to Authelia (or any OIDC provider).
//!
//! Uses `CoreClient` for discovery, authorization URL, and token exchange.
//! The `groups` claim is extracted by parsing the raw JWT payload, since
//! `CoreClient` hardcodes `EmptyAdditionalClaims` and custom claim types
//! require redefining the entire generic `Client<...>` stack.
//!
//! ID token signature, issuer, audience, expiry, and nonce are all validated
//! by the `openidconnect` crate. We only parse the payload for `groups`.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use openidconnect::{
    core::{CoreAuthenticationFlow, CoreProviderMetadata},
    reqwest, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet,
    EndpointNotSet, EndpointSet, IssuerUrl, Nonce, OAuth2TokenResponse, RedirectUrl, Scope,
    TokenResponse, TokenUrl,
};
use serde::Deserialize;

/// The fully-configured OIDC client type: auth URL and token URL are both set
/// (EndpointSet), userinfo is optional (EndpointMaybeSet), everything else is unset.
type ConfiguredClient = openidconnect::Client<
    openidconnect::EmptyAdditionalClaims,
    openidconnect::core::CoreAuthDisplay,
    openidconnect::core::CoreGenderClaim,
    openidconnect::core::CoreJweContentEncryptionAlgorithm,
    openidconnect::core::CoreJsonWebKey,
    openidconnect::core::CoreAuthPrompt,
    openidconnect::StandardErrorResponse<openidconnect::core::CoreErrorResponseType>,
    openidconnect::core::CoreTokenResponse,
    openidconnect::core::CoreTokenIntrospectionResponse,
    openidconnect::core::CoreRevocableToken,
    openidconnect::core::CoreRevocationErrorResponse,
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointSet,
    EndpointMaybeSet,
>;

/// Raw claims we extract from the ID token JWT payload (beyond what
/// `IdTokenClaims` provides). Only used as fallback — primary source is
/// the UserInfo endpoint, which is where modern Authelia puts `groups`.
#[derive(Debug, Deserialize)]
struct RawClaims {
    #[serde(default)]
    groups: Vec<String>,
    #[serde(default)]
    email: Option<String>,
}

/// Claims from the OIDC UserInfo endpoint response.
#[derive(Debug, Deserialize)]
struct UserInfoClaims {
    #[serde(default)]
    groups: Vec<String>,
    #[serde(default)]
    email: Option<String>,
}


/// The OIDC client, ready to use after discovery.
pub struct OidcClient {
    client: ConfiguredClient,
    http_client: reqwest::Client,
    userinfo_url: Option<String>,
    debug: bool,
}

/// Identity extracted from a validated ID token.
#[derive(Debug, Clone)]
pub struct UserIdentity {
    pub oidc_sub: String,
    pub email: Option<String>,
    pub groups: Vec<String>,
}

impl OidcClient {
    /// Discover provider metadata and build the client.
    pub async fn discover(
        issuer: &str,
        client_id: &str,
        client_secret: &str,
        redirect_uri: &str,
        debug: bool,
    ) -> Result<Self, OidcError> {
        let http_client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(OidcError::Http)?;

        let issuer_url = IssuerUrl::new(issuer.to_string()).map_err(OidcError::Url)?;

        let provider_metadata = CoreProviderMetadata::discover_async(issuer_url, &http_client)
            .await
            .map_err(|e| OidcError::Discovery(e))?;

        // Extract the UserInfo endpoint URL — modern Authelia only puts
        // groups/email there, not in the ID token.
        let userinfo_url = provider_metadata
            .userinfo_endpoint()
            .map(|u| u.url().to_string());


        // from_provider_metadata returns a client with HasTokenUrl = EndpointMaybeSet.
        // authorize_url and exchange_code both require EndpointSet.
        // The token endpoint is required by OIDC spec — if missing, the flow can't work.
        let token_url: TokenUrl = provider_metadata
            .token_endpoint()
            .cloned()
            .ok_or(OidcError::MissingTokenEndpoint)?;

        let client: ConfiguredClient = openidconnect::core::CoreClient::from_provider_metadata(
            provider_metadata,
            ClientId::new(client_id.to_string()),
            Some(ClientSecret::new(client_secret.to_string())),
        )
        .set_token_uri(token_url)
        .set_redirect_uri(RedirectUrl::new(redirect_uri.to_string()).map_err(OidcError::Url)?);

        Ok(Self {
            client,
            http_client,
            userinfo_url,
            debug,
        })
    }

    /// Build the authorization URL to redirect the user's browser to.
    /// Returns (url, csrf_token, nonce).
    pub fn authorize_url(&self) -> (url::Url, CsrfToken, Nonce) {
        self.client
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .add_scope(Scope::new("profile".to_string()))
            .add_scope(Scope::new("email".to_string()))
            .add_scope(Scope::new("groups".to_string()))
            .url()
    }

    /// Exchange an authorization code for tokens and validate the ID token.
    /// Returns the user's identity if the token is valid and the user is
    /// in one of the allowed groups.
    ///
    /// Groups and email are fetched from the UserInfo endpoint (primary),
    /// falling back to ID token claims if the endpoint is unavailable.
    pub async fn exchange_code(
        &self,
        code: &str,
        nonce: &Nonce,
        allowed_groups: &[String],
    ) -> Result<UserIdentity, OidcError> {
        let token_response = self
            .client
            .exchange_code(AuthorizationCode::new(code.to_string()))
            .request_async(&self.http_client)
            .await
            .map_err(OidcError::TokenExchange)?;

        let id_token = token_response
            .id_token()
            .ok_or(OidcError::MissingIdToken)?;

        // Validate signature, issuer, audience, expiry, nonce
        let claims = id_token
            .claims(&self.client.id_token_verifier(), nonce)
            .map_err(OidcError::Validation)?;

        let oidc_sub = claims.subject().to_string();

        // Debug: log full ID token payload (only when --debug is enabled)
        if self.debug {
            let raw = id_token.to_string();
            if let Some(payload_b64) = raw.split('.').nth(1) {
                if let Ok(payload_bytes) = URL_SAFE_NO_PAD.decode(payload_b64) {
                    if let Ok(payload_str) = String::from_utf8(payload_bytes) {
                        tracing::warn!(id_token_payload = %payload_str, "debug: ID token payload");
                    }
                }
            }
        }

        // Fetch groups/email from the UserInfo endpoint (primary source).
        // Modern Authelia puts these claims only at UserInfo, not in the ID token.
        let (groups, email) = match self.fetch_userinfo(token_response.access_token()).await {
            Ok(ui) => (ui.groups, ui.email),
            Err(e) => {
                // Fallback: parse from ID token JWT payload
                tracing::warn!(error = %e, "UserInfo endpoint failed, falling back to ID token claims");
                let raw_groups = extract_groups(id_token).unwrap_or_default();
                let raw_email = extract_email(id_token);
                (raw_groups, raw_email.or_else(|| claims.email().map(|e| e.to_string())))
            }
        };

        // Group check: user must be in at least one allowed group
        if !allowed_groups.is_empty() {
            let is_member = groups.iter().any(|g| allowed_groups.contains(g));
            if !is_member {
                return Err(OidcError::NotInAllowedGroup);
            }
        }

        Ok(UserIdentity {
            oidc_sub,
            email,
            groups,
        })
    }

    /// Call the OIDC UserInfo endpoint with the access token.
    /// Returns the parsed claims (groups, email).
    async fn fetch_userinfo(
        &self,
        access_token: &openidconnect::AccessToken,
    ) -> Result<UserInfoClaims, OidcError> {
        let url = self
            .userinfo_url
            .as_ref()
            .ok_or(OidcError::MissingUserInfoEndpoint)?;

        let response = self
            .http_client
            .get(url)
            .bearer_auth(access_token.secret())
            .send()
            .await
            .map_err(OidcError::Http)?;

        if !response.status().is_success() {
            return Err(OidcError::UserInfoFailed(response.status().as_u16()));
        }

        let body = response.text().await.map_err(OidcError::Http)?;

        // Debug: log full UserInfo response (only when --debug is enabled)
        if self.debug {
            tracing::warn!(userinfo_response = %body, "debug: UserInfo endpoint response");
        }

        serde_json::from_str(&body).map_err(OidcError::UserInfoParse)
    }
}

/// Extract the `groups` claim from a raw ID token JWT.
/// The token has already been signature-validated by the openidconnect crate;
/// this just parses the payload segment for the `groups` field.
fn extract_groups(id_token: &openidconnect::core::CoreIdToken) -> Option<Vec<String>> {
    let raw = id_token.to_string();
    let payload_b64 = raw.split('.').nth(1)?;
    let payload_bytes = URL_SAFE_NO_PAD.decode(payload_b64).ok()?;
    let payload: RawClaims = serde_json::from_slice(&payload_bytes).ok()?;
    Some(payload.groups)
}

/// Extract the `email` claim from a raw ID token JWT payload.
fn extract_email(id_token: &openidconnect::core::CoreIdToken) -> Option<String> {
    let raw = id_token.to_string();
    let payload_b64 = raw.split('.').nth(1)?;
    let payload_bytes = URL_SAFE_NO_PAD.decode(payload_b64).ok()?;
    let payload: RawClaims = serde_json::from_slice(&payload_bytes).ok()?;
    payload.email
}


#[derive(Debug)]
pub enum OidcError {
    Http(reqwest::Error),
    Url(openidconnect::url::ParseError),
    Discovery(openidconnect::DiscoveryError<openidconnect::HttpClientError<reqwest::Error>>),
    Configuration(openidconnect::ConfigurationError),
    TokenExchange(
        openidconnect::RequestTokenError<
            openidconnect::HttpClientError<reqwest::Error>,
            openidconnect::StandardErrorResponse<openidconnect::core::CoreErrorResponseType>,
        >,
    ),
    MissingIdToken,
    Validation(openidconnect::ClaimsVerificationError),
    MissingTokenEndpoint,
    MissingUserInfoEndpoint,
    UserInfoFailed(u16),
    UserInfoParse(serde_json::Error),
    NotInAllowedGroup,
}

impl std::fmt::Display for OidcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OidcError::Http(e) => write!(f, "HTTP error: {e}"),
            OidcError::Url(e) => write!(f, "URL parse error: {e}"),
            OidcError::Discovery(e) => write!(f, "OIDC discovery error: {e}"),
            OidcError::Configuration(e) => write!(f, "OIDC configuration error: {e}"),
            OidcError::TokenExchange(e) => write!(f, "token exchange error: {e}"),
            OidcError::MissingIdToken => write!(f, "no ID token in response"),
            OidcError::Validation(e) => write!(f, "ID token validation error: {e}"),
            OidcError::MissingTokenEndpoint => write!(f, "provider metadata has no token endpoint"),
            OidcError::NotInAllowedGroup => write!(f, "user is not in any allowed group"),
            OidcError::MissingUserInfoEndpoint => write!(f, "provider metadata has no userinfo endpoint"),
            OidcError::UserInfoFailed(status) => write!(f, "UserInfo endpoint returned HTTP {status}"),
            OidcError::UserInfoParse(e) => write!(f, "UserInfo response parse error: {e}"),
        }
    }
}

impl std::error::Error for OidcError {}
