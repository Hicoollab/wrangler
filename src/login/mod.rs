use oauth2::basic::BasicClient;
use oauth2::reqwest::http_client;

use oauth2::{
    AuthType, AuthUrl, AuthorizationCode, ClientId, CsrfToken, PkceCodeChallenge, RedirectUrl,
    Scope, TokenResponse, TokenUrl,
};

use std::collections::HashSet;
use std::env; // TODO: remove
use std::iter::FromIterator;

use hyper::server::conn::AddrStream;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server, StatusCode};

use anyhow::Result;
use futures::executor::block_on;
use tokio::sync::mpsc;

use crate::terminal::{interactive, open_browser};

use crate::commands::config::global_config;
use crate::settings::global_user::{GlobalUser, TokenType};

// List of allowed scopes for OAuth
static SCOPES_LIST: [&str; 8] = [
    "account:read",
    "user:read",
    "workers:write",
    "workers_kv:write",
    "workers_routes:write",
    "workers_scripts:write",
    "workers_tail:read",
    "zone:read",
];

// HTTP Server request handler
async fn handle_callback(req: Request<Body>, tx: mpsc::Sender<String>) -> Result<Response<Body>> {
    match req.uri().path() {
        // Endpoint given when registering oauth client
        "/oauth/callback" => {
            // Get authorization code from request
            let params = req
                .uri()
                .query()
                .map(|v| url::form_urlencoded::parse(v.as_bytes()))
                .unwrap();

            // Get authorization code and csrf state
            let mut params_values: Vec<String> = Vec::with_capacity(2);
            for (key, value) in params {
                if key == "code" || key == "state" {
                    params_values.push(value.to_string());
                }
            }

            if params_values.len() != 2 {
                // user denied consent
                let params_response = "denied".to_string();
                tx.send(params_response).await?;
                // TODO: placeholder, probably change to a specific denied consent page
                let response = Response::builder()
                    .status(StatusCode::PERMANENT_REDIRECT)
                    //.header("Location", "https://welcome.developers.workers.dev")
                    .header("Location", "http://127.0.0.1:8787/wrangler-oauth-consent-denied")
                    .body(Body::empty())
                    .unwrap();
                return Ok(response);
            }

            // Send authorization code back
            let params_values_str = format!("ok {} {}", params_values[0], params_values[1]);
            tx.send(params_values_str).await?;

            let response = Response::builder()
                .status(StatusCode::PERMANENT_REDIRECT)
                //.header("Location", "https://welcome.developers.workers.dev")
                .header("Location", "http://127.0.0.1:8787/wrangler-oauth-consent-granted")
                .body(Body::empty())
                .unwrap();

            Ok(response)
        }
        _ => {
            let params_response = "error".to_string();
            tx.send(params_response).await?;

            let response = Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap();

            Ok(response)
        }
    }
}

// Get results (i.e. authorization code and CSRF state) back from local HTTP server
async fn http_server_get_params() -> Result<String> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(1);

    // Create and start listening for authorization redirect on local HTTP server
    let server_fn_gen = |tx: mpsc::Sender<String>| {
        service_fn(move |req: Request<Body>| {
            let tx_clone = tx.clone();
            handle_callback(req, tx_clone)
        })
    };

    let service = make_service_fn(move |_socket: &AddrStream| {
        let tx_clone = tx.clone();
        async move { Ok::<_, hyper::Error>(server_fn_gen(tx_clone)) }
    });

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.spawn(async {
        let addr = ([127, 0, 0, 1], 8976).into();

        let server = Server::bind(&addr).serve(service);
        server.await.unwrap();
    });

    // Receive authorization code and csrf state from HTTP server
    let params = runtime.block_on(async { rx.recv().await.unwrap() });
    Ok(params)
}

