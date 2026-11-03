use clap::Args;
use serde_json::json;

use crate::client::{BffClient, CliError};
use crate::config;

#[derive(Args)]
pub struct LoginArgs {
    /// Username for authentication
    #[arg(long)]
    pub username: String,

    /// Password for authentication
    #[arg(long)]
    pub password: String,
}

pub async fn execute(client: &BffClient, args: LoginArgs) -> Result<(), CliError> {
    let body = json!({
        "username": args.username,
        "password": args.password,
    });

    // API-only login: /v1/auth/login is the browser flow and no longer
    // returns a token (#660). chvctl is a non-browser client and uses the
    // control-plane token endpoint, which is merged onto the same admin
    // router under /api/v1.
    let resp = client.post("/api/v1/auth/login", &body).await?;

    let token = resp
        .get("token")
        .and_then(|t| t.as_str())
        .ok_or_else(|| CliError::Parse("no token in response".to_string()))?;

    config::save_credentials(token).map_err(CliError::Io)?;

    println!("Login successful. Token saved to ~/.config/chvctl/credentials");
    Ok(())
}