pub fn run(scopes: Option<&[&str]>) -> Result<()> {
    // -------------------------
    // Temporary authentication
    // TODO: Remove when ready
    let env_key = "CLIENT_ID";
    let client_id = match env::var(env_key) {
        Ok(value) => value,
        Err(_) => panic!("client_id not provided"),
    };

    // -------------------------

    // Create oauth2 client
    let client = BasicClient::new(
        ClientId::new(client_id.to_string()),
        None,
        AuthUrl::new("https://dash.staging.cloudflare.com/oauth2/auth".to_string())
            .expect("Invalid authorization endpoint URL"),
        Some(
            TokenUrl::new("https://dash.staging.cloudflare.com/oauth2/token".to_string())
                .expect("Invalid token endpoint URL"),
        ),
    )
    .set_redirect_uri(
        RedirectUrl::new("http://localhost:8976/oauth/callback".to_string())
            .expect("Invalid redirect URL"),
    )
    .set_auth_type(AuthType::RequestBody);

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

    // Create URL for user with the necessary scopes
    let mut client_state = client
        .authorize_url(CsrfToken::new_random)
        .set_pkce_challenge(pkce_challenge);

    if scopes.is_none() {
        // User did not provide any scopes
        for scope in SCOPES_LIST {
            client_state = client_state.add_scope(Scope::new(scope.to_string()));
        }
    } else {
        // User did provide some scopes
        let valid_scopes: HashSet<&str> = HashSet::from_iter(SCOPES_LIST.iter().cloned());
        for scope in scopes.unwrap() {
            if valid_scopes.contains(scope) {
                client_state = client_state.add_scope(Scope::new(scope.to_string()));
            } else {
                anyhow::bail!("Invalid scope has been provided: {}", scope)
            }
        }
    }
    let (auth_url, csrf_state) = client_state.url();

    // Navigate to authorization endpoint
    let browser_permission =
        interactive::confirm("Allow Wrangler to open a page in your browser?")?;
    if !browser_permission {
        anyhow::bail!("In order to log in you must allow Wrangler to open your browser. If you don't want to do this consider using `wrangler config`");
    }
    open_browser(auth_url.as_str())?;

    // Get authorization code and CSRF state from local HTTP server
    let params_values = match block_on(http_server_get_params()) {
        Ok(params) => params,
        Err(_) => anyhow::bail!("Failed to receive authorization code from local HTTP server"),
    };
    let params_values_vec: Vec<&str> = params_values.split_whitespace().collect();
    if params_values_vec.is_empty() {
        anyhow::bail!("Failed to receive authorization code from local HTTP server")
    }

    // Check if user has given consent, or if an error has been encountered
    let response_status = params_values_vec[0];
    if response_status == "denied" {
        anyhow::bail!("Consent denied. You must grant consent to Wrangler in order to login. If you don't want to do this consider using `wrangler config`")
    } else if response_status == "err" {
        anyhow::bail!("Failed to receive authorization code from local HTTP server")
    }

    // Get authorization code and CSRF state
    if params_values_vec.len() != 3 {
        anyhow::bail!(
            "Failed to receive authorization code and/or csrf state from local HTTP server"
        )
    }
    let auth_code = params_values_vec[1];
    let recv_csrf_state = params_values_vec[2];

    // Check CSRF token to ensure redirect is legit
    let recv_csrf_state = CsrfToken::new(recv_csrf_state.to_string());
    if recv_csrf_state.secret() != csrf_state.secret() {
        anyhow::bail!(
            "Redirect URI CSRF state check failed. Received: {}, expected: {}",
            recv_csrf_state.secret(),
            csrf_state.secret()
        );
    }

    // Exchange authorization token for access token
    let token_response = client
        .exchange_code(AuthorizationCode::new(auth_code.to_string()))
        .set_pkce_verifier(pkce_verifier)
        .request(http_client)
        .expect("Failed to retrieve access token");

    // Configure user with new token
    let user = GlobalUser::TokenAuth {
        token_type: TokenType::Oauth,
        value: TokenResponse::access_token(&token_response)
            .secret()
            .to_string(),
    };
    global_config(&user, false)?;

    Ok(())
}
